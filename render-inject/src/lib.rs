#![allow(non_snake_case)] // crate/DLL name RenderCore64 mirrors MacType64.Core
//! render-inject — the DLL injected into each process. Stage 4b: on load it
//! installs a hook on gdi32!ExtTextOutW and, for each text draw, resolves the
//! DC's font, renders the run with `render-core`, and blits the result back
//! into the target DC — replacing Windows' own text rendering, inside whatever
//! process this DLL was injected into.
//!
//! Scope: GDI text (string + ETO_GLYPH_INDEX), grayscale/LCD per the profile,
//! transparent composite over the DC's current content. DirectWrite is not
//! hooked; opaque-fill / clip / dx are not yet carried over from hook-probe.

use core::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use render_core::render::{draw_glyphs_onto, draw_text_onto, Canvas, Ink};
use render_core::{tables_for, Ft, Profile, Tables};
use windows::core::{s, w, BOOL};
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetCurrentObject,
    GetFontData, GetObjectW, GetTextAlign, GetTextColor, GetTextExtentPoint32W, GetTextExtentPointI,
    GetTextMetricsW, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HDC, LOGFONTW,
    OBJ_FONT, SRCCOPY, TEXTMETRICW,
};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::{
    CreateThread, GetCurrentProcessId, THREAD_CREATION_FLAGS,
};

const DLL_PROCESS_ATTACH: u32 = 1;

type FnEto = unsafe extern "system" fn(
    isize, i32, i32, u32, *const c_void, *const u16, u32, *const i32,
) -> i32;

static mut ORIG: Option<FnEto> = None;
static mut FT: Option<Ft> = None;
static mut TABLES: Option<Tables> = None;
static mut PROFILE: Option<Profile> = None;
static IN_DETOUR: AtomicBool = AtomicBool::new(false);
static CAPTURED: AtomicBool = AtomicBool::new(false);

