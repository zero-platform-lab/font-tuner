//! A Win32 window that draws text with DirectWrite (IDWriteBitmapRenderTarget::
//! DrawGlyphRun), then BitBlts the result to the window — a controlled
//! injection target for the DirectWrite hook in render-inject.

use core::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::core::{w, Interface};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, EndPaint, InvalidateRect, HDC, LOGFONTW, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_METRICS, DWRITE_GLYPH_METRICS, DWRITE_GLYPH_RUN, DWRITE_MEASURING_MODE_NATURAL,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassW, SetTimer, ShowWindow, TranslateMessage, CW_USEDEFAULT, MSG, SW_SHOW,
    WINDOW_EX_STYLE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

const W: i32 = 560;
const H: i32 = 90;

fn logfont(face: &str, px: i32) -> LOGFONTW {
    let mut lf = LOGFONTW { lfHeight: -px, lfWeight: 400, ..Default::default() };
    for (d, c) in lf.lfFaceName.iter_mut().zip(face.encode_utf16()) {
        *d = c;
    }
    lf
}

/// Draw the sample text with DirectWrite into a bitmap render target, then blit
/// it onto `dst`.
unsafe fn paint_dwrite(dst: HDC) -> windows::core::Result<()> {
    let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let gdi = factory.GetGdiInterop()?;
    let font = gdi.CreateFontFromLOGFONT(&logfont("Yu Gothic UI", 28))?;
    let face = font.CreateFontFace()?;

    let text = "DirectWrite via injection 水面 Rust 0123";
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

    let brt: IDWriteBitmapRenderTarget = gdi.CreateBitmapRenderTarget(None, W as u32, H as u32)?;
    // clear the render target's DC to a pale colour first
    let memdc = brt.GetMemoryDC();
    {
        use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect};
        use windows::Win32::Foundation::RECT;
        let b = CreateSolidBrush(COLORREF(0x00DCF5FA));
        let rc = RECT { left: 0, top: 0, right: W, bottom: H };
        FillRect(memdc, &rc, b);
        let _ = DeleteObject(b.into());
    }
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
    let _ = brt.DrawGlyphRun(24.0, 52.0, DWRITE_MEASURING_MODE_NATURAL, &run, None,
                             COLORREF(0x00_1E_50_C8), None);
    let _ = BitBlt(dst, 0, 0, W, H, Some(memdc), 0, 0, SRCCOPY);
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let _ = paint_dwrite(hdc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_TIMER => {
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn main() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("dwrite-window");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0), class, w!("dwrite-window (DirectWrite target)"),
            WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT, W + 20, H + 40,
            None, None, Some(hinst.into()), None,
        ).unwrap();
        let _ = ShowWindow(hwnd, SW_SHOW);
        SetTimer(Some(hwnd), 1, 400, None);
        let _: *mut c_void = std::ptr::null_mut();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
