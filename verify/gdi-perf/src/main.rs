//! Times 500 `ExtTextOutW` calls of one 19-character string (Yu Gothic UI,
//! 24px) on a memory DC, twice, and prints the milliseconds per round.
//!
//! ```text
//! cargo run --release                                  # plain GDI
//! cargo run --release -- "<path>\RenderCore64.dll"     # through the core
//! ```
//!
//! Keep the tray stopped (AGENTS.md: verify with the tray off). The core
//! reads the profile next to the DLL, as when injected. A second argument
//! writes the drawn bitmap there as a BMP, to see whose rendering was timed.

use windows::core::{w, PCWSTR};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, ExtTextOutW, GetDC, SelectObject, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, ETO_OPTIONS, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::LoadLibraryW;

fn main() {
    let dump = std::env::args().nth(2);
    if let Some(dll) = std::env::args().nth(1).filter(|a| !a.is_empty()) {
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
        if let Some(path) = dump {
            save_bmp(hdc, &path);
        }
    }
}

/// Write the 600x100 memory bitmap as a 24-bit bottom-up BMP.
unsafe fn save_bmp(hdc: windows::Win32::Graphics::Gdi::HDC, path: &str) {
    use windows::Win32::Graphics::Gdi::GetPixel;
    let (w, h) = (600i32, 100i32);
    let row = ((w * 3 + 3) & !3) as usize;
    let mut data = vec![0u8; row * h as usize];
    for y in 0..h {
        for x in 0..w {
            // SAFETY: reading inside the bitmap selected into `hdc`.
            let c = unsafe { GetPixel(hdc, x, y) }.0;
            let o = (h - 1 - y) as usize * row + x as usize * 3;
            data[o] = ((c >> 16) & 0xFF) as u8;
            data[o + 1] = ((c >> 8) & 0xFF) as u8;
            data[o + 2] = (c & 0xFF) as u8;
        }
    }
    let mut f = Vec::with_capacity(54 + data.len());
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + data.len()) as u32).to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&w.to_le_bytes());
    f.extend_from_slice(&h.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    f.extend_from_slice(&[0; 24]);
    f.extend_from_slice(&data);
    let _ = std::fs::write(path, f);
}
