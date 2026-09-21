//! Stage 3 of the injection roadmap (render-inject/DESIGN.md): **in-process
//! writeback**. Still a standalone test exe (no injection). It draws text two
//! ways into a 32bpp DIB and saves both to PNG:
//!   * `gdi.png`         — Windows' own ExtTextOutW (control)
//!   * `render-core.png` — our ExtTextOutW hook renders with render-core and
//!                         blits the result into the DIB, skipping GDI.
//!
//! Stage 3b/3c/3d: the font is resolved from the DC (pixel size from the
//! LOGFONT; bytes via GetFontData, 'ttcf' tag for TTCs, matching the family).
//! Both the string and ETO_GLYPH_INDEX paths are handled. The DC's text colour
//! (GetTextColor) and baseline (GetTextMetrics ascent + GetTextAlign) are
//! honoured, and text is composited over the existing surface. Opaque bg fill,
//! clipping rect and dx spacing are later stages.

use core::ffi::c_void;
use std::ptr::null_mut;

use minhook::MinHook;
use render_core::render::{draw_glyphs_onto, draw_text_onto, Canvas, Ink};
use render_core::{tables_for, Ft, Profile, Tables};
use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, ExtTextOutW,
    GetCurrentObject, GetFontData, GetGlyphIndicesW, GetObjectW, GetTextAlign, GetTextColor,
    GetTextMetricsW, SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, ETO_OPTIONS, FONT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION,
    FONT_QUALITY, HDC, LOGFONTW, OBJ_FONT, TEXTMETRICW, TRANSPARENT,
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

    // --- resolve the font from the DC ---
    let hdc = HDC(_hdc as *mut c_void);
    // pixel size from the selected LOGFONT
    let hfont = GetCurrentObject(hdc, OBJ_FONT);
    let mut lf = LOGFONTW::default();
    GetObjectW(hfont, core::mem::size_of::<LOGFONTW>() as i32,
               Some(&mut lf as *mut _ as *mut c_void));
    let px = if lf.lfHeight != 0 { lf.lfHeight.unsigned_abs() as i32 } else { PX };
    // The actual font file bytes GDI is using. For a TrueType Collection,
    // GetFontData with dwTable=0 returns bytes FreeType can't parse; the
    // 'ttcf' tag (0x66637474 as GDI reads it) returns the whole collection.
    const TTCF: u32 = 0x6663_7474;
    let mut table = TTCF;
    let mut size = GetFontData(hdc, TTCF, 0, None, 0);
    if size == 0 || size == u32::MAX {
        table = 0;
        size = GetFontData(hdc, 0, 0, None, 0);
    }
    if size == 0 || size == u32::MAX {
        return (ORIG.unwrap())(_hdc, x, y, _options, _rect, str_ptr, count, _dx);
    }
    let mut buf = vec![0u8; size as usize];
    GetFontData(hdc, table, 0, Some(buf.as_mut_ptr() as *mut c_void), size);
    let face = String::from_utf16_lossy(
        &lf.lfFaceName[..lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(0)],
    );
    if ft.reface_memory(&buf, &face).is_err() {
        return (ORIG.unwrap())(_hdc, x, y, _options, _rect, str_ptr, count, _dx);
    }

    // Honour the DC's text colour and baseline. TA_BASELINE means y is already
    // the baseline; otherwise y is the cell top, so baseline = y + ascent.
    let color = GetTextColor(hdc).0;
    let ink = Ink { fg: [(color & 0xFF) as u8, ((color >> 8) & 0xFF) as u8, ((color >> 16) & 0xFF) as u8] };
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    const TA_BASELINE: u32 = 24;
    let align = GetTextAlign(hdc).0;
    let base_y = if align & TA_BASELINE == TA_BASELINE { y } else { y + tm.tmAscent };

    // Composite over the *existing* DIB content (transparent draw), so text sits
    // on whatever is already there rather than a fresh white fill.
    const ETO_GLYPH_INDEX: u32 = 0x0010;
    let dib = std::slice::from_raw_parts_mut(DIB, (W * H * 4) as usize);
    let mut canvas = Canvas::from_bgra_topdown(W as usize, H as usize, dib);
    if _options & ETO_GLYPH_INDEX != 0 {
        let glyphs = std::slice::from_raw_parts(str_ptr, count as usize);
        draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px, (x, base_y));
    } else {
        draw_text_onto(&mut canvas, ft, tables, profile, ink, &text, px, (x, base_y));
    }
    canvas.blit_to_bgra_topdown(dib);

    let mode = if _options & ETO_GLYPH_INDEX != 0 { "glyph-index" } else { "string" };
    println!("[writeback] {mode} count={count} font={face:?} px={px} color={color:#08x} at ({x},{base_y})");
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

/// Fill the DIB with a solid colour (BGRA).
unsafe fn fill_color(rgb: [u8; 3]) {
    let dib = std::slice::from_raw_parts_mut(DIB, (W * H * 4) as usize);
    for px in dib.chunks_exact_mut(4) {
        px[0] = rgb[2]; px[1] = rgb[1]; px[2] = rgb[0]; px[3] = 0xFF;
    }
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
        fill_color([250, 245, 220]); // pale

        // Use a font OTHER than the old hardcoded Meiryo so that correct
        // resolution from the DC is visible. Yu Gothic UI at PX, grayscale AA.
        let face = w!("Yu Gothic UI");
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
        let _ = SetTextColor(memdc, windows::Win32::Foundation::COLORREF(0x00C8_5A1E)); // RGB(30,90,200) blue

        let sample = "水面に映る Rust 0123";
        let wtext: Vec<u16> = sample.encode_utf16().collect();
        let draw = |dc: HDC| {
            let _ = ExtTextOutW(dc, 12, 14, ETO_OPTIONS(0), None,
                                PCWSTR(wtext.as_ptr()), wtext.len() as u32, None);
        };

        // --- control: GDI's own rendering ---
        draw(memdc);
        save_dib("gdi.png");

        // --- hooked: render-core writeback (composited over the pale bg) ---
        fill_color([250, 245, 220]);
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

        // --- hooked, glyph-index path (as real apps draw) ---
        fill_color([250, 245, 220]);
        let mut gi = vec![0u16; wtext.len()];
        GetGlyphIndicesW(memdc, PCWSTR(wtext.as_ptr()), wtext.len() as i32,
                         gi.as_mut_ptr(), 0);
        let _ = ExtTextOutW(memdc, 12, 14, ETO_OPTIONS(0x0010), None,
                            PCWSTR(gi.as_ptr()), gi.len() as u32, None);
        save_dib("render-core-glyph.png");

        let _ = MinHook::disable_all_hooks();
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(hbmp.into());
        let _ = DeleteDC(memdc);
        let _ = DeleteDC(screen);
        println!("done.");
    }
}
