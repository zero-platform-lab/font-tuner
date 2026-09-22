//! An offscreen 32-bit DIB that mirrors a rectangle of a DC: copy the DC's
//! pixels in, let render-core draw on them, copy them back. Owned by RAII so
//! every exit path releases the GDI objects.

use core::ffi::c_void;

use render_core::render::Canvas;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};

use crate::hook::struct_size;

/// A `w`×`h` top-down BGRA DIB selected into its own memory DC.
pub(crate) struct Dib {
    memdc: HDC,
    hbmp: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Dib {
    /// Largest edge we will allocate; anything bigger is not a text run.
    pub(crate) const MAX_EDGE: i32 = 8192;

    /// Create a DIB compatible with `hdc`, `w`×`h` pixels (each clamped to
    /// `1..=MAX_EDGE`). `None` if GDI refuses.
    pub(crate) fn new(hdc: HDC, w: i32, h: i32) -> Option<Dib> {
        let w = w.clamp(1, Self::MAX_EDGE);
        let h = h.clamp(1, Self::MAX_EDGE);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: struct_size::<BITMAPINFOHEADER>(),
                biWidth: w,
                biHeight: -h, // negative = top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = core::ptr::null_mut();
        // SAFETY: plain GDI object creation; on failure the DC created here
        // is deleted before returning, on success all handles go into the
        // struct and are released in `drop`.
        unsafe {
            let memdc = CreateCompatibleDC(Some(hdc));
            let Ok(hbmp) = CreateDIBSection(Some(memdc), &raw const bmi, DIB_RGB_COLORS, &raw mut bits, None, 0) else {
                let _ = DeleteDC(memdc);
                return None;
            };
            let previous = SelectObject(memdc, hbmp.into());
            Some(Dib { memdc, hbmp, previous, bits: bits.cast::<u8>(), w, h })
        }
    }

    /// The pixel buffer, `w * h` BGRA quads, top-down.
    pub(crate) fn pixels(&mut self) -> &mut [u8] {
        let len = usize::try_from(self.w * self.h * 4).unwrap_or(0);
        // SAFETY: `bits` was returned by CreateDIBSection for exactly this
        // size and stays valid until the bitmap is deleted in `drop`.
        unsafe { core::slice::from_raw_parts_mut(self.bits, len) }
    }

    /// A render-core canvas initialised from the pixel buffer. Draw on it,
    /// then `blit` it back.
    pub(crate) fn canvas(&mut self) -> Canvas {
        let (w, h) = (usize::try_from(self.w).unwrap_or(1), usize::try_from(self.h).unwrap_or(1));
        Canvas::from_bgra_topdown(w, h, self.pixels())
    }

    /// Write a canvas made by `canvas()` back into the pixel buffer.
    pub(crate) fn blit(&mut self, canvas: &Canvas) {
        canvas.blit_to_bgra_topdown(self.pixels());
    }

    /// Copy the DC's pixels at (`x`, `y`) into the DIB.
    pub(crate) fn copy_from(&self, hdc: HDC, x: i32, y: i32) {
        // SAFETY: both DCs are live for the duration of the call.
        let _ = unsafe { BitBlt(self.memdc, 0, 0, self.w, self.h, Some(hdc), x, y, SRCCOPY) };
    }

    /// Copy the DIB back onto the DC at (`x`, `y`).
    pub(crate) fn copy_to(&self, hdc: HDC, x: i32, y: i32) {
        // SAFETY: as in `copy_from`.
        let _ = unsafe { BitBlt(hdc, x, y, self.w, self.h, Some(self.memdc), 0, 0, SRCCOPY) };
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: undoing exactly what `new` did, once.
        unsafe {
            SelectObject(self.memdc, self.previous);
            let _ = DeleteObject(self.hbmp.into());
            let _ = DeleteDC(self.memdc);
        }
    }
}
