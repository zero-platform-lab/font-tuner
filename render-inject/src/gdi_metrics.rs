//! GDI glyph *metrics*: `GetGlyphOutlineW` / `GetGlyphOutlineA`.
//!
//! Upstream's "ClipBoxFix" (`override.cpp`, `[Experimental] ClipBoxFix`,
//! default on). Apps that rasterise or clip glyphs themselves (Java2D, so
//! IntelliJ) first ask `GetGlyphOutline` for the metrics only, then clip to
//! that box. Our glyphs come out a little heavier than GDI's, so the box is
//! padded by ~1.5px scaled to the screen DPI: the origin is moved up (capped
//! at the ascent) and the black box grown by the same amount (capped at the
//! font height). Only the metrics-only call is touched; a call that fetches
//! bitmap or outline data is passed through untouched.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use windows::core::s;
use windows::Win32::Graphics::Gdi::{
    GetDC, GetDeviceCaps, GetTextMetricsW, ReleaseDC, GGO_BITMAP, GGO_GRAY2_BITMAP, GGO_GRAY4_BITMAP,
    GGO_GRAY8_BITMAP, GGO_NATIVE, GLYPHMETRICS, HDC, LOGPIXELSX, TEXTMETRICW,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::w;

use crate::hook::install_hook;
use crate::log;
use crate::state::{orig, RENDER};

/// `GetGlyphOutline{W,A}(hdc, uChar, fuFormat, lpgm, cjBuffer, pvBuffer, lpmat2)`.
type FnGgo = unsafe extern "system" fn(HDC, u32, u32, *mut GLYPHMETRICS, u32, *mut c_void, *const c_void) -> u32;
static ORIG_W: OnceLock<FnGgo> = OnceLock::new();
static ORIG_A: OnceLock<FnGgo> = OnceLock::new();
static LOGGED_W: AtomicBool = AtomicBool::new(false);
static LOGGED_A: AtomicBool = AtomicBool::new(false);

/// Upstream's `floor(1.5 * ScreenDpi / 96)`, from the screen DC's LOGPIXELSX.
fn pad_px() -> i32 {
    static PAD: OnceLock<i32> = OnceLock::new();
    *PAD.get_or_init(|| unsafe {
        let dc = GetDC(None);
        let dpi = GetDeviceCaps(Some(dc), LOGPIXELSX);
        let _ = ReleaseDC(None, dc);
        (1.5 * dpi.max(96) as f32 / 96.0).floor() as i32
    })
}

fn clipbox_fix_enabled() -> bool {
    RENDER.lock().ok().and_then(|g| g.as_ref().map(|s| s.profile.clipbox_fix)).unwrap_or(false)
}

/// The metrics adjustment, exactly as upstream computes it. Returns whether
/// it applied (metrics-only call, fix enabled).
unsafe fn fix_metrics(hdc: HDC, format: u32, gm: *mut GLYPHMETRICS, cj: u32, buf: *mut c_void) -> bool {
    if !(cj == 0 || buf.is_null()) || gm.is_null() { return false; }
    let data_formats = GGO_BITMAP.0 | GGO_GRAY2_BITMAP.0 | GGO_GRAY4_BITMAP.0 | GGO_GRAY8_BITMAP.0 | GGO_NATIVE.0;
    if format & data_formats != 0 { return false; }
    if !clipbox_fix_enabled() { return false; }
    let n = pad_px();
    let mut tm = TEXTMETRICW::default();
    if !GetTextMetricsW(hdc, &mut tm).as_bool() { return false; }
    let gm = &mut *gm;
    // Move the origin up by n, but not past the ascent.
    let mut dy = n;
    if gm.gmptGlyphOrigin.y < tm.tmAscent {
        if gm.gmptGlyphOrigin.y + dy > tm.tmAscent { dy = tm.tmAscent - gm.gmptGlyphOrigin.y; }
    } else {
        dy = 0;
    }
    gm.gmptGlyphOrigin.y += dy;
    gm.gmBlackBoxY = (gm.gmBlackBoxY as i32 + dy) as u32;
    // Grow the black box by n while the glyph still fits in the font height.
    let bottom = tm.tmAscent - gm.gmptGlyphOrigin.y + gm.gmBlackBoxY as i32;
    if bottom - 1 < tm.tmHeight {
        if bottom + 1 + n > tm.tmHeight {
            gm.gmBlackBoxY = (tm.tmHeight - tm.tmAscent + gm.gmptGlyphOrigin.y + 1) as u32;
        } else {
            gm.gmBlackBoxY = (gm.gmBlackBoxY as i32 + n) as u32;
        }
    }
    true
}

unsafe extern "system" fn ggo_w_detour(hdc: HDC, ch: u32, format: u32, gm: *mut GLYPHMETRICS, cj: u32, buf: *mut c_void, mat: *const c_void) -> u32 {
    let ret = (orig(&ORIG_W))(hdc, ch, format, gm, cj, buf, mat);
    if ret != u32::MAX && fix_metrics(hdc, format, gm, cj, buf) && !LOGGED_W.swap(true, Ordering::Relaxed) {
        log("GetGlyphOutlineW metrics adjusted (ClipBoxFix)");
    }
    ret
}

unsafe extern "system" fn ggo_a_detour(hdc: HDC, ch: u32, format: u32, gm: *mut GLYPHMETRICS, cj: u32, buf: *mut c_void, mat: *const c_void) -> u32 {
    let ret = (orig(&ORIG_A))(hdc, ch, format, gm, cj, buf, mat);
    if ret != u32::MAX && fix_metrics(hdc, format, gm, cj, buf) && !LOGGED_A.swap(true, Ordering::Relaxed) {
        log("GetGlyphOutlineA metrics adjusted (ClipBoxFix)");
    }
    ret
}

/// Detour both exports. Unlike `TextOutW`/`ExtTextOutA`, which end in the
/// `ExtTextOutW` entry, `GetGlyphOutlineA` does not route through the W
/// export (checked on Windows 11 26200 with the probe harness), so it needs
/// its own detour.
pub(crate) unsafe fn setup_gdi_metrics_hooks() {
    let Ok(gdi32) = GetModuleHandleW(w!("gdi32.dll")) else { return };
    for (name, cell, detour) in [
        ("GetGlyphOutlineW", &ORIG_W, ggo_w_detour as *const ()),
        ("GetGlyphOutlineA", &ORIG_A, ggo_a_detour as *const ()),
    ] {
        let sym = if name.ends_with('W') { s!("GetGlyphOutlineW") } else { s!("GetGlyphOutlineA") };
        let Some(target) = GetProcAddress(gdi32, sym) else { continue };
        if install_hook(target as *const (), detour, |tramp| {
            let _ = cell.set(std::mem::transmute::<*const (), FnGgo>(tramp));
        }) {
            log(&format!("hook installed on {name}"));
        } else {
            log(&format!("{name} hook failed"));
        }
    }
}
