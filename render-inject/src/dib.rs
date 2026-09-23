//! An offscreen 32-bit DIB that mirrors a rectangle of a DC: copy the DC's
//! pixels in, let render-core draw on them, copy them back. Owned by RAII so
//! every exit path releases the GDI objects.
//!
//! Draws on a display DC reuse one DIB per thread, as upstream does
//! (`CBitmapCache` in `CThreadLocalInfo`, `cache.cpp`): creating a memory
//! DC and a DIB section per draw was the largest cost of the GDI path after
//! the glyph cache (measured). The rules, and what they leave behind:
//!
//! * A spare too small is replaced by one covering both sizes.
//! * Every `SHRINK_AFTER` reuses, a spare bigger than the draw is replaced
//!   by one of the draw's size (upstream: `BITMAP_REDUCE_COUNTER` 256), so a
//!   one-off large draw does not stay.
//! * A DIB over `MAX_SPARE_BYTES` is never kept (not in upstream): a huge
//!   draw is freed at once instead of waiting out the counter.
//! * What stays: at most one memory DC and one DIB section (up to
//!   `MAX_SPARE_BYTES`) per thread that has drawn text on a display DC,
//!   freed when the thread exits. The DLL is never unloaded (it pins
//!   itself), so the thread-local destructor always has its code.
//! * Printer and metafile DCs get a DIB of their own each time, compatible
//!   with them, as before.

use core::ffi::c_void;
use std::cell::{Cell, RefCell};

use render_core::render::Canvas;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDeviceCaps, RestoreDC, SaveDC,
    SelectObject, SetGraphicsMode, SetMapMode, SetWorldTransform, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    DT_RASDISPLAY, GM_COMPATIBLE, HBITMAP, HDC, HGDIOBJ, MM_TEXT, SRCCOPY, TECHNOLOGY, XFORM,
};

use crate::hook::struct_size;

/// A top-down BGRA DIB selected into its own memory DC, used as `w`×`h`
/// (its top-left corner when the bitmap is larger: a reused spare).
pub(crate) struct Dib {
    memdc: HDC,
    hbmp: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
    /// The bitmap's real size.
    cap_w: i32,
    cap_h: i32,
    /// Screen-compatible, and handed back to the thread's spare on drop.
    pooled: bool,
}

/// Reuses before an oversized spare is cut down to the draw's size.
const SHRINK_AFTER: u32 = 256;
/// Largest DIB kept as a spare (a 1024 x 1024 BGRA bitmap).
const MAX_SPARE_BYTES: i64 = 4 << 20;

thread_local! {
    /// The thread's spare DIB for display DCs, and its reuses since it was
    /// made.
    static SPARE: RefCell<Option<Dib>> = const { RefCell::new(None) };
    static REUSES: Cell<u32> = const { Cell::new(0) };
}

impl Dib {
    /// Largest edge we will allocate; anything bigger is not a text run.
    pub(crate) const MAX_EDGE: i32 = 8192;

    /// A DIB for `hdc`, `w`×`h` pixels (each clamped to `1..=MAX_EDGE`):
    /// the thread's spare when `hdc` is a display DC (or a memory DC
    /// compatible with one), else a new one compatible with `hdc`. `None`
    /// if GDI refuses.
    pub(crate) fn new(hdc: HDC, w: i32, h: i32) -> Option<Dib> {
        let w = w.clamp(1, Self::MAX_EDGE);
        let h = h.clamp(1, Self::MAX_EDGE);
        // SAFETY: a capability query on the app's DC.
        let display = unsafe { GetDeviceCaps(Some(hdc), TECHNOLOGY) } == DT_RASDISPLAY.cast_signed();
        if !display {
            return Self::create(Some(hdc), w, h, false);
        }
        let spare = SPARE.try_with(|s| s.borrow_mut().take()).ok().flatten();
        let reuses = REUSES.try_with(|r| r.replace(r.get().saturating_add(1))).unwrap_or(0);
        let mut dib = match spare {
            Some(d) if d.cap_w >= w && d.cap_h >= h && (reuses < SHRINK_AFTER || (d.cap_w, d.cap_h) == (w, h)) => d,
            Some(d) => {
                // Too small: cover both. Oversized past the counter: the
                // draw's own size. Either way a fresh count.
                let _ = REUSES.try_with(|r| r.set(0));
                let (cw, ch) = if d.cap_w >= w && d.cap_h >= h { (w, h) } else { (w.max(d.cap_w), h.max(d.cap_h)) };
                drop(d.unpooled());
                Self::create(None, cw, ch, true)?
            }
            None => {
                let _ = REUSES.try_with(|r| r.set(0));
                Self::create(None, w, h, true)?
            }
        };
        if (reuses >= SHRINK_AFTER) && (dib.cap_w, dib.cap_h) == (w, h) {
            let _ = REUSES.try_with(|r| r.set(0));
        }
        dib.w = w;
        dib.h = h;
        Some(dib)
    }

