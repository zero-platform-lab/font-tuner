//! GDI text: `gdi32!ExtTextOutW`, which `TextOutW`, `TextOutA` and
//! `ExtTextOutA` also end in (checked on Windows 11 26200).
//!
//! The detour converts the raw arguments into `Draw` once, at the boundary,
//! and everything after that is safe code: resolve the DC's font into
//! render-core, mirror the affected rectangle into a DIB, draw the run over
//! it, copy it back. Any step that cannot be done hands the call to GDI.

use core::ffi::c_void;
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

use render_core::render::{draw_glyphs_onto, draw_text_onto, Canvas, Ink, Layout};
use render_core::Ft;
use windows::core::{s, w, BOOL};
use windows::Win32::Foundation::{POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    GetBkColor, GetBkMode, GetCurrentObject, GetCurrentPositionEx, GetFontData, GetObjectW, GetTextAlign,
    GetTextCharacterExtra, GetTextColor, GetTextExtentPoint32W, GetTextExtentPointI, GetTextMetricsW, MoveToEx, HDC,
    LOGFONTW, OBJ_FONT, OPAQUE, TEXTMETRICW,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use crate::dib::Dib;
use crate::hook::install_hook;
use crate::log;
use crate::state::{orig, RenderState, CAPTURED, RENDER};
use crate::xform;

/// `ExtTextOutW(hdc, x, y, options, lprect, lpString, c, lpDx)`.
type FnEto = unsafe extern "system" fn(HDC, i32, i32, u32, *const RECT, *const u16, u32, *const i32) -> BOOL;

/// Trampoline to the real ExtTextOutW, published before the detour goes live.
/// `OnceLock` gives set-once semantics and a safe read, so a second attach
/// cannot overwrite it with a pointer to our own jump.
static ORIG: OnceLock<FnEto> = OnceLock::new();

thread_local! {
    // Per-thread re-entrancy guard for the GDI detour. Must be thread-local, not
    // a process-global flag: a global one makes every *other* thread's
    // ExtTextOutW fall back to untuned GDI whenever one thread is mid-render, so
    // multi-window apps render inconsistently. We only need to stop the same
    // thread re-entering (our own GDI calls / nested draws).
    static IN_DETOUR: Cell<bool> = const { Cell::new(false) };
}

const ETO_OPAQUE: u32 = 0x0002;
const ETO_CLIPPED: u32 = 0x0004;
const ETO_GLYPH_INDEX: u32 = 0x0010;
/// `SetTextAlign` flags (wingdi.h). The horizontal ones are a 2-bit field
/// (LEFT 0 / RIGHT 2 / CENTER 6) and the vertical ones another
/// (TOP 0 / BOTTOM 8 / BASELINE 24); CENTER and BASELINE each set two bits,
/// so they must be masked, not tested with `&`.
const TA_LEFT: u32 = 0;
const TA_RIGHT: u32 = 2;
const TA_CENTER: u32 = 6;
const TA_TOP: u32 = 0;
const TA_BOTTOM: u32 = 8;
const TA_BASELINE: u32 = 24;
const TA_UPDATECP: u32 = 1;
const TA_HORZ_MASK: u32 = TA_LEFT | TA_RIGHT | TA_CENTER;
const TA_VERT_MASK: u32 = TA_TOP | TA_BOTTOM | TA_BASELINE;
/// The `'ttcf'` table tag: present only for TrueType collections.
const TTCF: u32 = 0x6663_7474;

/// One `ExtTextOutW` call, with the raw pointers already turned into
/// borrows that live for the duration of the detour.
struct Draw<'a> {
    hdc: HDC,
    x: i32,
    y: i32,
    options: u32,
    rect: Option<RECT>,
    /// UTF-16 code units, or glyph indices when `options` has `ETO_GLYPH_INDEX`.
    text: &'a [u16],
    dx: Option<&'a [i32]>,
}

