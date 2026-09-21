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
//!     any target's CreateCompatibleRenderTarget ──────────────────────────────▶ bitmap render target
//! ```
//!
//! On each render target / device context we patch `DrawGlyphRun` (slot 29),
//! `ID2D1DeviceContext::DrawGlyphRun` (slot 82, the overload with a run
//! description), `SetTextAntialiasMode` (34), `SetTextRenderingParams` (36)
//! and `CreateCompatibleRenderTarget` (12). Every vtable is patched once,
//! tracked in `SLOT_ORIG` by (vtable, slot). Slot numbers were checked
//! against the `windows` crate's `*_Vtbl` definitions.
//!
//! Two tiers of substitution. Where the target lends a GDI DC
//! (`ID2D1GdiInteropRenderTarget::GetDC`: HWND/DC render targets, GDI-compatible
//! bitmaps) the run is rasterised by render-core and blitted over — beyond
//! what upstream does. Where it does not (device contexts on DXGI surfaces:
//! swap chains, composition), we do what upstream does: hand Direct2D the
//! profile's `[DirectWrite]` rendering params + antialias mode, and nudge the
//! transform by 1/65535 when grid fitting is off so DirectWrite stops
//! snapping to the pixel grid.
//!
//! Every detour is a thin `unsafe extern "system"` shell: it borrows what it
//! was handed (a COM object, a glyph run) once and calls safe code.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use render_core::render::{draw_glyphs_onto, Ink};
use windows::core::{s, w, Interface, GUID, HRESULT, PCSTR};
use windows::Win32::Graphics::Direct2D::{
    ID2D1Brush, ID2D1Device, ID2D1Device1, ID2D1Device2, ID2D1Device3, ID2D1Device4, ID2D1Device5, ID2D1Device6,
    ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1, ID2D1Factory2, ID2D1Factory3, ID2D1Factory4, ID2D1Factory5,
    ID2D1Factory6, ID2D1Factory7, ID2D1GdiInteropRenderTarget, ID2D1RenderTarget, ID2D1SolidColorBrush,
    D2D1_DC_INITIALIZE_MODE_COPY, D2D1_TEXT_ANTIALIAS_MODE,
};
use windows::Win32::Graphics::DirectWrite::DWRITE_GLYPH_RUN;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_numerics::{Matrix3x2, Vector2};

use crate::dib::Dib;
use crate::dwrite::{dw_rendering, GlyphRun};
use crate::hook::{install_hook, patch_slot};
use crate::log;
use crate::state::{orig, round_i32, RenderState, RENDER};

// ---- signatures ----

type FnD2DCreateFactory = unsafe extern "system" fn(i32, *const GUID, *const c_void, *mut *mut c_void) -> HRESULT;
/// `D2D1CreateDevice(IDXGIDevice*, const D2D1_CREATION_PROPERTIES*, ID2D1Device**)`
/// and `D2D1CreateDeviceContext(IDXGISurface*, ..., ID2D1DeviceContext**)`.
type FnD2DCreateDevice = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
/// `ID2D1Factory::CreateDCRenderTarget(props, out)`, and the same shape as
/// every `ID2D1FactoryN::CreateDevice(IDXGIDevice*, out)`.
type FnCreate2 = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
/// `CreateHwndRenderTarget(props, hwndProps, out)`; also `CreateWicBitmapRenderTarget(bitmap, props, out)`.
type FnCreate3 = unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
/// `ID2D1DeviceN::CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS, out)`.
type FnCreateDeviceContext = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
/// `CreateCompatibleRenderTarget(size*, pixelSize*, format*, options, out)`.
type FnCreateCompatibleRT =
    unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *const c_void, u32, *mut *mut c_void) -> HRESULT;
type FnDrawGlyphRun = unsafe extern "system" fn(*mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *mut c_void, i32);
/// `ID2D1DeviceContext::DrawGlyphRun(baseline, run, description, brush, measuring)`.
type FnDrawGlyphRun1 = unsafe extern "system" fn(*mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *const c_void, *mut c_void, i32);
type FnSetTextAaMode = unsafe extern "system" fn(*mut c_void, i32);
type FnSetTextRenderingParams = unsafe extern "system" fn(*mut c_void, *mut c_void);

