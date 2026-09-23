//! An offscreen 32-bit DIB that mirrors a rectangle of a DC: copy the DC's
//! pixels in, let render-core draw on them, copy them back. Owned by RAII so
//! every exit path releases the GDI objects.

use core::ffi::c_void;

use render_core::render::Canvas;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, RestoreDC, SaveDC, SelectObject,
    SetGraphicsMode, SetMapMode, SetWorldTransform, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GM_COMPATIBLE,
    HBITMAP, HDC, HGDIOBJ, MM_TEXT, SRCCOPY, XFORM,
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

    /// Copy the DC's pixels at device (`x`, `y`) into the DIB.
    pub(crate) fn copy_from(&self, hdc: HDC, x: i32, y: i32) {
        let _guard = DeviceUnits::of(hdc);
        // SAFETY: both DCs are live for the duration of the call.
        let _ = unsafe { BitBlt(self.memdc, 0, 0, self.w, self.h, Some(hdc), x, y, SRCCOPY) };
    }

    /// Copy the DIB back onto the DC at device (`x`, `y`).
    pub(crate) fn copy_to(&self, hdc: HDC, x: i32, y: i32) {
        let _guard = DeviceUnits::of(hdc);
        // SAFETY: as in `copy_from`.
        let _ = unsafe { BitBlt(hdc, x, y, self.w, self.h, Some(self.memdc), 0, 0, SRCCOPY) };
    }
}

/// Puts a DC into device units (MM_TEXT, GM_COMPATIBLE, identity transform)
/// for the life of the guard, and restores everything on drop.
///
/// `BitBlt` takes logical coordinates, so on a DC with a map mode or a world
/// transform it would stretch the DIB instead of placing it pixel for pixel -
/// which is exactly the blur this port used to produce. `SaveDC`/`RestoreDC`
/// is the documented way to put the mapping back untouched, including the
/// bits we never look at.
struct DeviceUnits {
    hdc: HDC,
    saved: i32,
}

impl DeviceUnits {
    fn of(hdc: HDC) -> DeviceUnits {
        // SAFETY: `hdc` is the app's DC, live for the enclosing draw. Every
        // change here is undone in `drop` by the matching RestoreDC.
        let saved = unsafe { SaveDC(hdc) };
        if saved != 0 {
            let identity = XFORM { eM11: 1.0, eM12: 0.0, eM21: 0.0, eM22: 1.0, eDx: 0.0, eDy: 0.0 };
            // SAFETY: as above; the transform is only meaningful in GM_ADVANCED,
            // and SetGraphicsMode back to GM_COMPATIBLE needs it to be identity.
            unsafe {
                let _ = SetWorldTransform(hdc, &raw const identity);
                let _ = SetGraphicsMode(hdc, GM_COMPATIBLE);
                let _ = SetMapMode(hdc, MM_TEXT);
            }
        }
        DeviceUnits { hdc, saved }
    }
}

impl Drop for DeviceUnits {
    fn drop(&mut self) {
        if self.saved != 0 {
            // SAFETY: restoring the state this guard saved on the same DC.
            let _ = unsafe { RestoreDC(self.hdc, self.saved) };
        }
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