impl Draw<'_> {
    fn glyph_mode(&self) -> bool {
        self.options & ETO_GLYPH_INDEX != 0
    }
    /// The rect as `(left, top, right, bottom)`.
    fn rect_tuple(&self) -> Option<(i32, i32, i32, i32)> {
        self.rect.map(|r| (r.left, r.top, r.right, r.bottom))
    }
}

/// A `COLORREF` (`0x00BBGGRR`) as `[r, g, b]`.
fn rgb(colorref: u32) -> [u8; 3] {
    let [r, g, b, _] = colorref.to_le_bytes();
    [r, g, b]
}

unsafe extern "system" fn detour(
    hdc: HDC, x: i32, y: i32, options: u32, rect: *const RECT, text: *const u16, count: u32, dx: *const i32,
) -> BOOL {
    // Guard against re-entrancy (our own GDI calls, or nested draws) on this
    // thread only.
    if IN_DETOUR.with(|f| f.replace(true)) {
        // SAFETY: arguments forwarded untouched to the real function.
        return unsafe { (orig(&ORIG))(hdc, x, y, options, rect, text, count, dx) };
    }
    let handled = if text.is_null() || count == 0 {
        false
    } else {
        let count = count as usize;
        // SAFETY: GDI's contract for ExtTextOutW — `text` holds `count`
        // code units (or glyph indices), `dx` when non-null holds `count`
        // advances, `rect` when non-null is one RECT — all valid for the
        // duration of the call. The borrows end before the call returns.
        let draw = unsafe {
            Draw {
                hdc,
                x,
                y,
                options,
                rect: rect.as_ref().copied(),
                text: core::slice::from_raw_parts(text, count),
                dx: (!dx.is_null()).then(|| core::slice::from_raw_parts(dx, count)),
            }
        };
        render_into_dc(&draw).is_some()
    };
    IN_DETOUR.with(|f| f.set(false));
    if handled {
        BOOL(1)
    } else {
        // SAFETY: as above.
        unsafe { (orig(&ORIG))(hdc, x, y, options, rect, text, count, dx) }
    }
}

