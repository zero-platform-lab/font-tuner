//! Stage 5b: render intercepted DirectWrite glyph runs with render-core.
//!
//! Builds on 5a: after intercepting IDWriteBitmapRenderTarget::DrawGlyphRun, it
//! extracts the font bytes from the run's IDWriteFontFace, refaces render-core
//! to that font (by face index), renders the glyph run into the render target's
//! memory DC, and skips DirectWrite's own rasteriser. Single process, no
//! injection. Saves the result to dwrite.png as proof.

use core::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ops::Deref;

use render_core::render::{draw_glyphs_onto, Canvas, Ink};
use render_core::{tables_for, Ft, Profile, Tables};
use windows::core::{w, Interface, HRESULT};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS, HDC, LOGFONTW, SRCCOPY,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteFontFile,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_RUN,
    DWRITE_MEASURING_MODE_NATURAL,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};

const W: i32 = 480;
const H: i32 = 80;

type FnDrawGlyphRun = unsafe extern "system" fn(
    *mut c_void, f32, f32, i32, *const DWRITE_GLYPH_RUN, *mut c_void, u32, *mut RECT,
) -> HRESULT;

static mut ORIG: Option<FnDrawGlyphRun> = None;
static mut FT: Option<Ft> = None;
static mut TABLES: Option<Tables> = None;
static mut PROFILE: Option<Profile> = None;
static mut MEMDC: isize = 0;

/// Extract the font-file bytes + face index for a run's font face.
unsafe fn font_bytes(run: &DWRITE_GLYPH_RUN) -> Option<(Vec<u8>, u32)> {
    let face = run.fontFace.deref().as_ref()?;
    let mut n = 0u32;
    face.GetFiles(&mut n, None).ok()?;
    let mut files: Vec<Option<IDWriteFontFile>> = vec![None; n as usize];
    face.GetFiles(&mut n, Some(files.as_mut_ptr())).ok()?;
    let file = files.into_iter().next()??;

    let mut key: *mut c_void = std::ptr::null_mut();
    let mut keysz = 0u32;
    file.GetReferenceKey(&mut key, &mut keysz).ok()?;
    let loader = file.GetLoader().ok()?;
    let stream = loader.CreateStreamFromKey(key as *const c_void, keysz).ok()?;
    let size = stream.GetFileSize().ok()?;
    let mut frag: *mut c_void = std::ptr::null_mut();
    let mut ctx: *mut c_void = std::ptr::null_mut();
    stream.ReadFileFragment(&mut frag, 0, size, &mut ctx).ok()?;
    let bytes = std::slice::from_raw_parts(frag as *const u8, size as usize).to_vec();
    stream.ReleaseFileFragment(ctx);
    Some((bytes, face.GetIndex()))
}

unsafe extern "system" fn detour(
    this: *mut c_void, bx: f32, by: f32, mm: i32,
    run: *const DWRITE_GLYPH_RUN, rp: *mut c_void, color: u32, bbox: *mut RECT,
) -> HRESULT {
    if run.is_null() {
        return (ORIG.unwrap())(this, bx, by, mm, run, rp, color, bbox);
    }
    let r = &*run;
    if render_run(r, bx, by, color).is_some() {
        return HRESULT(0); // S_OK, handled
    }
    (ORIG.unwrap())(this, bx, by, mm, run, rp, color, bbox)
}

unsafe fn render_run(r: &DWRITE_GLYPH_RUN, bx: f32, by: f32, color: u32) -> Option<()> {
    let ft = FT.as_ref()?;
    let tables = TABLES.as_ref()?;
    let profile = PROFILE.as_ref()?;
    let (bytes, index) = font_bytes(r)?;
    ft.reface_memory_index(&bytes, index as i64).ok()?;

    let px = r.fontEmSize.round() as i32;
    let glyphs = std::slice::from_raw_parts(r.glyphIndices, r.glyphCount as usize);
    let ink = Ink { fg: [(color & 0xFF) as u8, ((color >> 8) & 0xFF) as u8, ((color >> 16) & 0xFF) as u8] };

    let hdc = HDC(MEMDC as *mut c_void);
    let memdc = CreateCompatibleDC(Some(hdc));
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: W, biHeight: -H, biPlanes: 1, biBitCount: 32, biCompression: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let old = SelectObject(memdc, hbmp.into());
    let _ = BitBlt(memdc, 0, 0, W, H, Some(hdc), 0, 0, SRCCOPY);

    let dib = std::slice::from_raw_parts_mut(bits as *mut u8, (W * H * 4) as usize);
    let mut canvas = Canvas::from_bgra_topdown(W as usize, H as usize, dib);
    draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px,
                     (bx.round() as i32, by.round() as i32), None);
    canvas.blit_to_bgra_topdown(dib);
    let _ = canvas.save("dwrite.png");

    let _ = BitBlt(hdc, 0, 0, W, H, Some(memdc), 0, 0, SRCCOPY);
    SelectObject(memdc, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(memdc);
    Some(())
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
        FT = Some(Ft::open(r"C:\Windows\Fonts\meiryo.ttc", 0)
            .map_err(|_| windows::core::Error::from_thread())?);
        let p = Profile::clean_greyscale();
        TABLES = Some(tables_for(&p));
        PROFILE = Some(p);

        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let gdi = factory.GetGdiInterop()?;
        let lf = logfont("Yu Gothic UI", 26);
        let font = gdi.CreateFontFromLOGFONT(&lf)?;
        let face = font.CreateFontFace()?;

        let text = "DirectWrite 水 Rust 0123";
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

        let brt: IDWriteBitmapRenderTarget = gdi.CreateBitmapRenderTarget(None, W as u32, H as u32)?;
        MEMDC = brt.GetMemoryDC().0 as isize;

        let obj = brt.as_raw() as *mut *mut usize;
        let vtbl = *obj;
        let slot = vtbl.add(3);
        let mut oldp = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut oldp)?;
        ORIG = Some(std::mem::transmute::<usize, FnDrawGlyphRun>(*slot));
        *slot = detour as usize;
        let _ = VirtualProtect(slot as *const c_void, 8, oldp, &mut oldp);

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
        let _ = brt.DrawGlyphRun(20.0, 44.0, DWRITE_MEASURING_MODE_NATURAL, &run, None,
                                 COLORREF(0x00_1E_50_C8), None);

        println!("DirectWrite glyph run rendered with render-core -> dwrite.png");
        Ok(())
    }
}
