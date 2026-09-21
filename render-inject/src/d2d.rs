//! Direct2D text: `DrawGlyphRun` on every kind of render target.
//!
//! Render targets are created at runtime, so reaching them is a chain of
//! creation hooks, the same chain upstream `directwrite.cpp` walks:
//!
//! ```text
//! d2d1!D2D1CreateFactory ─▶ ID2D1Factory ─┬─▶ CreateHwndRenderTarget / CreateDCRenderTarget /
//!                                         │   CreateWicBitmapRenderTarget       ─▶ render target
//!                                         └─▶ ID2D1Factory1..7::CreateDevice    ─▶ ID2D1Device
//! d2d1!D2D1CreateDevice ──────────────────────────────────────────────────────▶ ID2D1Device
//!     ID2D1Device..6::CreateDeviceContext ────────────────────────────────────▶ ID2D1DeviceContext
//! d2d1!D2D1CreateDeviceContext ───────────────────────────────────────────────▶ ID2D1DeviceContext
//! ```
//!
//! On each render target / device context we patch `DrawGlyphRun` (slot 29),
//! `ID2D1DeviceContext::DrawGlyphRun` (slot 82, the overload with a run
//! description), `SetTextAntialiasMode` (34) and `SetTextRenderingParams`
//! (36). Every vtable is patched once, tracked in `SLOT_ORIG` by
//! (vtable, slot). Slot numbers were checked against the `windows` crate's
//! `*_Vtbl` definitions.
//!
//! Two tiers of substitution. Where the target lends a GDI DC
//! (`ID2D1GdiInteropRenderTarget::GetDC`: HWND/DC render targets, GDI-compatible
//! bitmaps) the run is rasterised by render-core and blitted over — beyond
//! what upstream does. Where it does not (device contexts on DXGI surfaces:
//! swap chains, composition), we do what upstream does: hand Direct2D the
//! profile's `[DirectWrite]` rendering params + antialias mode, and nudge the
//! transform by 1/65535 when grid fitting is off so DirectWrite stops
//! snapping to the pixel grid.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use render_core::render::{draw_glyphs_onto, Canvas, Ink};
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Graphics::Direct2D::{
    ID2D1Brush, ID2D1Device, ID2D1Device1, ID2D1Device2, ID2D1Device3, ID2D1Device4, ID2D1Device5, ID2D1Device6,
    ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1, ID2D1Factory2, ID2D1Factory3, ID2D1Factory4, ID2D1Factory5,
    ID2D1Factory6, ID2D1Factory7, ID2D1GdiInteropRenderTarget, ID2D1RenderTarget, ID2D1SolidColorBrush,
    D2D1_DC_INITIALIZE_MODE_COPY, D2D1_TEXT_ANTIALIAS_MODE,
};
use windows::Win32::Graphics::DirectWrite::DWRITE_GLYPH_RUN;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::{s, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::hook::{install_hook, patch_slot};
use crate::state::{orig, RenderState, RENDER};
use crate::dwrite::{dw_rendering, dwrite_font_bytes};
use crate::log;

type FnD2DCreateFactory = unsafe extern "system" fn(i32, *const GUID, *const c_void, *mut *mut c_void) -> HRESULT;
/// `D2D1CreateDevice(IDXGIDevice*, const D2D1_CREATION_PROPERTIES*, ID2D1Device**)`
/// and `D2D1CreateDeviceContext(IDXGISurface*, ..., ID2D1DeviceContext**)`.
type FnD2DCreateDevice = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
/// `ID2D1Factory::CreateDCRenderTarget(props, out)`, and the same shape as
/// every `ID2D1FactoryN::CreateDevice(IDXGIDevice*, out)`.
type FnCreateDCRT = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
/// `CreateHwndRenderTarget(props, hwndProps, out)`; also `CreateWicBitmapRenderTarget(bitmap, props, out)`.
type FnCreateHwndRT = unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
/// `ID2D1DeviceN::CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS, out)`.
type FnCreateDeviceContext = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
type FnD2DDrawGlyphRun = unsafe extern "system" fn(*mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *mut c_void, i32);
/// `ID2D1DeviceContext::DrawGlyphRun(baseline, run, description, brush, measuring)`.
type FnD2DDrawGlyphRun1 = unsafe extern "system" fn(*mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *const c_void, *mut c_void, i32);
type FnSetTextAaMode = unsafe extern "system" fn(*mut c_void, i32);
type FnSetTextRenderingParams = unsafe extern "system" fn(*mut c_void, *mut c_void);

static ORIG_D2DCF: OnceLock<FnD2DCreateFactory> = OnceLock::new();
static ORIG_D2DCD: OnceLock<FnD2DCreateDevice> = OnceLock::new();
static ORIG_D2DCDC: OnceLock<FnD2DCreateDevice> = OnceLock::new();
/// (vtable, slot) → original function, for every COM slot patched here. One
/// mutex both serialises the one-time patches and guards the map, so two
/// threads creating render targets at once cannot both capture a slot that
/// already holds our detour (the detour would then call itself).
static SLOT_ORIG: Mutex<Option<HashMap<(usize, usize), usize>>> = Mutex::new(None);
static D2D_CAPTURED: AtomicBool = AtomicBool::new(false); // log the first D2D substitution once

// ID2D1Factory
const SLOT_CREATE_WIC_RT: usize = 13;
const SLOT_CREATE_HWND_RT: usize = 14;
const SLOT_CREATE_DC_RT: usize = 16;
/// `CreateDevice` in ID2D1Factory1 … ID2D1Factory7, in that order.
const SLOTS_FACTORY_CREATE_DEVICE: [usize; 7] = [17, 27, 28, 29, 30, 31, 32];
/// `CreateDeviceContext` in ID2D1Device … ID2D1Device6, in that order.
const SLOTS_DEVICE_CREATE_CONTEXT: [usize; 7] = [4, 11, 12, 15, 16, 19, 20];
// ID2D1RenderTarget / ID2D1DeviceContext
const SLOT_DRAW_GLYPH_RUN: usize = 29;
const SLOT_SET_TEXT_AA_MODE: usize = 34;
const SLOT_SET_TEXT_RENDERING_PARAMS: usize = 36;
const SLOT_DRAW_GLYPH_RUN1: usize = 82;

/// Patch `slot` of `obj`'s vtable to `detour` unless that (vtable, slot) is
/// already ours. Returns whether a patch was made.
unsafe fn patch_once(obj: *mut c_void, slot: usize, detour: *const ()) -> bool {
    let vtbl = *(obj as *mut *mut usize);
    let key = (vtbl as usize, slot);
    let Ok(mut m) = SLOT_ORIG.lock() else { return false };
    let map = m.get_or_insert_with(HashMap::new);
    if map.contains_key(&key) { return false; }
    patch_slot(vtbl.add(slot), detour as usize, |old| { map.insert(key, old); })
}

/// The original function behind `slot` for `this`'s vtable, if we patched it.
unsafe fn slot_orig(this: *mut c_void, slot: usize) -> Option<usize> {
    let vtbl = *(this as *const usize);
    SLOT_ORIG.lock().ok()?.as_ref()?.get(&(vtbl, slot)).copied()
}

// ---- creation chain ----

unsafe extern "system" fn d2dcf_detour(ftype: i32, riid: *const GUID, opts: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let hr = (orig(&ORIG_D2DCF))(ftype, riid, opts, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { hook_factory(*out); }
    hr
}

unsafe extern "system" fn d2dcd_detour(dxgi: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let hr = (orig(&ORIG_D2DCD))(dxgi, props, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { hook_device(*out); }
    hr
}

unsafe extern "system" fn d2dcdc_detour(surface: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let hr = (orig(&ORIG_D2DCDC))(surface, props, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { hook_render_target(*out); }
    hr
}

/// Patch the factory's render-target and device creation slots. Which
/// `CreateDevice` overloads exist depends on the OS's Direct2D version, so
/// each is patched only when the factory answers the matching QueryInterface.
unsafe fn hook_factory(factory: *mut c_void) {
    let Some(f) = ID2D1Factory::from_raw_borrowed(&factory) else { return };
    let mut n = 0;
    n += patch_once(factory, SLOT_CREATE_WIC_RT, create_wic_detour as *const ()) as u32;
    n += patch_once(factory, SLOT_CREATE_HWND_RT, create_hwnd_detour as *const ()) as u32;
    n += patch_once(factory, SLOT_CREATE_DC_RT, create_dc_detour as *const ()) as u32;
    let supported = [
        f.cast::<ID2D1Factory1>().is_ok(), f.cast::<ID2D1Factory2>().is_ok(), f.cast::<ID2D1Factory3>().is_ok(),
        f.cast::<ID2D1Factory4>().is_ok(), f.cast::<ID2D1Factory5>().is_ok(), f.cast::<ID2D1Factory6>().is_ok(),
        f.cast::<ID2D1Factory7>().is_ok(),
    ];
    for (i, slot) in SLOTS_FACTORY_CREATE_DEVICE.iter().enumerate() {
        if supported[i] { n += patch_once(factory, *slot, create_device_detour as *const ()) as u32; }
    }
    if n > 0 { log(&format!("hook installed on D2D1Factory creation ({n} slots)")); }
}

/// Patch every `CreateDeviceContext` overload the device supports.
unsafe fn hook_device(dev: *mut c_void) {
    let Some(d) = ID2D1Device::from_raw_borrowed(&dev) else { return };
    let supported = [
        true, d.cast::<ID2D1Device1>().is_ok(), d.cast::<ID2D1Device2>().is_ok(), d.cast::<ID2D1Device3>().is_ok(),
        d.cast::<ID2D1Device4>().is_ok(), d.cast::<ID2D1Device5>().is_ok(), d.cast::<ID2D1Device6>().is_ok(),
    ];
    let mut n = 0;
    for (i, slot) in SLOTS_DEVICE_CREATE_CONTEXT.iter().enumerate() {
        if supported[i] { n += patch_once(dev, *slot, create_context_detour as *const ()) as u32; }
    }
    if n > 0 { log(&format!("hook installed on ID2D1Device::CreateDeviceContext ({n} slots)")); }
}

/// Patch the text slots of a render target or device context, then apply the
/// profile's rendering params to it (as upstream does at creation).
unsafe fn hook_render_target(rt: *mut c_void) {
    let mut n = 0;
    n += patch_once(rt, SLOT_DRAW_GLYPH_RUN, d2d_dgr_detour as *const ()) as u32;
    n += patch_once(rt, SLOT_SET_TEXT_AA_MODE, set_text_aa_detour as *const ()) as u32;
    n += patch_once(rt, SLOT_SET_TEXT_RENDERING_PARAMS, set_text_rp_detour as *const ()) as u32;
    // Every modern render target is a device context underneath; its
    // ID2D1DeviceContext vtable may be a separate (thunk) table, so patch the
    // description overload through that interface pointer.
    if let Some(r) = ID2D1RenderTarget::from_raw_borrowed(&rt) {
        if let Ok(dc) = r.cast::<ID2D1DeviceContext>() {
            n += patch_once(dc.as_raw(), SLOT_DRAW_GLYPH_RUN1, d2d_dgr1_detour as *const ()) as u32;
        }
        if let Some(dw) = dw_rendering() {
            r.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE(dw.aa_mode));
            r.SetTextRenderingParams(&dw.params);
        }
    }
    if n > 0 { log(&format!("hook installed on D2D render target text ({n} slots)")); }
}

unsafe extern "system" fn create_dc_detour(this: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let Some(o) = slot_orig(this, SLOT_CREATE_DC_RT) else { return HRESULT(-1) };
    let f: FnCreateDCRT = std::mem::transmute(o);
    let hr = f(this, props, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { hook_render_target(*out); }
    hr
}
unsafe extern "system" fn create_hwnd_detour(this: *mut c_void, p1: *const c_void, p2: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let Some(o) = slot_orig(this, SLOT_CREATE_HWND_RT) else { return HRESULT(-1) };
    let f: FnCreateHwndRT = std::mem::transmute(o);
    let hr = f(this, p1, p2, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { hook_render_target(*out); }
    hr
}
unsafe extern "system" fn create_wic_detour(this: *mut c_void, bitmap: *const c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let Some(o) = slot_orig(this, SLOT_CREATE_WIC_RT) else { return HRESULT(-1) };
    let f: FnCreateHwndRT = std::mem::transmute(o);
    let hr = f(this, bitmap, props, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { hook_render_target(*out); }
    hr
}
/// One detour serves every `ID2D1FactoryN::CreateDevice`: the signatures are
/// identical. We cannot tell which slot the app called, so the original of
/// any patched slot on this vtable is used — they all create the same device
/// (each overload only differs in the interface it returns).
unsafe extern "system" fn create_device_detour(this: *mut c_void, dxgi: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let Some(o) = SLOTS_FACTORY_CREATE_DEVICE.iter().find_map(|s| slot_orig(this, *s)) else { return HRESULT(-1) };
    let f: FnCreateDCRT = std::mem::transmute(o);
    let hr = f(this, dxgi, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { hook_device(*out); }
    hr
}
/// Same for `ID2D1DeviceN::CreateDeviceContext`.
unsafe extern "system" fn create_context_detour(this: *mut c_void, options: u32, out: *mut *mut c_void) -> HRESULT {
    let Some(o) = SLOTS_DEVICE_CREATE_CONTEXT.iter().find_map(|s| slot_orig(this, *s)) else { return HRESULT(-1) };
    let f: FnCreateDeviceContext = std::mem::transmute(o);
    let hr = f(this, options, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { hook_render_target(*out); }
    hr
}

// ---- text slots ----

/// The app sets its own antialias mode / rendering params: keep ours instead
/// (upstream does the same), unless we have none.
unsafe extern "system" fn set_text_aa_detour(this: *mut c_void, mode: i32) {
    let Some(o) = slot_orig(this, SLOT_SET_TEXT_AA_MODE) else { return };
    let f: FnSetTextAaMode = std::mem::transmute(o);
    f(this, dw_rendering().map_or(mode, |d| d.aa_mode));
}
unsafe extern "system" fn set_text_rp_detour(this: *mut c_void, params: *mut c_void) {
    let Some(o) = slot_orig(this, SLOT_SET_TEXT_RENDERING_PARAMS) else { return };
    let f: FnSetTextRenderingParams = std::mem::transmute(o);
    f(this, dw_rendering().map_or(params, |d| d.params.as_raw()));
}

/// Run the original draw with the grid-fit nudge upstream applies: with
/// grid fitting off, a 1/65535 skew keeps DirectWrite from snapping glyphs
/// to whole pixels.
unsafe fn with_grid_fit_nudge(this: *mut c_void, draw: impl FnOnce()) {
    let nudge = dw_rendering().is_some_and(|d| d.grid_fit_disabled);
    let rt = if nudge { ID2D1RenderTarget::from_raw_borrowed(&this) } else { None };
    let Some(rt) = rt else { draw(); return };
    let mut prev = Matrix3x2::default();
    rt.GetTransform(&mut prev);
    let mut skew = prev;
    skew.M12 += 1.0 / 65535.0;
    skew.M21 += 1.0 / 65535.0;
    rt.SetTransform(&skew);
    draw();
    rt.SetTransform(&prev);
}

unsafe extern "system" fn d2d_dgr_detour(this: *mut c_void, baseline: Vector2, run: *const DWRITE_GLYPH_RUN, brush: *mut c_void, measuring: i32) {
    if !run.is_null() && d2d_substitute(this, baseline, &*run, brush).is_some() {
        return;
    }
    if let Some(o) = slot_orig(this, SLOT_DRAW_GLYPH_RUN) {
        let f: FnD2DDrawGlyphRun = std::mem::transmute(o);
        with_grid_fit_nudge(this, || f(this, baseline, run, brush, measuring));
    }
}

unsafe extern "system" fn d2d_dgr1_detour(this: *mut c_void, baseline: Vector2, run: *const DWRITE_GLYPH_RUN, desc: *const c_void, brush: *mut c_void, measuring: i32) {
    if !run.is_null() && d2d_substitute(this, baseline, &*run, brush).is_some() {
        return;
    }
    if let Some(o) = slot_orig(this, SLOT_DRAW_GLYPH_RUN1) {
        let f: FnD2DDrawGlyphRun1 = std::mem::transmute(o);
        with_grid_fit_nudge(this, || f(this, baseline, run, desc, brush, measuring));
    }
}

/// Read the run's ink color from the D2D brush. D2D DrawGlyphRun paints with
/// the given brush, so mirroring the GDI/DWrite paths means honoring it — a
/// solid-color brush yields its RGB; anything else falls back to black. Colors
/// are premultiplied-free sRGB floats in 0..1.
unsafe fn d2d_brush_ink(brush: *mut c_void) -> Ink {
    if brush.is_null() { return Ink::default(); }
    let Some(b) = ID2D1Brush::from_raw_borrowed(&brush) else { return Ink::default() };
    let Ok(scb) = b.cast::<ID2D1SolidColorBrush>() else { return Ink::default() };
    let c = scb.GetColor();
    let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    Ink { fg: [to8(c.r), to8(c.g), to8(c.b)] }
}

unsafe fn d2d_substitute(this: *mut c_void, baseline: Vector2, r: &DWRITE_GLYPH_RUN, brush: *mut c_void) -> Option<()> {
    let mut guard = RENDER.lock().ok()?;
    let RenderState { ft, tables, profile, font_key } = guard.as_mut()?;
    let (bytes, index) = dwrite_font_bytes(r)?;
    let key = format!("d2d:{index}:{}", bytes.len());
    if font_key.as_deref() != Some(key.as_str()) {
        ft.reface_memory_index(&bytes, index as i64).ok()?;
        *font_key = Some(key);
    }
    let px = r.fontEmSize.round() as i32;
    let glyphs = std::slice::from_raw_parts(r.glyphIndices, r.glyphCount as usize);
    let adv: f32 = if r.glyphAdvances.is_null() { 0.0 }
        else { std::slice::from_raw_parts(r.glyphAdvances, r.glyphCount as usize).iter().sum() };

    let rt = ID2D1RenderTarget::from_raw_borrowed(&this)?;
    let gi: ID2D1GdiInteropRenderTarget = rt.cast().ok()?;
    let hdc = gi.GetDC(D2D1_DC_INITIALIZE_MODE_COPY).ok()?;

    // region around the text baseline
    let bx = baseline.X.round() as i32;
    let by = baseline.Y.round() as i32;
    let rw = (adv.ceil() as i32 + px).clamp(1, 8192);
    let rh = (px * 2).clamp(1, 8192);
    let rx = bx;
    let ry = by - px - px / 4;

    let memdc = CreateCompatibleDC(Some(hdc));
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: rw, biHeight: -rh, biPlanes: 1, biBitCount: 32, biCompression: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let old = SelectObject(memdc, hbmp.into());
    let _ = BitBlt(memdc, 0, 0, rw, rh, Some(hdc), rx, ry, SRCCOPY);
    let dib = std::slice::from_raw_parts_mut(bits as *mut u8, (rw * rh * 4) as usize);
    let mut canvas = Canvas::from_bgra_topdown(rw as usize, rh as usize, dib);
    let ink = d2d_brush_ink(brush);
    draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px, (bx - rx, by - ry), None);
    canvas.blit_to_bgra_topdown(dib);
    let _ = BitBlt(hdc, rx, ry, rw, rh, Some(memdc), 0, 0, SRCCOPY);
    SelectObject(memdc, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(memdc);
    let _ = gi.ReleaseDC(None);
    if !D2D_CAPTURED.swap(true, Ordering::SeqCst) {
        log(&format!("substituted D2D DrawGlyphRun via render-core ({} glyphs, {px}px)", glyphs.len()));
    }
    Some(())
}

/// Hook the three d2d1 exports that start the creation chains above.
pub(crate) unsafe fn setup_d2d_hook() {
    let d2d1 = match GetModuleHandleW(w!("d2d1.dll")) {
        Ok(h) if !h.is_invalid() => h,
        _ => match windows::Win32::System::LibraryLoader::LoadLibraryW(w!("d2d1.dll")) {
            Ok(h) => h.into(),
            Err(_) => { log("d2d1.dll not available"); return; }
        },
    };
    if let Some(target) = GetProcAddress(d2d1, s!("D2D1CreateFactory")) {
        if install_hook(target as *const (), d2dcf_detour as *const (), |tramp| {
            let _ = ORIG_D2DCF.set(std::mem::transmute::<*const (), FnD2DCreateFactory>(tramp));
        }) { log("hook installed on D2D1CreateFactory"); } else { log("D2D1CreateFactory hook failed"); }
    }
    if let Some(target) = GetProcAddress(d2d1, s!("D2D1CreateDevice")) {
        if install_hook(target as *const (), d2dcd_detour as *const (), |tramp| {
            let _ = ORIG_D2DCD.set(std::mem::transmute::<*const (), FnD2DCreateDevice>(tramp));
        }) { log("hook installed on D2D1CreateDevice"); } else { log("D2D1CreateDevice hook failed"); }
    }
    if let Some(target) = GetProcAddress(d2d1, s!("D2D1CreateDeviceContext")) {
        if install_hook(target as *const (), d2dcdc_detour as *const (), |tramp| {
            let _ = ORIG_D2DCDC.set(std::mem::transmute::<*const (), FnD2DCreateDevice>(tramp));
        }) { log("hook installed on D2D1CreateDeviceContext"); } else { log("D2D1CreateDeviceContext hook failed"); }
    }
}