static ORIG_D2DCF: OnceLock<FnD2DCreateFactory> = OnceLock::new();
static ORIG_D2DCD: OnceLock<FnD2DCreateDevice> = OnceLock::new();
static ORIG_D2DCDC: OnceLock<FnD2DCreateDevice> = OnceLock::new();
/// (vtable address, slot) → original function, for every COM slot patched
/// here. One mutex both serialises the one-time patches and guards the map,
/// so two threads creating render targets at once cannot both capture a slot
/// that already holds our detour (the detour would then call itself).
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
const SLOT_CREATE_COMPATIBLE_RT: usize = 12;
const SLOT_DRAW_GLYPH_RUN: usize = 29;
const SLOT_SET_TEXT_AA_MODE: usize = 34;
const SLOT_SET_TEXT_RENDERING_PARAMS: usize = 36;
const SLOT_DRAW_GLYPH_RUN1: usize = 82;

// ---- vtable bookkeeping ----

/// A live COM object's vtable, as the pointer we patch through.
///
/// # Safety
/// `obj` must be a live COM interface pointer.
unsafe fn vtable_of(obj: *mut c_void) -> *mut *const () {
    // SAFETY: the first word of any COM object is its vtable pointer.
    unsafe { *obj.cast::<*mut *const ()>() }
}

/// Patch `slot` of `obj`'s vtable to `detour` unless that (vtable, slot) is
/// already ours. Returns whether a patch was made.
///
/// # Safety
/// `obj` must be a live COM object whose interface has at least `slot + 1`
/// methods, and `detour` must match that method's ABI and signature.
unsafe fn patch_once(obj: *mut c_void, slot: usize, detour: *const ()) -> bool {
    // SAFETY: per the contract above.
    let vtbl = unsafe { vtable_of(obj) };
    let key = (vtbl.addr(), slot);
    let Ok(mut m) = SLOT_ORIG.lock() else { return false };
    let map = m.get_or_insert_with(HashMap::new);
    if map.contains_key(&key) {
        return false;
    }
    // SAFETY: `slot` is within the interface per the contract above.
    unsafe {
        patch_slot(vtbl.add(slot), detour, |old| {
            map.insert(key, old.expose_provenance());
        })
    }
}

/// The original function behind `slot` for `this`'s vtable, if we patched
/// it, typed as `F`.
///
/// # Safety
/// `this` must be a live COM object and `F` the exact fn-pointer type of
/// that slot's method.
unsafe fn slot_orig<F: Copy>(this: *mut c_void, slot: usize) -> Option<F> {
    const {
        assert!(core::mem::size_of::<F>() == core::mem::size_of::<*const ()>(), "F must be a fn pointer");
    }
    // SAFETY: `this` is live per the contract above.
    let vtbl = unsafe { vtable_of(this) }.addr();
    let addr = SLOT_ORIG.lock().ok()?.as_ref()?.get(&(vtbl, slot)).copied()?;
    let p = core::ptr::with_exposed_provenance::<()>(addr);
    // SAFETY: `F` is a fn pointer type of the same size, and the address is
    // the one `patch_slot` read out of the slot.
    Some(unsafe { core::mem::transmute_copy::<*const (), F>(&p) })
}

/// The original of any of `slots` on `this`'s vtable — for method families
/// (`CreateDevice`, `CreateDeviceContext`) whose overloads share one
/// signature and one implementation per object.
///
/// # Safety
/// As `slot_orig`.
unsafe fn any_slot_orig<F: Copy>(this: *mut c_void, slots: &[usize]) -> Option<F> {
    // SAFETY: forwarded.
    slots.iter().find_map(|&s| unsafe { slot_orig::<F>(this, s) })
}

/// The object a creation call wrote to `out`, if it succeeded.
///
/// # Safety
/// `out` is the call's out-parameter, valid for the call's duration.
unsafe fn created(hr: HRESULT, out: *mut *mut c_void) -> Option<*mut c_void> {
    if !hr.is_ok() {
        return None;
    }
    // SAFETY: per the contract above.
    unsafe { out.as_ref() }.copied().filter(|p| !p.is_null())
}

// ---- creation chain ----

unsafe extern "system" fn d2dcf_detour(ftype: i32, riid: *const GUID, opts: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: arguments forwarded untouched; `out` inspected under the call's contract.
    unsafe {
        let hr = (orig(&ORIG_D2DCF))(ftype, riid, opts, out);
        if let Some(f) = created(hr, out) {
            hook_factory(f);
        }
        hr
    }
}

