//! Stage 5d/5e: intercept IDWriteGlyphRunAnalysis::CreateAlphaTexture — the path
//! Chromium/Skia (VS Code, many Electron apps) use to get glyph coverage from
//! DirectWrite's CPU rasteriser — and substitute render-core (FreeType) coverage.
//! Unlike DrawGlyphRun this returns raw alpha coverage the caller composites
//! itself, so we supply only the coverage (FreeType-hinted), not a blended image.
//! Single process. Saves dwrite-alpha.png (DirectWrite) and rendercore-alpha.png.

use core::ffi::c_void;
use std::mem::ManuallyDrop;

use render_core::render::glyph_run_coverage_lcd;
use render_core::{Ft, Profile};
use windows::core::{w, Interface, HRESULT};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::LOGFONTW;
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontFile, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_METRICS, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_RUN, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_RENDERING_MODE_NATURAL, DWRITE_TEXTURE_CLEARTYPE_3x1,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};

type FnCreateAlphaTexture = unsafe extern "system" fn(
    *mut c_void, i32, *const RECT, *mut u8, u32,
) -> HRESULT;

static mut ORIG: Option<FnCreateAlphaTexture> = None;
static mut FT: Option<Ft> = None;
static mut PROFILE: Option<Profile> = None;
static mut GLYPHS: Vec<u16> = Vec::new();
static mut PX: i32 = 26;
static mut BASELINE: (i32, i32) = (0, 0);

/// Substitute render-core's coverage for DirectWrite's.
unsafe extern "system" fn detour(
    this: *mut c_void, tex_type: i32, bounds: *const RECT, alpha: *mut u8, size: u32,
) -> HRESULT {
    if !bounds.is_null() && !alpha.is_null() && tex_type == 1 {
        let b = *bounds;
        let (w, h) = ((b.right - b.left) as usize, (b.bottom - b.top) as usize);
        if w * h * 3 == size as usize {
            if let (Some(ft), Some(profile)) = (FT.as_ref(), PROFILE.as_ref()) {
                let pen = (BASELINE.0 - b.left, BASELINE.1 - b.top);
                let cov = glyph_run_coverage_lcd(ft, profile, &GLYPHS, PX, pen, w, h);
                std::ptr::copy_nonoverlapping(cov.as_ptr(), alpha, size as usize);
                println!("[substitute] filled {size} bytes with render-core coverage ({w}x{h})");
                return HRESULT(0);
            }
        }
    }
    (ORIG.unwrap())(this, tex_type, bounds, alpha, size)
}

unsafe fn font_bytes(face: &windows::Win32::Graphics::DirectWrite::IDWriteFontFace) -> Option<(Vec<u8>, u32)> {
    let mut n = 0u32;
    face.GetFiles(&mut n, None).ok()?;
    let mut files: Vec<Option<IDWriteFontFile>> = vec![None; n as usize];
    face.GetFiles(&mut n, Some(files.as_mut_ptr())).ok()?;
    let file = files.into_iter().next()??;
    let mut key: *mut c_void = std::ptr::null_mut();
    let mut ks = 0u32;
    file.GetReferenceKey(&mut key, &mut ks).ok()?;
    let loader = file.GetLoader().ok()?;
    let stream = loader.CreateStreamFromKey(key as *const c_void, ks).ok()?;
    let sz = stream.GetFileSize().ok()?;
    let mut frag: *mut c_void = std::ptr::null_mut();
    let mut ctx: *mut c_void = std::ptr::null_mut();
    stream.ReadFileFragment(&mut frag, 0, sz, &mut ctx).ok()?;
    let bytes = std::slice::from_raw_parts(frag as *const u8, sz as usize).to_vec();
    stream.ReleaseFileFragment(ctx);
    Some((bytes, face.GetIndex()))
}

