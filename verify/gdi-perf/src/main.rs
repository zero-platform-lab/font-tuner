//! Times 500 `ExtTextOutW` calls of one 19-character string (Yu Gothic UI,
//! 24px) on a memory DC, twice, and prints the milliseconds per round.
//!
//! ```text
//! cargo run --release                                  # plain GDI
//! cargo run --release -- "<path>\RenderCore64.dll"     # through the core
//! ```
//!
//! Keep the tray stopped (AGENTS.md: verify with the tray off). The core
//! reads the profile next to the DLL, as when injected.

use windows::core::{w, PCWSTR};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, ExtTextOutW, GetDC, SelectObject, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, ETO_OPTIONS, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::LoadLibraryW;

fn main() {
    if let Some(dll) = std::env::args().nth(1) {
        let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path; loading is the point of the harness.
        let h = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) };
        println!("loaded {dll} -> {h:?}");
        // The core hooks on a background thread off the loader lock.
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    // SAFETY: plain GDI calls on objects created here; single-threaded.
    unsafe {
        let screen = GetDC(None);
        let hdc = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, 600, 100);
        SelectObject(hdc, bmp.into());
        let font = CreateFontW(
            -24, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0,
            w!("Yu Gothic UI"),
        );
        SelectObject(hdc, font.into());
        let text: Vec<u16> = "The quick brown fox".encode_utf16().collect();
        for round in 0..2 {
            let t = std::time::Instant::now();
            for _ in 0..500 {
                let _ = ExtTextOutW(hdc, 10, 10, ETO_OPTIONS(0), None, PCWSTR(text.as_ptr()), text.len() as u32, None);
            }
            println!("round {round}: 500 ExtTextOutW = {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);
        }
    }
}
