//! Stage 2 of the injection roadmap (render-inject/DESIGN.md): **in-process,
//! log-only** interception of GDI `ExtTextOutW`.
//!
//! This is NOT the injected DLL. It is a standalone test executable that hooks
//! `ExtTextOutW` inside its own process (via MinHook, which works on stable),
//! draws a few strings to a memory DC to trigger the hook, logs the arguments
//! MacType would see, and then calls the real function unchanged. No other
//! process is touched; output is not altered. It proves we can capture and
//! reconstruct the GDI text-draw inputs.

use core::ffi::c_void;
use minhook::MinHook;
use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, ExtTextOutW, ETO_OPTIONS, HDC,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

// ABI-compatible signature of ExtTextOutW (handles/pointers as pointer-sized
// scalars so we need not import the exact newtypes here).
type FnEto = unsafe extern "system" fn(
    isize, i32, i32, u32, *const c_void, *const u16, u32, *const i32,
) -> i32;

// Trampoline to the real ExtTextOutW, set once at install time.
static mut ORIG: Option<FnEto> = None;

/// The detour: log, then call the original unchanged.
unsafe extern "system" fn detour(
    hdc: isize, x: i32, y: i32, options: u32,
    rect: *const c_void, str_ptr: *const u16, count: u32, dx: *const i32,
) -> i32 {
    let text = if str_ptr.is_null() || count == 0 {
        String::new()
    } else {
        let slice = std::slice::from_raw_parts(str_ptr, count as usize);
        String::from_utf16_lossy(slice)
    };
    println!(
        "[capture] ExtTextOutW hdc={:#x} pos=({},{}) opts={:#06x} rect={} count={} dx={} text={:?}",
        hdc, x, y, options,
        if rect.is_null() { "none" } else { "clip" },
        count,
        if dx.is_null() { "none" } else { "spacing" },
        text,
    );
    (ORIG.expect("orig set"))(hdc, x, y, options, rect, str_ptr, count, dx)
}

fn main() {
    unsafe {
        // Resolve gdi32!ExtTextOutW and install the hook.
        let gdi32: HMODULE = GetModuleHandleW(w!("gdi32.dll")).expect("gdi32");
        let target = GetProcAddress(gdi32, s!("ExtTextOutW")).expect("ExtTextOutW addr");

        let trampoline = MinHook::create_hook(target as *mut c_void, detour as *mut c_void)
            .expect("create_hook");
        ORIG = Some(std::mem::transmute::<*mut c_void, FnEto>(trampoline));
        MinHook::enable_all_hooks().expect("enable");

        println!("hook installed on gdi32!ExtTextOutW; drawing test strings...\n");

        // Trigger the hook by drawing to a memory DC (nothing shown on screen).
        let memdc: HDC = CreateCompatibleDC(None);
        for text in ["Hello 世界", "Aa 09 —", "水面に映る Rust"] {
            let wtext: Vec<u16> = text.encode_utf16().collect();
            let _ = ExtTextOutW(
                memdc, 10, 20, ETO_OPTIONS(0), None,
                PCWSTR(wtext.as_ptr()), wtext.len() as u32, None,
            );
        }
        let _ = DeleteDC(memdc);

        println!("\ndone. (hook was log-only; real rendering is a later stage)");
        let _ = MinHook::disable_all_hooks();
    }
}
