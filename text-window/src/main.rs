//! A tiny Win32 window that repeatedly draws text with ExtTextOutW — a
//! controlled, GDI-based injection target for render-inject. Modern apps use
//! DirectWrite; this one deliberately uses classic GDI so the GDI hook applies.

use core::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, ExtTextOutW, FillRect,
    InvalidateRect, SelectObject, SetBkMode, SetTextColor, ETO_OPTIONS, FONT_CHARSET, FONT_CLIP_PRECISION,
    FONT_OUTPUT_PRECISION, FONT_QUALITY, HBRUSH, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    PostQuitMessage, RegisterClassW, SetTimer, ShowWindow, TranslateMessage, CW_USEDEFAULT, MSG,
    SW_SHOW, WINDOW_EX_STYLE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

const SAMPLE: PCWSTR = w!("Injected? 水面に映る Rust 0123 Aa");

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            // pale background
            let brush: HBRUSH = CreateSolidBrush(COLORREF(0x00DCF5FA)); // pale (BGR)
            let rc = RECT { left: 0, top: 0, right: 640, bottom: 120 };
            FillRect(hdc, &rc, brush);
            let _ = DeleteObject(brush.into());
            // Yu Gothic UI 26px, grayscale AA, dark blue text
            let font = CreateFontW(
                -26, 0, 0, 0, 400, 0, 0, 0,
                FONT_CHARSET(1), FONT_OUTPUT_PRECISION(0), FONT_CLIP_PRECISION(0),
                FONT_QUALITY(4), 0, w!("Yu Gothic UI"),
            );
            let old = SelectObject(hdc, font.into());
            SetBkMode(hdc, TRANSPARENT);
            let _ = SetTextColor(hdc, COLORREF(0x00C8501E));
            let s: Vec<u16> = (0..).map_while(|i| {
                let c = unsafe { *SAMPLE.0.add(i) };
                if c == 0 { None } else { Some(c) }
            }).collect();
            let _ = ExtTextOutW(hdc, 20, 44, ETO_OPTIONS(0), None,
                                PCWSTR(s.as_ptr()), s.len() as u32, None);
            SelectObject(hdc, old);
            let _ = DeleteObject(font.into());
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
        let class = w!("text-window");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0), class, w!("text-window (GDI ExtTextOutW target)"),
            WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT, 680, 180,
            None, None, Some(hinst.into()), None,
        ).unwrap();
        let _ = ShowWindow(hwnd, SW_SHOW);
        SetTimer(Some(hwnd), 1, 400, None); // repaint so an injected hook keeps applying

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = hwnd;
        let _: *mut c_void = std::ptr::null_mut();
    }
}
