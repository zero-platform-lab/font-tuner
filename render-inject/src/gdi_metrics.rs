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
/// Integer form: `3 * dpi / 192` is the same floor without the float trip.
fn pad_px() -> i32 {
    static PAD: OnceLock<i32> = OnceLock::new();
    *PAD.get_or_init(|| {
        // SAFETY: a screen DC obtained and released in the same expression.
        let dpi = unsafe {
            let dc = GetDC(None);
            let dpi = GetDeviceCaps(Some(dc), LOGPIXELSX);
            let _ = ReleaseDC(None, dc);
            dpi
        };
        3 * dpi.max(96) / 192
    })
}

fn clipbox_fix_enabled() -> bool {
    RENDER.lock().ok().and_then(|g| g.as_ref().map(|s| s.profile.clipbox_fix)).unwrap_or(false)
}

/// Is this a metrics-only query (no buffer, no bitmap/outline format)? Only
/// those get the fix; a call that fetches data is passed through untouched.
fn is_metrics_only(format: u32, cj: u32, buf: *mut c_void) -> bool {
    let data_formats = GGO_BITMAP.0 | GGO_GRAY2_BITMAP.0 | GGO_GRAY4_BITMAP.0 | GGO_GRAY8_BITMAP.0 | GGO_NATIVE.0;
    (cj == 0 || buf.is_null()) && format & data_formats == 0
}

/// The metrics adjustment, exactly as upstream computes it: origin up by
/// `n` (capped at the ascent), black box grown by the same (capped at the
/// font height). Pure so it can be reasoned about without a DC.
fn pad_metrics(gm: &mut GLYPHMETRICS, tm: &TEXTMETRICW, n: i32) {
    let mut dy = n;
    if gm.gmptGlyphOrigin.y < tm.tmAscent {
        if gm.gmptGlyphOrigin.y + dy > tm.tmAscent {
            dy = tm.tmAscent - gm.gmptGlyphOrigin.y;
        }
    } else {
        dy = 0;
    }
    gm.gmptGlyphOrigin.y += dy;
    let mut box_y = i32::try_from(gm.gmBlackBoxY).unwrap_or(i32::MAX).saturating_add(dy);
    let bottom = tm.tmAscent - gm.gmptGlyphOrigin.y + box_y;
    if bottom - 1 < tm.tmHeight {
        box_y = if bottom + 1 + n > tm.tmHeight { tm.tmHeight - tm.tmAscent + gm.gmptGlyphOrigin.y + 1 } else { box_y + n };
    }
    gm.gmBlackBoxY = u32::try_from(box_y).unwrap_or(0);
}

/// Apply the fix after a successful `GetGlyphOutline`. Returns whether it applied.
///
/// # Safety
/// `gm` must be the caller's `LPGLYPHMETRICS`, valid for writes (the original
/// call just filled it).
unsafe fn fix_metrics(hdc: HDC, format: u32, gm: *mut GLYPHMETRICS, cj: u32, buf: *mut c_void) -> bool {
    if gm.is_null() || !is_metrics_only(format, cj, buf) || !clipbox_fix_enabled() {
        return false;
    }
    let mut tm = TEXTMETRICW::default();
    // SAFETY: `hdc` is the app's DC for this very call; `tm` is ours.
    if !unsafe { GetTextMetricsW(hdc, &raw mut tm) }.as_bool() {
        return false;
    }
    // SAFETY: non-null and valid for writes per the contract above.
    pad_metrics(unsafe { &mut *gm }, &tm, pad_px());
    true
}

unsafe extern "system" fn ggo_w_detour(hdc: HDC, ch: u32, format: u32, gm: *mut GLYPHMETRICS, cj: u32, buf: *mut c_void, mat: *const c_void) -> u32 {
    // SAFETY: arguments are forwarded untouched to the real function.
    let ret = unsafe { (orig(&ORIG_W))(hdc, ch, format, gm, cj, buf, mat) };
    // SAFETY: `gm` is the caller's out-parameter, valid for the call's duration.
    if ret != u32::MAX && unsafe { fix_metrics(hdc, format, gm, cj, buf) } && !LOGGED_W.swap(true, Ordering::Relaxed) {
        log("GetGlyphOutlineW metrics adjusted (ClipBoxFix)");
    }
    ret
}

unsafe extern "system" fn ggo_a_detour(hdc: HDC, ch: u32, format: u32, gm: *mut GLYPHMETRICS, cj: u32, buf: *mut c_void, mat: *const c_void) -> u32 {
    // SAFETY: as in `ggo_w_detour`.
    let ret = unsafe { (orig(&ORIG_A))(hdc, ch, format, gm, cj, buf, mat) };
    // SAFETY: as in `ggo_w_detour`.
    if ret != u32::MAX && unsafe { fix_metrics(hdc, format, gm, cj, buf) } && !LOGGED_A.swap(true, Ordering::Relaxed) {
        log("GetGlyphOutlineA metrics adjusted (ClipBoxFix)");
    }
    ret
}

/// Detour both exports. Unlike `TextOutW`/`ExtTextOutA`, which end in the
/// `ExtTextOutW` entry, `GetGlyphOutlineA` does not route through the W
/// export (checked on Windows 11 26200 with the probe harness), so it needs
/// its own detour.
pub(crate) fn setup_gdi_metrics_hooks() {
    // SAFETY: gdi32 is loaded for the life of every GUI process.
    let Ok(gdi32) = (unsafe { GetModuleHandleW(w!("gdi32.dll")) }) else { return };
    for (name, sym, cell, detour) in [
        ("GetGlyphOutlineW", s!("GetGlyphOutlineW"), &ORIG_W, ggo_w_detour as *const ()),
        ("GetGlyphOutlineA", s!("GetGlyphOutlineA"), &ORIG_A, ggo_a_detour as *const ()),
    ] {
        // SAFETY: `sym` is a NUL-terminated export name.
        let Some(target) = (unsafe { GetProcAddress(gdi32, sym) }) else { continue };
        // SAFETY: `target` is a gdi32 export (mapped for the process's life)
        // and `detour` has the identical `system` ABI signature.
        let ok = unsafe {
            install_hook(target as *const (), detour, |tramp| {
                // The transmute is sound because the trampoline continues the
                // very function we detoured, with the same signature.
                let _ = cell.set(std::mem::transmute::<*const (), FnGgo>(tramp));
            })
        };
        if ok {
            log(&format!("hook installed on {name}"));
        } else {
            log(&format!("{name} hook failed"));
        }
    }
}