    /// This DIB, freed on drop instead of kept as the spare.
    fn unpooled(mut self) -> Dib {
        self.pooled = false;
        self
    }

    /// A new `w`×`h` DIB section in a memory DC compatible with `hdc` (the
    /// screen when `None`). A pooled one over `MAX_SPARE_BYTES` is made
    /// unpooled.
    fn create(hdc: Option<HDC>, w: i32, h: i32, pooled: bool) -> Option<Dib> {
        let pooled = pooled && i64::from(w) * i64::from(h) * 4 <= MAX_SPARE_BYTES;
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
            let memdc = CreateCompatibleDC(hdc);
            let Ok(hbmp) = CreateDIBSection(Some(memdc), &raw const bmi, DIB_RGB_COLORS, &raw mut bits, None, 0) else {
                let _ = DeleteDC(memdc);
                return None;
            };
            let previous = SelectObject(memdc, hbmp.into());
            Some(Dib { memdc, hbmp, previous, bits: bits.cast::<u8>(), w, h, cap_w: w, cap_h: h, pooled })
        }
    }

    /// The whole bitmap's pixels, `cap_w * cap_h` BGRA quads, top-down.
    fn pixels(&mut self) -> &mut [u8] {
        let len = usize::try_from(self.cap_w * self.cap_h * 4).unwrap_or(0);
        // SAFETY: `bits` was returned by CreateDIBSection for exactly this
        // size and stays valid until the bitmap is deleted in `drop`.
        unsafe { core::slice::from_raw_parts_mut(self.bits, len) }
    }

    /// A render-core canvas initialised from the pixel buffer. Draw on it,
    /// then `blit` it back.
    pub(crate) fn canvas(&mut self) -> Canvas {
        let (w, h) = (usize::try_from(self.w).unwrap_or(1), usize::try_from(self.h).unwrap_or(1));
        let stride = usize::try_from(self.cap_w).unwrap_or(1);
        Canvas::from_bgra_rows(w, h, self.pixels(), stride)
    }

    /// Write a canvas made by `canvas()` back into the pixel buffer.
    pub(crate) fn blit(&mut self, canvas: &Canvas) {
        let stride = usize::try_from(self.cap_w).unwrap_or(1);
        canvas.blit_to_bgra_rows(self.pixels(), stride);
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
        if self.pooled {
            // Hand the handles to a new owner in the thread's spare slot; a
            // spare already there is freed (`pooled` cleared first, so it
            // does not come back here). When the thread is exiting there is
            // no slot, and the new owner is freed instead.
            let mut back = Some(Dib { ..*self });
            let _ = SPARE.try_with(|s| {
                let old = s.borrow_mut().replace(back.take().expect("taken once"));
                if let Some(mut old) = old {
                    old.pooled = false;
                }
            });
            if let Some(mut b) = back {
                b.pooled = false;
            }
            return;
        }
        // SAFETY: undoing exactly what `new` did, once.
        unsafe {
            SelectObject(self.memdc, self.previous);
            let _ = DeleteObject(self.hbmp.into());
            let _ = DeleteDC(self.memdc);
        }
    }
}