fn log(msg: &str) {
    if let Some(tmp) = std::env::var_os("TEMP") {
        let path = PathBuf::from(tmp).join("render-inject.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// Resolve the DC's font into render-core (returns the pixel size), or None.
unsafe fn resolve_font(hdc: HDC, ft: &Ft) -> Option<i32> {
    let hfont = GetCurrentObject(hdc, OBJ_FONT);
    let mut lf = LOGFONTW::default();
    GetObjectW(hfont, core::mem::size_of::<LOGFONTW>() as i32,
               Some(&mut lf as *mut _ as *mut c_void));
    let px = if lf.lfHeight != 0 { lf.lfHeight.unsigned_abs() as i32 } else { 16 };

    const TTCF: u32 = 0x6663_7474;
    let mut table = TTCF;
    let mut size = GetFontData(hdc, TTCF, 0, None, 0);
    if size == 0 || size == u32::MAX {
        table = 0;
        size = GetFontData(hdc, 0, 0, None, 0);
    }
    if size == 0 || size == u32::MAX {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    GetFontData(hdc, table, 0, Some(buf.as_mut_ptr() as *mut c_void), size);
    let face = String::from_utf16_lossy(
        &lf.lfFaceName[..lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(0)],
    );
    ft.reface_memory(&buf, &face).ok()?;
    Some(px)
}

unsafe extern "system" fn detour(
    hdc_i: isize, x: i32, y: i32, options: u32,
    rect: *const c_void, str_ptr: *const u16, count: u32, dx: *const i32,
) -> i32 {
    // Guard against re-entrancy (our own GDI calls, or nested draws).
    if IN_DETOUR.swap(true, Ordering::SeqCst) {
        return (ORIG.unwrap())(hdc_i, x, y, options, rect, str_ptr, count, dx);
    }
    let r = render_into_dc(hdc_i, x, y, options, str_ptr, count);
    IN_DETOUR.store(false, Ordering::SeqCst);
    match r {
        Some(v) => v,
        None => (ORIG.unwrap())(hdc_i, x, y, options, rect, str_ptr, count, dx),
    }
}

/// Returns Some(retval) if we handled the draw, None to fall back to GDI.
unsafe fn render_into_dc(hdc_i: isize, x: i32, y: i32, options: u32,
                         str_ptr: *const u16, count: u32) -> Option<i32> {
    if str_ptr.is_null() || count == 0 {
        return None;
    }
    let ft = FT.as_ref()?;
    let tables = TABLES.as_ref()?;
    let profile = PROFILE.as_ref()?;
    let hdc = HDC(hdc_i as *mut c_void);

    let px = resolve_font(hdc, ft)?;

    // colour, metrics, baseline
    let color = GetTextColor(hdc).0;
    let ink = Ink { fg: [(color & 0xFF) as u8, ((color >> 8) & 0xFF) as u8, ((color >> 16) & 0xFF) as u8] };
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    const TA_BASELINE: u32 = 24;
    let baseline = if GetTextAlign(hdc).0 & TA_BASELINE == TA_BASELINE { y } else { y + tm.tmAscent };

    // text extent -> region on the DC
    const ETO_GLYPH_INDEX: u32 = 0x0010;
    let glyph_mode = options & ETO_GLYPH_INDEX != 0;
    let run = std::slice::from_raw_parts(str_ptr, count as usize);
    let mut sz = windows::Win32::Foundation::SIZE::default();
    let ok = if glyph_mode {
        GetTextExtentPointI(hdc, run, &mut sz)
    } else {
        GetTextExtentPoint32W(hdc, run, &mut sz)
    };
    if !ok.as_bool() || sz.cx <= 0 {
        return None;
    }
    let rw = sz.cx + 6;
    let rh = tm.tmHeight + 4;
    let rx = x;
    let ry = baseline - tm.tmAscent;

    // offscreen DIB, seeded with the DC's current pixels
    let memdc = CreateCompatibleDC(Some(hdc));
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: rw, biHeight: -rh, biPlanes: 1, biBitCount: 32, biCompression: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbmp = match CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        Ok(h) => h,
        Err(_) => { let _ = DeleteDC(memdc); return None; }
    };
    let old = SelectObject(memdc, hbmp.into());
    let _ = BitBlt(memdc, 0, 0, rw, rh, Some(hdc), rx, ry, SRCCOPY);

    let dib = std::slice::from_raw_parts_mut(bits as *mut u8, (rw * rh * 4) as usize);
    let mut canvas = Canvas::from_bgra_topdown(rw as usize, rh as usize, dib);
    let pen = (x - rx, baseline - ry);
    if glyph_mode {
        let glyphs = std::slice::from_raw_parts(str_ptr, count as usize);
        draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px, pen, None);
    } else {
        let s: String = String::from_utf16_lossy(std::slice::from_raw_parts(str_ptr, count as usize));
        draw_text_onto(&mut canvas, ft, tables, profile, ink, &s, px, pen, None);
    }
    canvas.blit_to_bgra_topdown(dib);

    // Save what render-core produced inside the injected process, once, as proof.
    if !CAPTURED.swap(true, Ordering::SeqCst) {
        if let Some(tmp) = std::env::var_os("TEMP") {
            let p = PathBuf::from(tmp).join("render-inject-capture.png");
            let _ = canvas.save(&p.to_string_lossy());
            log(&format!("captured render-core output to {}", p.display()));
        }
    }

    let _ = BitBlt(hdc, rx, ry, rw, rh, Some(memdc), 0, 0, SRCCOPY);

    SelectObject(memdc, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(memdc);
    Some(1) // handled; skip GDI
}

/// Runs off the loader lock: init render-core and install the hook.
unsafe extern "system" fn on_attach(_p: *mut c_void) -> u32 {
    let pid = GetCurrentProcessId();
    let mut buf = [0u16; 260];
    let n = GetModuleFileNameW(None, &mut buf);
    let exe = String::from_utf16_lossy(&buf[..n as usize]);
    log(&format!("loaded into pid={pid} exe={exe}"));

    let Ok(ft) = Ft::open(r"C:\Windows\Fonts\meiryo.ttc", 0) else {
        log("Ft::open failed");
        return 1;
    };
    FT = Some(ft);
    let p = Profile::clean_greyscale();
    TABLES = Some(tables_for(&p));
    PROFILE = Some(p);

    let Ok(gdi32) = GetModuleHandleW(w!("gdi32.dll")) else { return 1 };
    let Some(target) = GetProcAddress(gdi32, s!("ExtTextOutW")) else { return 1 };
    match minhook::MinHook::create_hook(target as *mut c_void, detour as *mut c_void) {
        Ok(tramp) => {
            ORIG = Some(std::mem::transmute::<*mut c_void, FnEto>(tramp));
            if minhook::MinHook::enable_all_hooks().is_ok() {
                log("hook installed on ExtTextOutW");
            } else {
                log("enable_all_hooks failed");
            }
        }
        Err(e) => log(&format!("create_hook failed: {e:?}")),
    }
    0
}

#[no_mangle]
pub extern "system" fn DllMain(_hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            let _ = CreateThread(None, 0, Some(on_attach), None, THREAD_CREATION_FLAGS(0), None);
        }
    }
    BOOL(1)
}