unsafe extern "system" fn d2dcd_detour(dxgi: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `d2dcf_detour`.
    unsafe {
        let hr = (orig(&ORIG_D2DCD))(dxgi, props, out);
        if let Some(d) = created(hr, out) {
            hook_device(d);
        }
        hr
    }
}

unsafe extern "system" fn d2dcdc_detour(surface: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `d2dcf_detour`.
    unsafe {
        let hr = (orig(&ORIG_D2DCDC))(surface, props, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

/// Patch the factory's render-target and device creation slots. Which
/// `CreateDevice` overloads exist depends on the OS's Direct2D version, so
/// each is patched only when the factory answers the matching QueryInterface.
///
/// # Safety
/// `factory` must be a live `ID2D1Factory`.
unsafe fn hook_factory(factory: *mut c_void) {
    // SAFETY: per the contract above; `from_raw_borrowed` does not AddRef.
    let Some(f) = (unsafe { ID2D1Factory::from_raw_borrowed(&factory) }) else { return };
    let supported = [
        f.cast::<ID2D1Factory1>().is_ok(),
        f.cast::<ID2D1Factory2>().is_ok(),
        f.cast::<ID2D1Factory3>().is_ok(),
        f.cast::<ID2D1Factory4>().is_ok(),
        f.cast::<ID2D1Factory5>().is_ok(),
        f.cast::<ID2D1Factory6>().is_ok(),
        f.cast::<ID2D1Factory7>().is_ok(),
    ];
    // SAFETY: each slot exists on the interface the QueryInterface confirmed,
    // and each detour matches that slot's signature.
    let n = unsafe {
        u32::from(patch_once(factory, SLOT_CREATE_WIC_RT, create_wic_detour as *const ()))
            + u32::from(patch_once(factory, SLOT_CREATE_HWND_RT, create_hwnd_detour as *const ()))
            + u32::from(patch_once(factory, SLOT_CREATE_DC_RT, create_dc_detour as *const ()))
            + SLOTS_FACTORY_CREATE_DEVICE
                .iter()
                .zip(supported)
                .filter(|(_, ok)| *ok)
                .map(|(&slot, _)| u32::from(patch_once(factory, slot, create_device_detour as *const ())))
                .sum::<u32>()
    };
    if n > 0 {
        log(&format!("hook installed on D2D1Factory creation ({n} slots)"));
    }
}

/// Patch every `CreateDeviceContext` overload the device supports.
///
/// # Safety
/// `dev` must be a live `ID2D1Device`.
unsafe fn hook_device(dev: *mut c_void) {
    // SAFETY: per the contract above.
    let Some(d) = (unsafe { ID2D1Device::from_raw_borrowed(&dev) }) else { return };
    let supported = [
        true,
        d.cast::<ID2D1Device1>().is_ok(),
        d.cast::<ID2D1Device2>().is_ok(),
        d.cast::<ID2D1Device3>().is_ok(),
        d.cast::<ID2D1Device4>().is_ok(),
        d.cast::<ID2D1Device5>().is_ok(),
        d.cast::<ID2D1Device6>().is_ok(),
    ];
    // SAFETY: as in `hook_factory`.
    let n = unsafe {
        SLOTS_DEVICE_CREATE_CONTEXT
            .iter()
            .zip(supported)
            .filter(|(_, ok)| *ok)
            .map(|(&slot, _)| u32::from(patch_once(dev, slot, create_context_detour as *const ())))
            .sum::<u32>()
    };
    if n > 0 {
        log(&format!("hook installed on ID2D1Device::CreateDeviceContext ({n} slots)"));
    }
}

/// Patch the text slots of a render target or device context, then apply the
/// profile's rendering params to it (as upstream does at creation).
///
/// # Safety
/// `rt` must be a live `ID2D1RenderTarget` (or any interface deriving from it).
unsafe fn hook_render_target(rt: *mut c_void) {
    // SAFETY: per the contract above; every slot is on ID2D1RenderTarget
    // itself, and each detour matches its signature.
    let mut n = unsafe {
        u32::from(patch_once(rt, SLOT_CREATE_COMPATIBLE_RT, create_compatible_detour as *const ()))
            + u32::from(patch_once(rt, SLOT_DRAW_GLYPH_RUN, dgr_detour as *const ()))
            + u32::from(patch_once(rt, SLOT_SET_TEXT_AA_MODE, set_text_aa_detour as *const ()))
            + u32::from(patch_once(rt, SLOT_SET_TEXT_RENDERING_PARAMS, set_text_rp_detour as *const ()))
    };
    // SAFETY: per the contract above.
    if let Some(r) = unsafe { ID2D1RenderTarget::from_raw_borrowed(&rt) } {
        // Every modern render target is a device context underneath; its
        // ID2D1DeviceContext vtable may be a separate (thunk) table, so patch
        // the description overload through that interface pointer.
        if let Ok(dc) = r.cast::<ID2D1DeviceContext>() {
            // SAFETY: slot 82 is on ID2D1DeviceContext, which `cast` confirmed.
            n += u32::from(unsafe { patch_once(dc.as_raw(), SLOT_DRAW_GLYPH_RUN1, dgr1_detour as *const ()) });
        }
        if let Some(dw) = dw_rendering() {
            // SAFETY: setters on a live render target.
            unsafe {
                r.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE(dw.aa_mode));
                r.SetTextRenderingParams(&dw.params);
            }
        }
    }
    if n > 0 {
        log(&format!("hook installed on D2D render target text ({n} slots)"));
    }
}

unsafe extern "system" fn create_dc_detour(this: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: `this` is the factory the app called; arguments forwarded.
    unsafe {
        let Some(f) = slot_orig::<FnCreate2>(this, SLOT_CREATE_DC_RT) else { return HRESULT(-1) };
        let hr = f(this, props, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

unsafe extern "system" fn create_hwnd_detour(this: *mut c_void, p1: *const c_void, p2: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = slot_orig::<FnCreate3>(this, SLOT_CREATE_HWND_RT) else { return HRESULT(-1) };
        let hr = f(this, p1, p2, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

unsafe extern "system" fn create_wic_detour(this: *mut c_void, bitmap: *const c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = slot_orig::<FnCreate3>(this, SLOT_CREATE_WIC_RT) else { return HRESULT(-1) };
        let hr = f(this, bitmap, props, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

unsafe extern "system" fn create_compatible_detour(
    this: *mut c_void, size: *const c_void, pixel_size: *const c_void, format: *const c_void, options: u32, out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = slot_orig::<FnCreateCompatibleRT>(this, SLOT_CREATE_COMPATIBLE_RT) else { return HRESULT(-1) };
        let hr = f(this, size, pixel_size, format, options, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

/// One detour serves every `ID2D1FactoryN::CreateDevice`: the signatures are
/// identical. We cannot tell which slot the app called, so the original of
/// any patched slot on this vtable is used — they all create the same device
/// (each overload only differs in the interface it returns).
unsafe extern "system" fn create_device_detour(this: *mut c_void, dxgi: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = any_slot_orig::<FnCreate2>(this, &SLOTS_FACTORY_CREATE_DEVICE) else { return HRESULT(-1) };
        let hr = f(this, dxgi, out);
        if let Some(d) = created(hr, out) {
            hook_device(d);
        }
        hr
    }
}

/// Same for `ID2D1DeviceN::CreateDeviceContext`.
unsafe extern "system" fn create_context_detour(this: *mut c_void, options: u32, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: as in `create_dc_detour`.
    unsafe {
        let Some(f) = any_slot_orig::<FnCreateDeviceContext>(this, &SLOTS_DEVICE_CREATE_CONTEXT) else { return HRESULT(-1) };
        let hr = f(this, options, out);
        if let Some(rt) = created(hr, out) {
            hook_render_target(rt);
        }
        hr
    }
}

// ---- text slots ----

/// The app sets its own antialias mode / rendering params: keep ours instead
/// (upstream does the same), unless we have none.
unsafe extern "system" fn set_text_aa_detour(this: *mut c_void, mode: i32) {
    // SAFETY: `this` is the render target the app called.
    unsafe {
        let Some(f) = slot_orig::<FnSetTextAaMode>(this, SLOT_SET_TEXT_AA_MODE) else { return };
        f(this, dw_rendering().map_or(mode, |d| d.aa_mode));
    }
}

unsafe extern "system" fn set_text_rp_detour(this: *mut c_void, params: *mut c_void) {
    // Keep our clone alive across the call: a profile reload on another
    // thread may replace DW_RENDERING meanwhile, and this clone is then the
    // only reference behind the raw pointer we pass.
    let dw = dw_rendering();
    // SAFETY: `this` is the render target the app called.
    unsafe {
        let Some(f) = slot_orig::<FnSetTextRenderingParams>(this, SLOT_SET_TEXT_RENDERING_PARAMS) else { return };
        f(this, dw.as_ref().map_or(params, |d| d.params.as_raw()));
    }
}

/// Run the original draw with the grid-fit nudge upstream applies: with
/// grid fitting off, a 1/65535 skew keeps DirectWrite from snapping glyphs
/// to whole pixels.
fn with_grid_fit_nudge(rt: &ID2D1RenderTarget, draw: impl FnOnce()) {
    if !dw_rendering().is_some_and(|d| d.grid_fit_disabled) {
        draw();
        return;
    }
    let mut prev = Matrix3x2::default();
    // SAFETY: transform get/set on a live render target, restored after.
    unsafe {
        rt.GetTransform(&raw mut prev);
        let mut skew = prev;
        skew.M12 += 1.0 / 65535.0;
        skew.M21 += 1.0 / 65535.0;
        rt.SetTransform(&raw const skew);
        draw();
        rt.SetTransform(&raw const prev);
    }
}

unsafe extern "system" fn dgr_detour(this: *mut c_void, baseline: Vector2, run: *const DWRITE_GLYPH_RUN, brush: *mut c_void, measuring: i32) {
    // SAFETY: `this` is the render target the app called, `run` and `brush`
    // its arguments, all live for the call. The GlyphRun borrow ends before
    // the original is called.
    unsafe {
        let Some(rt) = ID2D1RenderTarget::from_raw_borrowed(&this) else { return };
        if run.as_ref().and_then(|r| GlyphRun::borrow(r)).is_some_and(|g| substitute(rt, baseline, &g, brush).is_some()) {
            return;
        }
        if let Some(f) = slot_orig::<FnDrawGlyphRun>(this, SLOT_DRAW_GLYPH_RUN) {
            with_grid_fit_nudge(rt, || f(this, baseline, run, brush, measuring));
        }
    }
}

unsafe extern "system" fn dgr1_detour(
    this: *mut c_void, baseline: Vector2, run: *const DWRITE_GLYPH_RUN, desc: *const c_void, brush: *mut c_void, measuring: i32,
) {
    // SAFETY: as in `dgr_detour`.
    unsafe {
        let Some(rt) = ID2D1RenderTarget::from_raw_borrowed(&this) else { return };
        if run.as_ref().and_then(|r| GlyphRun::borrow(r)).is_some_and(|g| substitute(rt, baseline, &g, brush).is_some()) {
            return;
        }
        if let Some(f) = slot_orig::<FnDrawGlyphRun1>(this, SLOT_DRAW_GLYPH_RUN1) {
            with_grid_fit_nudge(rt, || f(this, baseline, run, desc, brush, measuring));
        }
    }
}

// ---- substitution ----

/// Read the run's ink color from the D2D brush. D2D DrawGlyphRun paints with
/// the given brush, so mirroring the GDI/DWrite paths means honoring it — a
/// solid-color brush yields its RGB; anything else falls back to black. Colors
/// are premultiplied-free sRGB floats in 0..1.
///
/// # Safety
/// `brush` must be null or a live `ID2D1Brush`.
unsafe fn brush_ink(brush: *mut c_void) -> Ink {
    let to8 = |v: f32| u8::try_from(round_i32(v.clamp(0.0, 1.0) * 255.0)).unwrap_or(255);
    // SAFETY: per the contract above; `from_raw_borrowed` does not AddRef.
    let color = unsafe { ID2D1Brush::from_raw_borrowed(&brush) }
        .and_then(|b| b.cast::<ID2D1SolidColorBrush>().ok())
        // SAFETY: a getter on a live brush.
        .map(|scb| unsafe { scb.GetColor() });
    color.map_or_else(Ink::default, |c| Ink { fg: [to8(c.r), to8(c.g), to8(c.b)] })
}

/// Draw the run with render-core if the target lends a GDI DC.
///
/// # Safety
/// `brush` must be null or a live `ID2D1Brush`.
unsafe fn substitute(rt: &ID2D1RenderTarget, baseline: Vector2, g: &GlyphRun<'_>, brush: *mut c_void) -> Option<()> {
    // Nothing to do without a render state; decide that before the GetDC
    // round trip below, which flushes and copies the target.
    if RENDER.lock().ok()?.is_none() {
        return None;
    }
    // Can this target lend a GDI DC at all? Ask first: on DXGI-surface
    // device contexts (most Direct2D 1.1 apps) it cannot, and that answer
    // must not cost a font-file read or the render lock.
    let gi: ID2D1GdiInteropRenderTarget = rt.cast().ok()?;
    // SAFETY: GetDC/ReleaseDC pair on a live interop target.
    let hdc = unsafe { gi.GetDC(D2D1_DC_INITIALIZE_MODE_COPY) }.ok()?;
    // SAFETY: forwarded.
    let ink = unsafe { brush_ink(brush) };
    let result = substitute_on_dc(hdc, baseline, g, ink);
    // SAFETY: releasing the DC obtained above.
    let _ = unsafe { gi.ReleaseDC(None) };
    result
}

fn substitute_on_dc(hdc: HDC, baseline: Vector2, g: &GlyphRun<'_>, ink: Ink) -> Option<()> {
    let advance: f32 = g.advances.map_or(0.0, |a| a.iter().sum());
    // Region around the text baseline.
    let (bx, by) = (round_i32(baseline.X), round_i32(baseline.Y));
    let (rx, ry) = (bx, by - g.px - g.px / 4);
    let mut dib = Dib::new(hdc, round_i32(advance.ceil()) + g.px, g.px * 2)?;
    dib.copy_from(hdc, rx, ry);
    let mut canvas = dib.canvas();
    // The render lock serialises every draw; held for this statement only.
    RENDER.lock().ok()?.as_mut().and_then(|st| {
        g.reface(st, "d2d")?;
        let RenderState { ft, tables, profile, .. } = st;
        draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, g.glyphs, g.px, (bx - rx, by - ry), None);
        Some(())
    })?;
    dib.blit(&canvas);
    dib.copy_to(hdc, rx, ry);
    if !D2D_CAPTURED.swap(true, Ordering::SeqCst) {
        log(&format!("substituted D2D DrawGlyphRun via render-core ({} glyphs, {}px)", g.glyphs.len(), g.px));
    }
    Some(())
}

// ---- setup ----

/// Hook the three d2d1 exports that start the creation chains above.
pub(crate) fn setup_d2d_hook() {
    // SAFETY: loading/looking up a system DLL has no preconditions; d2d1
    // then stays loaded for the process's life.
    let d2d1 = unsafe { GetModuleHandleW(w!("d2d1.dll")).ok().filter(|h| !h.is_invalid()).or_else(|| LoadLibraryW(w!("d2d1.dll")).ok()) };
    let Some(d2d1) = d2d1 else {
        log("d2d1.dll not available");
        return;
    };
    // SAFETY: for all three, the export name is NUL-terminated, the export
    // stays mapped for the process's life, each detour matches its
    // signature, and each transmute types the trampoline, which continues
    // that same function.
    unsafe {
        hook_export(d2d1, "D2D1CreateFactory", s!("D2D1CreateFactory"), d2dcf_detour as *const (), |t| {
            let _ = ORIG_D2DCF.set(std::mem::transmute::<*const (), FnD2DCreateFactory>(t));
        });
        hook_export(d2d1, "D2D1CreateDevice", s!("D2D1CreateDevice"), d2dcd_detour as *const (), |t| {
            let _ = ORIG_D2DCD.set(std::mem::transmute::<*const (), FnD2DCreateDevice>(t));
        });
        hook_export(d2d1, "D2D1CreateDeviceContext", s!("D2D1CreateDeviceContext"), d2dcdc_detour as *const (), |t| {
            let _ = ORIG_D2DCDC.set(std::mem::transmute::<*const (), FnD2DCreateDevice>(t));
        });
    }
}

/// Detour one export of `module`, logging the outcome.
///
/// # Safety
/// As `install_hook`, plus `sym` must be NUL-terminated.
unsafe fn hook_export(module: HMODULE, name: &str, sym: PCSTR, detour: *const (), publish: impl FnOnce(*const ())) {
    // SAFETY: forwarded from the caller's contract.
    let ok = unsafe { GetProcAddress(module, sym).is_some_and(|target| install_hook(target as *const (), detour, publish)) };
    log(&if ok { format!("hook installed on {name}") } else { format!("{name} hook failed") });
}
