//! Intercept ID2D1RenderTarget::DrawGlyphRun (vtbl slot 29) — the Direct2D path
//! (WPF, native D2D apps) — and substitute render-core's rendering via the
//! render target's GDI-interop DC. Single process. Saves d2d.png.

use core::ffi::c_void;
use std::mem::ManuallyDrop;

use render_core::render::{draw_glyphs_onto, Canvas, Ink};
use render_core::{tables_for, Ft, Profile, Tables};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1GdiInteropRenderTarget,
    D2D1_DC_INITIALIZE_MODE_COPY, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontFace, IDWriteFontFile,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_RUN,
    DWRITE_MEASURING_MODE_NATURAL,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS, HDC, SRCCOPY,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};
use windows_numerics::Vector2;

const W: i32 = 480;
const H: i32 = 80;

type FnDrawGlyphRun = unsafe extern "system" fn(
    *mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *mut c_void, i32,
);
static mut ORIG: Option<FnDrawGlyphRun> = None;
static mut FT: Option<Ft> = None;
static mut TABLES: Option<Tables> = None;
static mut PROFILE: Option<Profile> = None;

unsafe fn font_bytes(face: &IDWriteFontFace) -> Option<(Vec<u8>, u32)> {
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

unsafe extern "system" fn detour(
    this: *mut c_void, baseline: Vector2, run: *const DWRITE_GLYPH_RUN,
    brush: *mut c_void, measuring: i32,
) {
    if !run.is_null() && substitute(this, baseline, &*run).is_some() {
        println!("[substitute] D2D DrawGlyphRun rendered with render-core");
        return; // skip D2D's own rendering
    }
    (ORIG.unwrap())(this, baseline, run, brush, measuring)
}

unsafe fn substitute(this: *mut c_void, baseline: Vector2, r: &DWRITE_GLYPH_RUN) -> Option<()> {
    let ft = FT.as_ref()?;
    let tables = TABLES.as_ref()?;
    let profile = PROFILE.as_ref()?;
    let face = (*r.fontFace).as_ref()?;
    let (bytes, idx) = font_bytes(face)?;
    ft.reface_memory_index(&bytes, idx as i64).ok()?;
    let px = r.fontEmSize.round() as i32;
    let glyphs = std::slice::from_raw_parts(r.glyphIndices, r.glyphCount as usize);

    let rt = ID2D1DCRenderTarget::from_raw_borrowed(&this)?;
    let gi: ID2D1GdiInteropRenderTarget = rt.cast().ok()?;
    let hdc: HDC = gi.GetDC(D2D1_DC_INITIALIZE_MODE_COPY).ok()?;

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
    draw_glyphs_onto(&mut canvas, ft, tables, profile, Ink::default(), glyphs, px,
                     (baseline.X.round() as i32, baseline.Y.round() as i32), None);
    canvas.blit_to_bgra_topdown(dib);
    let _ = BitBlt(hdc, 0, 0, W, H, Some(memdc), 0, 0, SRCCOPY);
    SelectObject(memdc, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(memdc);
    let _ = gi.ReleaseDC(None);
    Some(())
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let memdc: HDC = CreateCompatibleDC(None);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: W, biHeight: -H, biPlanes: 1, biBitCount: 32, biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        SelectObject(memdc, hbmp.into());

        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
            dpiX: 96.0, dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt: ID2D1DCRenderTarget = d2d.CreateDCRenderTarget(&props)?;
        rt.BindDC(memdc, &RECT { left: 0, top: 0, right: W, bottom: H })?;
        let brush = rt.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.12, g: 0.31, b: 0.78, a: 1.0 }, None)?;

        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let gdi = dw.GetGdiInterop()?;
        let mut lf = windows::Win32::Graphics::Gdi::LOGFONTW { lfHeight: -26, lfWeight: 400, ..Default::default() };
        for (d, c) in lf.lfFaceName.iter_mut().zip("Yu Gothic UI".encode_utf16()) { *d = c; }
        let font = gdi.CreateFontFromLOGFONT(&lf)?;
        let face = font.CreateFontFace()?;
        let text = "Direct2D 水 Rust 0123";
        let codes: Vec<u32> = text.chars().map(|c| c as u32).collect();
        let mut indices = vec![0u16; codes.len()];
        face.GetGlyphIndices(codes.as_ptr(), codes.len() as u32, indices.as_mut_ptr())?;
        let mut fm = DWRITE_FONT_METRICS::default();
        face.GetMetrics(&mut fm);
        let em = 26.0f32;
        let mut gm = vec![DWRITE_GLYPH_METRICS::default(); indices.len()];
        face.GetDesignGlyphMetrics(indices.as_ptr(), indices.len() as u32, gm.as_mut_ptr(), false)?;
        let advances: Vec<f32> = gm.iter().map(|m| m.advanceWidth as f32 * em / fm.designUnitsPerEm as f32).collect();
        let run = DWRITE_GLYPH_RUN {
            fontFace: ManuallyDrop::new(Some(face.clone())),
            fontEmSize: em, glyphCount: indices.len() as u32,
            glyphIndices: indices.as_ptr(), glyphAdvances: advances.as_ptr(),
            glyphOffsets: std::ptr::null(), isSideways: false.into(), bidiLevel: 0,
        };

        FT = Some(Ft::open(r"C:\Windows\Fonts\meiryo.ttc", 0).map_err(|_| windows::core::Error::from_thread())?);
        let p = Profile::clean_sharp();
        TABLES = Some(tables_for(&p));
        PROFILE = Some(p);

        let obj = rt.as_raw() as *mut *mut usize;
        let slot = (*obj).add(29);
        let mut oldp = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut oldp)?;
        ORIG = Some(std::mem::transmute::<usize, FnDrawGlyphRun>(*slot));
        *slot = detour as usize;
        let _ = VirtualProtect(slot as *const c_void, 8, oldp, &mut oldp);
        println!("hook installed on ID2D1RenderTarget::DrawGlyphRun\n");

        rt.BeginDraw();
        rt.Clear(Some(&D2D1_COLOR_F { r: 0.98, g: 0.96, b: 0.86, a: 1.0 }));
        rt.DrawGlyphRun(Vector2 { X: 20.0, Y: 44.0 }, &run, &brush, DWRITE_MEASURING_MODE_NATURAL);
        rt.EndDraw(None, None)?;

        let dib = std::slice::from_raw_parts(bits as *const u8, (W * H * 4) as usize);
        let mut rgb = vec![0u8; (W * H * 3) as usize];
        for i in 0..(W * H) as usize {
            rgb[i * 3] = dib[i * 4 + 2]; rgb[i * 3 + 1] = dib[i * 4 + 1]; rgb[i * 3 + 2] = dib[i * 4];
        }
        image::RgbImage::from_raw(W as u32, H as u32, rgb).unwrap().save("d2d.png").unwrap();
        let _ = DeleteObject(hbmp.into());
        let _ = DeleteDC(memdc);
        println!("\nsaved d2d.png");
        Ok(())
    }
}
