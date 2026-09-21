//! Stage 3 of the injection roadmap (render-inject/DESIGN.md): **in-process
//! writeback**. Still a standalone test exe (no injection). It draws text two
//! ways into a 32bpp DIB and saves both to PNG:
//!   * `gdi.png`         — Windows' own ExtTextOutW (control)
//!   * `render-core.png` — our ExtTextOutW hook renders with render-core and
//!                         blits the result into the DIB, skipping GDI.
//!
//! Simplifications (Stage 3a): the font is fixed to Meiryo at a fixed pixel
//! size instead of being resolved from the DC's LOGFONT; the text colour is
//! black on white; only the string path is handled. Full font resolution and
//! the glyph-index path are later stages.

use core::ffi::c_void;
use std::ptr::null_mut;

use minhook::MinHook;
use render_core::render::{render_text, Ink};
use render_core::{tables_for, Ft, Profile, Tables};
use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, ExtTextOutW,
    GetTextMetricsW, SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, ETO_OPTIONS, FONT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION,
    FONT_QUALITY, HDC, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

const W: i32 = 460;
const H: i32 = 64;
const PX: i32 = 26;
const FONT: &str = r"C:\Windows\Fonts\meiryo.ttc";

// ExtTextOutW ABI, handles/pointers as pointer-sized scalars.
type FnEto = unsafe extern "system" fn(
    isize, i32, i32, u32, *const c_void, *const u16, u32, *const i32,
) -> i32;

// State the detour needs (single-threaded test; set before enabling the hook).
static mut ORIG: Option<FnEto> = None;
static mut FT: Option<Ft> = None;
static mut TABLES: Option<Tables> = None;
static mut PROFILE: Option<Profile> = None;
static mut DIB: *mut u8 = null_mut();
static mut ASCENT: i32 = 0;

/// Detour: render the string with render-core and blit into our DIB, then
/// return without calling GDI.
unsafe extern "system" fn detour(
    _hdc: isize, x: i32, y: i32, _options: u32,
    _rect: *const c_void, str_ptr: *const u16, count: u32, _dx: *const i32,
) -> i32 {
    if str_ptr.is_null() || count == 0 {
        return (ORIG.unwrap())(_hdc, x, y, _options, _rect, str_ptr, count, _dx);
    }
    let text = {
        let slice = std::slice::from_raw_parts(str_ptr, count as usize);
        String::from_utf16_lossy(slice)
    };
    let ft = FT.as_ref().unwrap();
    let tables = TABLES.as_ref().unwrap();
    let profile = PROFILE.as_ref().unwrap();

    // Render the whole DIB-sized canvas with the text at the draw origin;
    // baseline = y + ascent so the text cell top lands at y.
    let canvas = render_text(
        ft, tables, profile, Ink::default(), [255, 255, 255],
        &text, PX, (x, y + ASCENT), (W as usize, H as usize),
    );
    // Copy canvas (RGB, top-down) into the DIB (BGRA, top-down).
    let dib = std::slice::from_raw_parts_mut(DIB, (W * H * 4) as usize);
    for i in 0..(W * H) as usize {
        dib[i * 4] = canvas.rgb[i * 3 + 2]; // B
        dib[i * 4 + 1] = canvas.rgb[i * 3 + 1]; // G
        dib[i * 4 + 2] = canvas.rgb[i * 3]; // R
        dib[i * 4 + 3] = 255; // A
    }
    println!("[writeback] rendered {text:?} at ({x},{y}) via render-core");
    1 // skip GDI
}

/// Read the DIB (BGRA top-down) into an RGB PNG.
unsafe fn save_dib(path: &str) {
    let dib = std::slice::from_raw_parts(DIB, (W * H * 4) as usize);
    let mut rgb = vec![0u8; (W * H * 3) as usize];
    for i in 0..(W * H) as usize {
        rgb[i * 3] = dib[i * 4 + 2]; // R
        rgb[i * 3 + 1] = dib[i * 4 + 1]; // G
        rgb[i * 3 + 2] = dib[i * 4]; // B
    }
    let img = image::RgbImage::from_raw(W as u32, H as u32, rgb).unwrap();
    img.save(path).unwrap();
    println!("wrote {path}");
}

unsafe fn fill_white() {
    let dib = std::slice::from_raw_parts_mut(DIB, (W * H * 4) as usize);
    dib.fill(0xFF);
}

fn main() {
    unsafe {
        // 32bpp top-down DIB section as the target surface.
        let screen = CreateCompatibleDC(None);
        let memdc: HDC = CreateCompatibleDC(Some(screen));
        let mut bits: *mut c_void = null_mut();
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: W,
                biHeight: -H, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };
        let hbmp = CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .expect("CreateDIBSection");
        SelectObject(memdc, hbmp.into());
        DIB = bits as *mut u8;
        fill_white();

        // A Meiryo font at PX (negative height = character height), grayscale AA.
        let face = w!("Meiryo");
        let font = CreateFontW(
            -PX, 0, 0, 0, 400, 0, 0, 0,
            FONT_CHARSET(1),               // DEFAULT_CHARSET
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(4),               // ANTIALIASED_QUALITY
            0,
            face,
        );
        SelectObject(memdc, font.into());
        SetBkMode(memdc, TRANSPARENT);
        let _ = SetTextColor(memdc, windows::Win32::Foundation::COLORREF(0x000000));

        let mut tm = TEXTMETRICW::default();
        GetTextMetricsW(memdc, &mut tm);
        ASCENT = tm.tmAscent;

        let sample = "水面に映る Rust 0123";
        let wtext: Vec<u16> = sample.encode_utf16().collect();
        let draw = |dc: HDC| {
            let _ = ExtTextOutW(dc, 12, 14, ETO_OPTIONS(0), None,
                                PCWSTR(wtext.as_ptr()), wtext.len() as u32, None);
        };

        // --- control: GDI's own rendering ---
        draw(memdc);
        save_dib("gdi.png");

        // --- hooked: render-core writeback ---
        fill_white();
        FT = Some(Ft::open(FONT, 0).expect("open font"));
        let p = Profile::clean_greyscale();
        TABLES = Some(tables_for(&p));
        PROFILE = Some(p);

        let gdi32: HMODULE = GetModuleHandleW(w!("gdi32.dll")).expect("gdi32");
        let target = GetProcAddress(gdi32, s!("ExtTextOutW")).expect("addr");
        let tramp = MinHook::create_hook(target as *mut c_void, detour as *mut c_void).expect("hook");
        ORIG = Some(std::mem::transmute::<*mut c_void, FnEto>(tramp));
        MinHook::enable_all_hooks().expect("enable");

        draw(memdc);
        save_dib("render-core.png");

        let _ = MinHook::disable_all_hooks();
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(hbmp.into());
        let _ = DeleteDC(memdc);
        let _ = DeleteDC(screen);
        println!("done.");
    }
}
