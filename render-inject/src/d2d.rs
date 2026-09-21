//! Direct2D text: `ID2D1RenderTarget::DrawGlyphRun`.
//!
//! Reaching it takes three steps, because the render targets we want are
//! created at runtime: hook `D2D1CreateFactory`, patch the factory's
//! render-target creation slots, then patch DrawGlyphRun in each render
//! target's vtable. Each vtable is patched once, tracked by D2D_DGR_ORIG.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use render_core::render::{draw_glyphs_onto, Canvas, Ink};
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Graphics::Direct2D::{
    ID2D1Brush, ID2D1GdiInteropRenderTarget, ID2D1RenderTarget, ID2D1SolidColorBrush,
    D2D1_DC_INITIALIZE_MODE_COPY,
};
use windows::Win32::Graphics::DirectWrite::DWRITE_GLYPH_RUN;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::{s, w};
use windows_numerics::Vector2;

use crate::hook::{install_hook, patch_slot, VTABLE_PATCH_LOCK};
use crate::state::{orig, RenderState, RENDER};
use crate::dwrite::dwrite_font_bytes;
use crate::log;

// ---- Direct2D (ID2D1RenderTarget::DrawGlyphRun) ----
type FnD2DCreateFactory = unsafe extern "system" fn(i32, *const GUID, *const c_void, *mut *mut c_void) -> HRESULT;
type FnCreateDCRT = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
type FnCreateHwndRT = unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
type FnD2DDrawGlyphRun = unsafe extern "system" fn(*mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *mut c_void, i32);
static ORIG_D2DCF: OnceLock<FnD2DCreateFactory> = OnceLock::new();
static ORIG_DCRT: OnceLock<FnCreateDCRT> = OnceLock::new();
static ORIG_HWNDRT: OnceLock<FnCreateHwndRT> = OnceLock::new();
static D2D_DGR_ORIG: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None); // rt vtable -> orig DrawGlyphRun
static D2D_CAPTURED: AtomicBool = AtomicBool::new(false); // log the first D2D substitution once


unsafe extern "system" fn d2dcf_detour(ftype: i32, riid: *const GUID, opts: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let hr = (orig(&ORIG_D2DCF))(ftype, riid, opts, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() {
        let vtbl = *(*out as *mut *mut usize);
        // ID2D1Factory: CreateHwndRenderTarget = slot 14, CreateDCRenderTarget = slot 16.
        // Serialise the one-time patch: two threads creating factories at once
        // must not both capture the "original" (see VTABLE_PATCH_LOCK).
        let _guard = VTABLE_PATCH_LOCK.lock();
        if ORIG_HWNDRT.get().is_none() {
            patch_slot(vtbl.add(14), create_hwnd_detour as *const () as usize, |o| { let _ = ORIG_HWNDRT.set(std::mem::transmute(o)); });
        }
        if ORIG_DCRT.get().is_none() {
            patch_slot(vtbl.add(16), create_dc_detour as *const () as usize, |o| { let _ = ORIG_DCRT.set(std::mem::transmute(o)); });
        }
        log("hook installed on D2D1Factory render-target creation");
    }
    hr
}

unsafe extern "system" fn create_dc_detour(this: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let hr = (orig(&ORIG_DCRT))(this, props, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { patch_rt_dgr(*out); }
    hr
}
unsafe extern "system" fn create_hwnd_detour(this: *mut c_void, p1: *const c_void, p2: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let hr = (orig(&ORIG_HWNDRT))(this, p1, p2, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { patch_rt_dgr(*out); }
    hr
}

unsafe fn patch_rt_dgr(rt: *mut c_void) {
    let vtbl = *(rt as *mut *mut usize);
    let vptr = vtbl as usize;
    let Ok(mut m) = D2D_DGR_ORIG.lock() else { return };
    let map = m.get_or_insert_with(HashMap::new);
    if map.contains_key(&vptr) { return; }
    if patch_slot(vtbl.add(29), d2d_dgr_detour as *const () as usize, |old| { map.insert(vptr, old); }) {
        log("hook installed on D2D DrawGlyphRun");
    }
}

unsafe extern "system" fn d2d_dgr_detour(this: *mut c_void, baseline: Vector2, run: *const DWRITE_GLYPH_RUN, brush: *mut c_void, measuring: i32) {
    if !run.is_null() && d2d_substitute(this, baseline, &*run, brush).is_some() {
        return;
    }
    let vptr = *(this as *const usize); // this's vtable pointer
    let orig = D2D_DGR_ORIG.lock().ok().and_then(|m| m.as_ref().and_then(|map| map.get(&vptr).copied()));
    if let Some(o) = orig {
        let f: FnD2DDrawGlyphRun = std::mem::transmute(o);
        f(this, baseline, run, brush, measuring);
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





/// Hook d2d1!D2D1CreateFactory so we can patch render-target DrawGlyphRun.
pub(crate) unsafe fn setup_d2d_hook() {
    let d2d1 = match GetModuleHandleW(w!("d2d1.dll")) {
        Ok(h) if !h.is_invalid() => h,
        _ => match windows::Win32::System::LibraryLoader::LoadLibraryW(w!("d2d1.dll")) {
            Ok(h) => h.into(),
            Err(_) => { log("d2d1.dll not available"); return; }
        },
    };
    let Some(target) = GetProcAddress(d2d1, s!("D2D1CreateFactory")) else { return };
    if install_hook(target as *const (), d2dcf_detour as *const (), |tramp| {
        let _ = ORIG_D2DCF.set(std::mem::transmute::<*const (), FnD2DCreateFactory>(tramp));
    }) {
        log("hook installed on D2D1CreateFactory");
    } else {
        log("D2D1CreateFactory hook failed");
    }
}