/// Resolve the DC's font into render-core, or None. Re-extracts + re-faces
/// only when the font differs from `cache`. The pixel size is not taken from
/// here: `LOGFONTW.lfHeight` means the em size when negative but the *cell*
/// height (em + internal leading) when positive, so the caller derives it
/// from the text metrics instead (see `em_px`).
fn resolve_font(hdc: HDC, ft: &Ft, cache: &mut Option<String>) -> Option<()> {
    let mut lf = LOGFONTW::default();
    // SAFETY: `lf` is a LOGFONTW and the size passed is exactly its size.
    unsafe {
        let hfont = GetCurrentObject(hdc, OBJ_FONT);
        let size = i32::try_from(core::mem::size_of::<LOGFONTW>()).unwrap_or(i32::MAX);
        GetObjectW(hfont, size, Some((&raw mut lf).cast::<c_void>()));
    }
    // Size-only probe is cheap; the full read only happens on a cache miss.
    // SAFETY: GetFontData with a null buffer only reports the size.
    let (table, size) = unsafe {
        match GetFontData(hdc, TTCF, 0, None, 0) {
            0 | u32::MAX => (0, GetFontData(hdc, 0, 0, None, 0)),
            n => (TTCF, n),
        }
    };
    if size == 0 || size == u32::MAX {
        return None;
    }
    let name_len = lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(0);
    let face = String::from_utf16_lossy(&lf.lfFaceName[..name_len]);
    let key = format!("gdi:{face}:{size}");
    if cache.as_deref() != Some(key.as_str()) {
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` is exactly `size` bytes, the length GDI reported.
        unsafe { GetFontData(hdc, table, 0, Some(buf.as_mut_ptr().cast::<c_void>()), size) };
        ft.reface_memory(&buf, &face).ok()?;
        *cache = Some(key);
    }
    Some(())
}

/// The em size in pixels for the DC's selected font, as upstream computes it
/// (`ft.cpp`: `tmHeight - tmInternalLeading`). This is right for both signs
/// of `lfHeight`; `|lfHeight|` would render a positive (cell-height) font too
/// large by the internal leading. Falls back to 16 if the metrics are odd.
fn em_px(tm: &TEXTMETRICW) -> i32 {
    let em = tm.tmHeight - tm.tmInternalLeading;
    if em > 0 { em } else { 16 }
}

/// Text metrics and the run's extent on this DC. `None` when GDI cannot
/// measure it (then GDI draws it too).
fn measure(d: &Draw<'_>) -> Option<(TEXTMETRICW, SIZE)> {
    let mut tm = TEXTMETRICW::default();
    let mut sz = SIZE::default();
    // SAFETY: `hdc` is the app's DC for this call; the out-params are ours.
    let ok = unsafe {
        let _ = GetTextMetricsW(d.hdc, &raw mut tm);
        if d.glyph_mode() {
            GetTextExtentPointI(d.hdc, d.text, &raw mut sz)
        } else {
            GetTextExtentPoint32W(d.hdc, d.text, &raw mut sz)
        }
    };
    (ok.as_bool() && sz.cx > 0).then_some((tm, sz))
}

/// Width of the run in pixels. An explicit `lpDx` overrides the font's
/// advances, so sum it (plus the inter-character spacing GDI adds on top —
/// measured). Without `lpDx`, `GetTextExtentPoint*` already includes the
/// spacing, so its measurement stands.
fn text_width(d: &Draw<'_>, sz: SIZE, layout: Layout<'_>) -> i32 {
    layout.dx_width(d.text.len()).unwrap_or(sz.cx)
}

/// Draw the run onto `canvas` with the shared face, refaced to the DC's font.
fn draw_run(st: &mut RenderState, canvas: &mut Canvas, d: &Draw<'_>, ink: Ink, pen: (i32, i32), px: i32,
            layout: Layout<'_>) -> Option<()> {
    let RenderState { ft, tables, profile, font_key, font_face } = st;
    resolve_font(d.hdc, ft, font_key)?;
    *font_face = None; // a GDI key does not name a DirectWrite face
    if d.glyph_mode() {
        draw_glyphs_onto(canvas, ft, tables, profile, ink, d.text, px, pen, layout);
    } else {
        draw_text_onto(canvas, ft, tables, profile, ink, &String::from_utf16_lossy(d.text), px, pen, layout);
    }
    Some(())
}

/// Returns `Some(())` if we handled the draw, `None` to fall back to GDI.
fn render_into_dc(d: &Draw<'_>) -> Option<()> {
    // The DC's logical-to-device mapping. `None` means a rotation, shear or
    // mirror, which render-core cannot lay out - GDI draws those itself.
    let map = xform::mapping(d.hdc)?;
    let (tm, sz) = measure(d)?;
    // SAFETY: attribute reads on the app's DC.
    let (color, align, bk, bk_mode, extra) = unsafe {
        (GetTextColor(d.hdc).0, GetTextAlign(d.hdc).0, GetBkColor(d.hdc).0, GetBkMode(d.hdc), GetTextCharacterExtra(d.hdc))
    };
    // `SetTextCharacterExtra` widens every advance, on top of any lpDx.
    // Both are logical units, so a scaled DC needs them converted; the
    // converted `lpDx` has to be owned for the rest of the draw.
    let device_dx = (!map.is_identity()).then(|| d.dx.map(|dx| map.device_dx(dx))).flatten();
    let layout = Layout {
        dx: device_dx.as_deref().or(d.dx),
        extra: map.len_x(extra),
    };
    // `TA_UPDATECP`: the origin is the DC's current position, not the (x, y)
    // arguments (which GDI ignores then), and the position advances by the
    // run afterwards. Without this the run landed at the arguments - x=1
    // instead of x=120 in a measured sample - and the position never moved.
    let (ox, oy) = if align & TA_UPDATECP != 0 {
        let mut p = POINT::default();
        // SAFETY: `p` is an out-param we own; `hdc` is the app's DC.
        if unsafe { GetCurrentPositionEx(d.hdc, &raw mut p) }.as_bool() { (p.x, p.y) } else { (d.x, d.y) }
    } else {
        (d.x, d.y)
    };

    // `SetTextAlign` places the run relative to (ox, oy); GDI applies it
    // for its own draws, so the port has to apply it too or right- and
    // centre-aligned text lands a whole string width away (measured: a
    // TA_RIGHT run drew at x..x+w instead of x-w..x). Same as upstream
    // (override.cpp `switch (horiz)` / `switch (vert)`).
    // From here on everything is device pixels: that is what render-core
    // rasterises in, and what the DIB is blitted back as. The origin goes
    // through LPtoDP (translation included); lengths scale by the factors.
    let (dox, doy) = map.to_device(ox, oy);
    let (ascent, descent, height) = (map.len_y(tm.tmAscent), map.len_y(tm.tmDescent), map.len_y(tm.tmHeight));
    let width = text_width(d, SIZE { cx: map.len_x(sz.cx), cy: map.len_y(sz.cy) }, layout);
    let left = match align & TA_HORZ_MASK {
        TA_RIGHT => dox - width,
        TA_CENTER => dox - width / 2,
        _ => dox,
    };
    let baseline = match align & TA_VERT_MASK {
        TA_BASELINE => doy,
        TA_BOTTOM => doy - descent,
        _ => doy + ascent,
    };

    // Text-extent region, unioned with the rect so opaque fill / clip fit.
    let (mut rx, mut ry) = (left, baseline - ascent);
    let (mut right, mut bottom) = (left + width + 6, ry + height + 4);
    if let Some(rect) = d.rect_tuple() {
        let (left_px, top_px) = map.to_device(rect.0, rect.1);
        let (right_px, bottom_px) = map.to_device(rect.2, rect.3);
        rx = rx.min(left_px);
        ry = ry.min(top_px);
        right = right.max(right_px);
        bottom = bottom.max(bottom_px);
    }

    // Offscreen DIB seeded with the DC's current pixels; render-core draws
    // the run over them and the result goes back in one blit.
    let mut dib = Dib::new(d.hdc, right - rx, bottom - ry)?;
    dib.copy_from(d.hdc, rx, ry);
    let mut canvas = dib.canvas();
    if let Some(rect) = d.rect_tuple() {
        let (left_px, top_px) = map.to_device(rect.0, rect.1);
        let (right_px, bottom_px) = map.to_device(rect.2, rect.3);
        let box_px = (left_px - rx, top_px - ry, right_px - rx, bottom_px - ry);
        if d.options & ETO_OPAQUE != 0 {
            canvas.fill_rect(box_px, rgb(bk));
        }
        if d.options & ETO_CLIPPED != 0 {
            canvas.set_clip(Some(box_px));
        }
    }
    // `SetBkMode(OPAQUE)` — GDI's default — fills the text box with the
    // background colour as it draws, which is how apps overwrite a value in
    // place (Process Explorer's numeric columns doubled up without this).
    // render-core composites over what is there, so do the fill ourselves.
    // Upstream does the same (override.cpp: `fillrect || GetBkMode == OPAQUE`).
    if bk_mode == OPAQUE.0.cast_signed() {
        let (bx, by) = (left - rx, baseline - ry - ascent);
        canvas.fill_rect((bx, by, bx + width, by + height), rgb(bk));
    }
    let ink = Ink { fg: rgb(color) };
    let pen = (left - rx, baseline - ry);
    // The render lock serialises every draw; it is held for this one
    // statement, while render-core touches the shared face.
    let px = map.len_y(em_px(&tm));
    RENDER.lock().ok()?.as_mut().and_then(|st| draw_run(st, &mut canvas, d, ink, pen, px, layout))?;
    dib.blit(&canvas);

    // Save what render-core produced inside the injected process, once, as proof.
    if !CAPTURED.swap(true, Ordering::SeqCst) {
        if let Some(tmp) = std::env::var_os("TEMP") {
            let p = PathBuf::from(tmp).join("render-inject-capture.png");
            let _ = canvas.save(&p.to_string_lossy());
            log(&format!("captured render-core output to {}", p.display()));
        }
    }
    dib.copy_to(d.hdc, rx, ry);

    // Advance the current position the way GDI would, using its own measured
    // width (the app measured with that, not with the tuned glyphs).
    // Upstream does the same: right-aligned moves back, centred does not move.
    if align & TA_UPDATECP != 0 {
        // The current position is logical, so advance by GDI's own logical
        // measurement rather than the device width used for drawing.
        let nx = match align & TA_HORZ_MASK {
            TA_RIGHT => ox - sz.cx,
            TA_CENTER => ox,
            _ => ox + sz.cx,
        };
        // SAFETY: setting the current position on the app's DC.
        let _ = unsafe { MoveToEx(d.hdc, nx, oy, None) };
    }
    Some(())
}

/// Detour `gdi32!ExtTextOutW`.
pub(crate) fn setup_gdi_hook() {
    // SAFETY: gdi32 is loaded for the life of every GUI process, and the
    // export name is NUL-terminated.
    let target = unsafe { GetModuleHandleW(w!("gdi32.dll")).ok().and_then(|m| GetProcAddress(m, s!("ExtTextOutW"))) };
    let Some(target) = target else {
        log("ExtTextOutW not found");
        return;
    };
    // SAFETY: `target` is a gdi32 export and `detour` has ExtTextOutW's
    // exact `system` ABI signature; the transmute in `publish` types the
    // trampoline, which continues that same function.
    let ok = unsafe {
        install_hook(target as *const (), detour as *const (), |tramp| {
            let _ = ORIG.set(std::mem::transmute::<*const (), FnEto>(tramp));
        })
    };
    log(if ok { "hook installed on ExtTextOutW" } else { "ExtTextOutW hook failed" });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SetTextAlign` moves the run's origin. Measured against plain GDI: a
    /// TA_RIGHT run ends at x, a TA_CENTER run is centred on x.
    #[test]
    fn text_align_places_the_run_like_gdi() {
        let place = |align: u32, x: i32, width: i32| match align & TA_HORZ_MASK {
            TA_RIGHT => x - width,
            TA_CENTER => x - width / 2,
            _ => x,
        };
        assert_eq!(place(TA_LEFT, 160, 74), 160);
        assert_eq!(place(TA_RIGHT, 160, 74), 86);
        assert_eq!(place(TA_CENTER, 160, 74), 123);
        // TA_CENTER sets both bits of the field, so a plain `&` test would
        // also match TA_RIGHT; the mask must be compared for equality.
        assert_ne!(place(TA_CENTER, 160, 74), place(TA_RIGHT, 160, 74));
    }

    /// Vertical alignment: TOP puts the ascent below y, BASELINE uses y as
    /// the baseline, BOTTOM lifts the run by the descent.
    #[test]
    fn vertical_align_matches_gdi() {
        let tm = TEXTMETRICW { tmAscent: 14, tmDescent: 4, ..Default::default() };
        let baseline = |align: u32, y: i32| match align & TA_VERT_MASK {
            TA_BASELINE => y,
            TA_BOTTOM => y - tm.tmDescent,
            _ => y + tm.tmAscent,
        };
        assert_eq!(baseline(TA_TOP, 100), 114);
        assert_eq!(baseline(TA_BASELINE, 100), 100);
        assert_eq!(baseline(TA_BOTTOM, 100), 96);
    }

    /// A positive lfHeight font (cell height 16 = em 13 + leading 3) must
    /// render at the em size, like upstream, not at the cell height.
    #[test]
    fn em_px_is_height_minus_internal_leading() {
        let tm = TEXTMETRICW { tmHeight: 16, tmInternalLeading: 3, ..Default::default() };
        assert_eq!(em_px(&tm), 13);
        let neg = TEXTMETRICW { tmHeight: 12, tmInternalLeading: 0, ..Default::default() };
        assert_eq!(em_px(&neg), 12);
        let odd = TEXTMETRICW { tmHeight: 0, tmInternalLeading: 0, ..Default::default() };
        assert_eq!(em_px(&odd), 16);
    }
}
