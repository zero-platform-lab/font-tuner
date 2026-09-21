//! Stage 5a of the injection roadmap: intercept DirectWrite text drawing.
//!
//! DirectWrite doesn't go through gdi32!ExtTextOutW, so the GDI hook can't see
//! it. GDI-interop DirectWrite renders glyph runs via
//! `IDWriteBitmapRenderTarget::DrawGlyphRun`. This probe sets one up, patches
//! that method in the object's vtable, draws a run, and logs the intercepted
//! call — proving DirectWrite text can be captured (log-only; rendering with
//! render-core is the next stage). Single process, no injection.

use core::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::core::{w, Interface, HRESULT};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::LOGFONTW;
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_METRICS, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_RUN, DWRITE_MEASURING_MODE_NATURAL,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};

type FnDrawGlyphRun = unsafe extern "system" fn(
    *mut c_void, f32, f32, i32, *const DWRITE_GLYPH_RUN, *mut c_void, u32, *mut RECT,
) -> HRESULT;

static mut ORIG: Option<FnDrawGlyphRun> = None;

unsafe extern "system" fn detour(
    this: *mut c_void, bx: f32, by: f32, mm: i32,
    run: *const DWRITE_GLYPH_RUN, rp: *mut c_void, color: u32, bbox: *mut RECT,
) -> HRESULT {
    if !run.is_null() {
        let r = &*run;
        let n = r.glyphCount.min(8) as usize;
        let idx: Vec<u16> = std::slice::from_raw_parts(r.glyphIndices, n).to_vec();
        println!(
            "[capture] DrawGlyphRun baseline=({bx:.1},{by:.1}) emSize={:.1} glyphs={} first={idx:?} color={color:#08x}",
            r.fontEmSize, r.glyphCount,
        );
    }
    (ORIG.unwrap())(this, bx, by, mm, run, rp, color, bbox)
}

fn logfont(face: &str, px: i32) -> LOGFONTW {
    let mut lf = LOGFONTW { lfHeight: -px, lfWeight: 400, ..Default::default() };
    for (d, c) in lf.lfFaceName.iter_mut().zip(face.encode_utf16()) {
        *d = c;
    }
    lf
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let factory: IDWriteFactory =
            DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let gdi = factory.GetGdiInterop()?;

        let lf = logfont("Yu Gothic UI", 26);
        let font = gdi.CreateFontFromLOGFONT(&lf)?;
        let face = font.CreateFontFace()?;

        // shape a string into glyph indices + advances
        let text = "DirectWrite 水 Rust 0123";
        let codes: Vec<u32> = text.chars().map(|c| c as u32).collect();
        let mut indices = vec![0u16; codes.len()];
        face.GetGlyphIndices(codes.as_ptr(), codes.len() as u32, indices.as_mut_ptr())?;

        let mut fm = DWRITE_FONT_METRICS::default();
        face.GetMetrics(&mut fm);
        let em = 26.0f32;
        let mut gm = vec![DWRITE_GLYPH_METRICS::default(); indices.len()];
        face.GetDesignGlyphMetrics(indices.as_ptr(), indices.len() as u32, gm.as_mut_ptr(), false)?;
        let advances: Vec<f32> = gm
            .iter()
            .map(|m| m.advanceWidth as f32 * em / fm.designUnitsPerEm as f32)
            .collect();

        let brt: IDWriteBitmapRenderTarget = gdi.CreateBitmapRenderTarget(None, 480, 80)?;

        // --- hook DrawGlyphRun (vtable slot 3: after QueryInterface/AddRef/Release) ---
        let obj = brt.as_raw() as *mut *mut usize; // *obj = vtable base
        let vtbl = *obj;
        let slot = vtbl.add(3);
        let mut old = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut old)?;
        ORIG = Some(std::mem::transmute::<usize, FnDrawGlyphRun>(*slot));
        *slot = detour as usize;
        let _ = VirtualProtect(slot as *const c_void, 8, old, &mut old);

        println!("hook installed on IDWriteBitmapRenderTarget::DrawGlyphRun; drawing...\n");

        let run = DWRITE_GLYPH_RUN {
            fontFace: ManuallyDrop::new(Some(face.clone())),
            fontEmSize: em,
            glyphCount: indices.len() as u32,
            glyphIndices: indices.as_ptr(),
            glyphAdvances: advances.as_ptr(),
            glyphOffsets: std::ptr::null(),
            isSideways: false.into(),
            bidiLevel: 0,
        };
        let _ = brt.DrawGlyphRun(
            20.0, 40.0, DWRITE_MEASURING_MODE_NATURAL, &run, None,
            COLORREF(0x00_1E_50_C8), None,
        );

        println!("\ndone. (hook was log-only; render-core rendering is the next stage)");
        Ok(())
    }
}
