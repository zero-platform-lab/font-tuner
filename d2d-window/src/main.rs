//! A Win32 window that paints a glyph run with Direct2D via
//! ID2D1HwndRenderTarget::DrawGlyphRun. The D2D factory + render target are
//! (re)created inside WM_PAINT, so a DLL injected after the window is already
//! up still sees a fresh D2D1CreateFactory call and can patch the render
//! target's DrawGlyphRun slot. This is the injection target for render-inject's
//! Direct2D path.

use std::cell::RefCell;
use std::mem::ManuallyDrop;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_FEATURE_LEVEL_DEFAULT, D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS,
    DWRITE_GLYPH_METRICS, DWRITE_GLYPH_RUN, DWRITE_MEASURING_MODE_NATURAL,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{InvalidateRect, LOGFONTW, ValidateRect};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
    PostQuitMessage, RegisterClassW, SetTimer, ShowWindow, TranslateMessage,
    CW_USEDEFAULT, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};
use windows_numerics::Vector2;

thread_local! {
    // Keep the DWrite factory + font face across paints (they don't need to be
    // recreated to trigger the D2D hook), but recreate the D2D factory + RT.
    static DWRITE: RefCell<Option<IDWriteFactory>> = RefCell::new(None);
}

fn logfont(face: &str, px: i32) -> LOGFONTW {
    let mut lf = LOGFONTW { lfHeight: -px, lfWeight: 400, ..Default::default() };
    for (d, c) in lf.lfFaceName.iter_mut().zip(face.encode_utf16()) { *d = c; }
    lf
}

unsafe fn paint(hwnd: HWND) -> windows::core::Result<()> {
    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc)?;
    let (w, h) = ((rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32);
    if w == 0 || h == 0 { return Ok(()); }

    // A fresh D2D factory each paint: this is the call render-inject hooks.
    let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
    let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
        dpiX: 96.0, dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
        hwnd,
        pixelSize: windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U { width: w, height: h },
        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
    };
    let rt: ID2D1HwndRenderTarget = d2d.CreateHwndRenderTarget(&rt_props, &hwnd_props)?;
    let brush = rt.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.12, g: 0.31, b: 0.78, a: 1.0 }, None)?;

    // Build a glyph run from a system font.
    let dw: IDWriteFactory = DWRITE.with(|c| {
        if c.borrow().is_none() {
            *c.borrow_mut() = Some(DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).unwrap());
        }
        c.borrow().clone().unwrap()
    });
    let gdi = dw.GetGdiInterop()?;
    let font = gdi.CreateFontFromLOGFONT(&logfont("Yu Gothic UI", 28))?;
    let face = font.CreateFontFace()?;
    let text = "Direct2D 注入テスト 水面 Rust 0123";
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
    let run = DWRITE_GLYPH_RUN {
        fontFace: ManuallyDrop::new(Some(face.clone())),
        fontEmSize: em, glyphCount: indices.len() as u32,
        glyphIndices: indices.as_ptr(), glyphAdvances: advances.as_ptr(),
        glyphOffsets: std::ptr::null(), isSideways: false.into(), bidiLevel: 0,
    };

    rt.BeginDraw();
    rt.Clear(Some(&D2D1_COLOR_F { r: 0.98, g: 0.96, b: 0.86, a: 1.0 }));
    rt.DrawGlyphRun(Vector2 { X: 24.0, Y: 52.0 }, &run, &brush, DWRITE_MEASURING_MODE_NATURAL);
    rt.DrawGlyphRun(Vector2 { X: 24.0, Y: 96.0 }, &run, &brush, DWRITE_MEASURING_MODE_NATURAL);
    let _ = rt.EndDraw(None, None);
    let _ = ValidateRect(Some(hwnd), None);
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => { let _ = paint(hwnd); LRESULT(0) }
        WM_TIMER => { let _ = InvalidateRect(Some(hwnd), None, false); LRESULT(0) }
        WM_DESTROY => { PostQuitMessage(0); LRESULT(0) }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn main() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap();
        let class = w!("d2d-window");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0), class, w!("d2d-window (Direct2D DrawGlyphRun target)"),
            WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT, 680, 200,
            None, None, Some(hinst.into()), None,
        ).unwrap();
        let _ = ShowWindow(hwnd, SW_SHOW);
        // repaint ~2/s so an injected DLL gets a fresh factory to hook.
        SetTimer(Some(hwnd), 1, 500, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
