//! Safe-ish wrapper over the FreeType C shim (`shim.c`).
//!
//! FreeType does the glyph rasterisation; this module just drives it with the
//! load flags / render mode that MacType's `FreeTypePrepare` selects for a
//! profile. The shim keeps a single process-global library + face, so `Ft` is a
//! non-clonable handle; dropping it tears FreeType down.

use std::ffi::CString;
use std::os::raw::{c_char, c_long};

use crate::config::{Aa, Profile};

#[repr(C)]
struct ShimGlyph {
    width: i32,
    rows: i32,
    pitch: i32,
    pixel_mode: i32,
    left: i32,
    top: i32,
    advance_x: i32,
    buffer: *const u8,
}

extern "C" {
    fn shim_init() -> i32;
    fn shim_open(path: *const c_char, face_index: c_long) -> i32;
    fn shim_reface_memory(data: *const u8, len: c_long, want_family: *const c_char) -> i32;
    fn shim_render(charcode: u32, px: i32, load_flags: i32, render_mode: i32, ex: i32, ey: i32, out: *mut ShimGlyph) -> i32;
    fn shim_render_glyph(glyph_index: u32, px: i32, load_flags: i32, render_mode: i32, ex: i32, ey: i32, out: *mut ShimGlyph) -> i32;
    fn shim_set_lcd_filter(filter: i32) -> i32;
    fn shim_done();
}

// FreeType constants (stable public ABI).
const FT_LOAD_NO_HINTING: i32 = 0x2;
const FT_LOAD_NO_BITMAP: i32 = 0x8;
const FT_LOAD_FORCE_AUTOHINT: i32 = 0x20;
const FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH: i32 = 0x200;
const FT_LOAD_TARGET_NORMAL: i32 = 0;
const FT_LOAD_TARGET_LCD: i32 = 3 << 16;
const FT_RENDER_MODE_NORMAL: i32 = 0;
const FT_RENDER_MODE_LCD: i32 = 3;
/// FreeType `FT_PIXEL_MODE_GRAY`.
pub const PIXEL_MODE_GRAY: i32 = 2;
/// FreeType `FT_PIXEL_MODE_LCD`.
pub const PIXEL_MODE_LCD: i32 = 5;

/// A rendered glyph: coverage bitmap plus placement.
pub struct Glyph<'a> {
    pub width: i32,
    pub rows: i32,
    pub pitch: i32,
    pub pixel_mode: i32,
    pub left: i32,
    pub top: i32,
    pub advance_px: i32, // integer pixels (26.6 >> 6)
    pub buffer: &'a [u8],
}

/// Owned FreeType handle (process-global; only construct one).
pub struct Ft {
    _priv: (),
}

impl Ft {
    /// Initialise FreeType and open a face from a font file.
    pub fn open(path: &str, face_index: i64) -> Result<Ft, i32> {
        unsafe {
            let r = shim_init();
            if r != 0 { return Err(r); }
            let c = CString::new(path).map_err(|_| -1)?;
            let r = shim_open(c.as_ptr(), face_index as c_long);
            if r != 0 { return Err(r); }
        }
        Ok(Ft { _priv: () })
    }

    /// Swap the active face to an in-memory font file (e.g. GDI `GetFontData`
    /// bytes), keeping the FreeType library. For a TTC, `want_family` picks the
    /// matching face by family name. `Ok` on success.
    pub fn reface_memory(&self, data: &[u8], want_family: &str) -> Result<(), i32> {
        let cf = CString::new(want_family).unwrap_or_default();
        let r = unsafe { shim_reface_memory(data.as_ptr(), data.len() as c_long, cf.as_ptr()) };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    fn set_lcd_filter(&self, filter: i32) {
        unsafe { shim_set_lcd_filter(filter); }
    }

    /// FreeType load flags + render mode for a profile's AA + hinting.
    fn flags(p: &Profile) -> (i32, i32) {
        let base = FT_LOAD_NO_BITMAP | FT_LOAD_IGNORE_GLOBAL_ADVANCE_WIDTH;
        let target = if p.aa.is_lcd() { FT_LOAD_TARGET_LCD } else { FT_LOAD_TARGET_NORMAL };
        let render = if p.aa.is_lcd() { FT_RENDER_MODE_LCD } else { FT_RENDER_MODE_NORMAL };
        let mut flags = base | target;
        match p.hinting {
            1 => flags |= FT_LOAD_NO_HINTING,
            2 => flags |= FT_LOAD_FORCE_AUTOHINT,
            _ => {}
        }
        (flags, render)
    }

    /// Prepare the library-global LCD filter for a profile (call before a run).
    pub fn prepare(&self, p: &Profile) {
        if p.aa.is_lcd() {
            self.set_lcd_filter(p.lcd_filter);
        }
    }

    /// Render one character at `px` pixels through `p`. Returns `None` only on
    /// a hard error; a missing/empty glyph yields an empty `Glyph` (advance only).
    pub fn render(&self, ch: char, px: i32, p: &Profile) -> Option<Glyph<'_>> {
        self.emit(p, |flags, mode, ex, ey, out| unsafe {
            shim_render(ch as u32, px, flags, mode, ex, ey, out)
        })
    }

    /// Render a glyph by its font glyph index (for ETO_GLYPH_INDEX draws).
    pub fn render_glyph(&self, gi: u16, px: i32, p: &Profile) -> Option<Glyph<'_>> {
        self.emit(p, |flags, mode, ex, ey, out| unsafe {
            shim_render_glyph(gi as u32, px, flags, mode, ex, ey, out)
        })
    }

    /// Shared body: run `call` (which invokes the right shim entry point) and
    /// wrap the resulting bitmap.
    fn emit(
        &self,
        p: &Profile,
        call: impl FnOnce(i32, i32, i32, i32, *mut ShimGlyph) -> i32,
    ) -> Option<Glyph<'_>> {
        let (flags, render_mode) = Self::flags(p);
        let mut g = ShimGlyph {
            width: 0, rows: 0, pitch: 0, pixel_mode: 0,
            left: 0, top: 0, advance_x: 0, buffer: std::ptr::null(),
        };
        let r = call(flags, render_mode, p.embolden, p.embolden, &mut g);
        if r != 0 || g.buffer.is_null() || g.rows == 0 {
            return Some(Glyph {
                width: 0, rows: 0, pitch: 0, pixel_mode: g.pixel_mode,
                left: g.left, top: g.top, advance_px: g.advance_x >> 6, buffer: &[],
            });
        }
        let len = (g.pitch.abs() * g.rows) as usize;
        let buffer = unsafe { std::slice::from_raw_parts(g.buffer, len) };
        Some(Glyph {
            width: g.width, rows: g.rows, pitch: g.pitch, pixel_mode: g.pixel_mode,
            left: g.left, top: g.top, advance_px: g.advance_x >> 6, buffer,
        })
    }
}

impl Drop for Ft {
    fn drop(&mut self) {
        unsafe { shim_done(); }
    }
}

/// Convenience: does this profile need BGR subpixel order?
pub fn is_bgr(aa: Aa) -> bool {
    matches!(aa, Aa::LcdBgr | Aa::LightLcdBgr)
}