fn logfont(face: &str, px: i32) -> LOGFONTW {
    let mut lf = LOGFONTW { lfHeight: -px, lfWeight: 400, ..Default::default() };
    for (d, c) in lf.lfFaceName.iter_mut().zip(face.encode_utf16()) { *d = c; }
    lf
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let gdi = factory.GetGdiInterop()?;
        let font = gdi.CreateFontFromLOGFONT(&logfont("Yu Gothic UI", 26))?;
        let face = font.CreateFontFace()?;

        let text = "Analysis path 水 Rust 0123";
        let codes: Vec<u32> = text.chars().map(|c| c as u32).collect();
        let mut indices = vec![0u16; codes.len()];
        face.GetGlyphIndices(codes.as_ptr(), codes.len() as u32, indices.as_mut_ptr())?;
        let mut fm = DWRITE_FONT_METRICS::default();
        face.GetMetrics(&mut fm);
        let em = 26.0f32;
        let mut gm = vec![DWRITE_GLYPH_METRICS::default(); indices.len()];
        face.GetDesignGlyphMetrics(indices.as_ptr(), indices.len() as u32, gm.as_mut_ptr(), false)?;
        let advances: Vec<f32> = gm.iter()
            .map(|m| m.advanceWidth as f32 * em / fm.designUnitsPerEm as f32).collect();

        // set up render-core with the same font (extract its bytes)
        FT = Some(Ft::open(r"C:\Windows\Fonts\meiryo.ttc", 0).map_err(|_| windows::core::Error::from_thread())?);
        if let Some((bytes, idx)) = font_bytes(&face) {
            let _ = FT.as_ref().unwrap().reface_memory_index(&bytes, idx as i64);
        }
        PROFILE = Some(Profile::clean_sharp()); // LCD
        GLYPHS = indices.clone();
        PX = em.round() as i32;
        BASELINE = (20, 40);

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
        let analysis = factory.CreateGlyphRunAnalysis(
            &run, 1.0, None, DWRITE_RENDERING_MODE_NATURAL, DWRITE_MEASURING_MODE_NATURAL,
            BASELINE.0 as f32, BASELINE.1 as f32,
        )?;

        // capture DirectWrite's coverage first (before hooking)
        let bounds = analysis.GetAlphaTextureBounds(DWRITE_TEXTURE_CLEARTYPE_3x1)?;
        let (w, h) = ((bounds.right - bounds.left) as usize, (bounds.bottom - bounds.top) as usize);
        let mut dw = vec![0u8; w * h * 3];
        analysis.CreateAlphaTexture(DWRITE_TEXTURE_CLEARTYPE_3x1, &bounds, &mut dw)?;
        save_inverted("dwrite-alpha.png", &dw, w, h);

        // hook CreateAlphaTexture (vtable slot 4) and ask again -> render-core fills it
        let obj = analysis.as_raw() as *mut *mut usize;
        let vtbl = *obj;
        let slot = vtbl.add(4);
        let mut oldp = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut oldp)?;
        ORIG = Some(std::mem::transmute::<usize, FnCreateAlphaTexture>(*slot));
        *slot = detour as usize;
        let _ = VirtualProtect(slot as *const c_void, 8, oldp, &mut oldp);
        println!("hook installed on CreateAlphaTexture\n");

        let mut rc = vec![0u8; w * h * 3];
        analysis.CreateAlphaTexture(DWRITE_TEXTURE_CLEARTYPE_3x1, &bounds, &mut rc)?;
        save_inverted("rendercore-alpha.png", &rc, w, h);
        println!("\nsaved dwrite-alpha.png and rendercore-alpha.png ({w}x{h})");
        Ok(())
    }
}

fn save_inverted(path: &str, cov: &[u8], w: usize, h: usize) {
    let inv: Vec<u8> = cov.iter().map(|&v| 255 - v).collect();
    image::RgbImage::from_raw(w as u32, h as u32, inv).unwrap().save(path).unwrap();
}
