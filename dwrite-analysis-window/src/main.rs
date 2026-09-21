//! A Win32 window that renders text the way Chromium/Skia do: get glyph
//! coverage from DirectWrite via IDWriteGlyphRunAnalysis::CreateAlphaTexture,
//! then composite it itself. An injection target for the analysis-path hook in
//! render-inject.

use core::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateDIBSection, CreateCompatibleDC, DeleteDC, DeleteObject, EndPaint, BitBlt,
    InvalidateRect, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HDC, LOGFONTW,
    PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS,
    DWRITE_GLYPH_METRICS, DWRITE_GLYPH_RUN, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_RENDERING_MODE_NATURAL, DWRITE_TEXTURE_CLEARTYPE_3x1,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassW, SetTimer, ShowWindow, TranslateMessage, CW_USEDEFAULT, MSG, SW_SHOW,
    WINDOW_EX_STYLE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

const W: i32 = 620;
const H: i32 = 90;

fn logfont(face: &str, px: i32) -> LOGFONTW {
    let mut lf = LOGFONTW { lfHeight: -px, lfWeight: 400, ..Default::default() };
    for (d, c) in lf.lfFaceName.iter_mut().zip(face.encode_utf16()) { *d = c; }
    lf
}

unsafe fn paint(dst: HDC) -> windows::core::Result<()> {
    let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let gdi = factory.GetGdiInterop()?;
    let font = gdi.CreateFontFromLOGFONT(&logfont("Yu Gothic UI", 28))?;
    let face = font.CreateFontFace()?;

    let text = "Analysis window 水面 Rust 0123";
    let codes: Vec<u32> = text.chars().map(|c| c as u32).collect();
    let mut indices = vec![0u16; codes.len()];
    face.GetGlyphIndices(codes.as_ptr(), codes.len() as u32, indices.as_mut_ptr())?;
    let mut fm = DWRITE_FONT_METRICS::default();
    face.GetMetrics(&mut fm);
    let em = 28.0f32;
    let mut gm = vec![DWRITE_GLYPH_METRICS::default(); indices.len()];
    face.GetDesignGlyphMetrics(indices.as_ptr(), indices.len() as u32, gm.as_mut_ptr(), false)?;
    let advances: Vec<f32> = gm.iter()
        .map(|m| m.advanceWidth as f32 * em / fm.designUnitsPerEm as f32).collect();

    let (bx, by) = (24.0f32, 52.0f32);
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
        &run, 1.0, None, DWRITE_RENDERING_MODE_NATURAL, DWRITE_MEASURING_MODE_NATURAL, bx, by,
    )?;
    let b = analysis.GetAlphaTextureBounds(DWRITE_TEXTURE_CLEARTYPE_3x1)?;
    let (cw, ch) = ((b.right - b.left) as usize, (b.bottom - b.top) as usize);
    if cw == 0 || ch == 0 { return Ok(()); }
    let mut cov = vec![0u8; cw * ch * 3];
    analysis.CreateAlphaTexture(DWRITE_TEXTURE_CLEARTYPE_3x1, &b, &mut cov)?;

    // Composite the coverage ourselves (black text on pale), into a DIB.
    let memdc = CreateCompatibleDC(Some(dst));
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
    let old = SelectObject(memdc, hbmp.into());
    let dib = std::slice::from_raw_parts_mut(bits as *mut u8, (W * H * 4) as usize);
    for px in dib.chunks_exact_mut(4) { px.copy_from_slice(&[0xFA, 0xF5, 0xDC, 0xFF]); } // pale BGRA
    for row in 0..ch {
        for col in 0..cw {
            let ci = (row * cw + col) * 3;
            let (cr, cg, cb) = (cov[ci], cov[ci + 1], cov[ci + 2]); // subpixel coverage
            let x = b.left + col as i32;
            let y = b.top + row as i32;
            if x < 0 || x >= W || y < 0 || y >= H { continue; }
            let o = (y as usize * W as usize + x as usize) * 4;
            // black text: out = bg * (255 - cov) / 255 per channel (BGRA order)
            dib[o] = (dib[o] as u32 * (255 - cb as u32) / 255) as u8;
            dib[o + 1] = (dib[o + 1] as u32 * (255 - cg as u32) / 255) as u8;
            dib[o + 2] = (dib[o + 2] as u32 * (255 - cr as u32) / 255) as u8;
        }
    }
    let _ = BitBlt(dst, 0, 0, W, H, Some(memdc), 0, 0, SRCCOPY);
    SelectObject(memdc, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(memdc);
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let _ = paint(hdc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_TIMER => { let _ = InvalidateRect(Some(hwnd), None, true); LRESULT(0) }
        WM_DESTROY => { PostQuitMessage(0); LRESULT(0) }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn main() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("dwrite-analysis-window");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0), class, w!("dwrite-analysis-window (GlyphRunAnalysis target)"),
            WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT, W + 20, H + 40,
            None, None, Some(hinst.into()), None,
        ).unwrap();
        let _ = ShowWindow(hwnd, SW_SHOW);
        SetTimer(Some(hwnd), 1, 400, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
