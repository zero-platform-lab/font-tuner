#![allow(non_snake_case)] // DLL name RenderCore64 / Win32 export names
//! render-inject — the DLL injected into each process. On load it hooks the
//! text-drawing entry points and renders with `render-core`, replacing Windows'
//! own text rendering inside whatever process this DLL was injected into:
//!   * GDI: gdi32!ExtTextOutW (string + ETO_GLYPH_INDEX; ETO_OPAQUE/CLIPPED/dx).
//!   * DirectWrite bitmap: IDWriteBitmapRenderTarget::DrawGlyphRun (vtbl patch).
//!   * DirectWrite analysis (Chromium/Skia, VS Code): capture the run at
//!     IDWriteFactory::CreateGlyphRunAnalysis, substitute render-core coverage at
//!     IDWriteGlyphRunAnalysis::CreateAlphaTexture.
//!
//! Fonts are cached (re-extracted only on change); rendering is serialised by a
//! mutex. Remaining per RUST-PORT.md: Direct2D ID2D1RenderTarget::DrawGlyphRun
//! and DPI transforms.

use core::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};


use render_core::render::{draw_glyphs_onto, draw_text_onto, Canvas, Ink};
use render_core::{tables_for, Ft};
use windows::core::{PCWSTR, s, w, BOOL};
use windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HMODULE, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetBkColor, GetCurrentObject,
    GetFontData, GetObjectW, GetTextAlign, GetTextColor, GetTextExtentPoint32W, GetTextExtentPointI,
    GetTextMetricsW, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HDC, LOGFONTW,
    OBJ_FONT, SRCCOPY, TEXTMETRICW,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW, GetProcAddress,
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
};
use windows::Win32::System::Threading::{
    CreateMutexW, CreateThread, GetCurrentProcessId, THREAD_CREATION_FLAGS,
};

mod dwrite;
mod d2d;
mod hook;
mod state;
mod profile;
use dwrite::setup_dwrite_hook;
use state::{orig, RenderState, CAPTURED, RENDER};
use d2d::setup_d2d_hook;
use hook::install_hook;
use profile::{load_profile, reload_profile, RELOAD_MSG, RELOAD_MSG_NAME, SELF_HINST};

const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_PROCESS_DETACH: u32 = 0;

type FnEto = unsafe extern "system" fn(
    isize, i32, i32, u32, *const c_void, *const u16, u32, *const i32,
) -> i32;

/// Trampoline to the real ExtTextOutW, published before the detour goes live.
/// `OnceLock` gives set-once semantics and a safe read, so a second attach
/// cannot overwrite it with a pointer to our own jump.
static ORIG: OnceLock<FnEto> = OnceLock::new();

thread_local! {
    // Per-thread re-entrancy guard for the GDI detour. Must be thread-local, not
    // a process-global flag: a global one makes every *other* thread's
    // ExtTextOutW fall back to untuned GDI whenever one thread is mid-render, so
    // multi-window apps render inconsistently. We only need to stop the same
    // thread re-entering (our own GDI calls / nested draws).
    static IN_DETOUR: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}




static LOG_LOCK: Mutex<()> = Mutex::new(());
fn log(msg: &str) {
    if let Some(tmp) = std::env::var_os("TEMP") {
        let _g = LOG_LOCK.lock();
        let path = PathBuf::from(tmp).join("render-inject.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{msg}");
        }
    }
}








/// Resolve the DC's font into render-core (returns the pixel size), or None.
/// Re-extracts + re-faces only when the font differs from `cache`.
unsafe fn resolve_font(hdc: HDC, ft: &Ft, cache: &mut Option<String>) -> Option<i32> {
    let hfont = GetCurrentObject(hdc, OBJ_FONT);
    let mut lf = LOGFONTW::default();
    GetObjectW(hfont, core::mem::size_of::<LOGFONTW>() as i32,
               Some(&mut lf as *mut _ as *mut c_void));
    let px = if lf.lfHeight != 0 { lf.lfHeight.unsigned_abs() as i32 } else { 16 };

    // size-only probe is cheap; the full read only happens on a cache miss.
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
    let face = String::from_utf16_lossy(
        &lf.lfFaceName[..lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(0)],
    );
    let key = format!("gdi:{face}:{size}");
    if cache.as_deref() != Some(key.as_str()) {
        let mut buf = vec![0u8; size as usize];
        GetFontData(hdc, table, 0, Some(buf.as_mut_ptr() as *mut c_void), size);
        ft.reface_memory(&buf, &face).ok()?;
        *cache = Some(key);
    }
    Some(px)
}

unsafe extern "system" fn detour(
    hdc_i: isize, x: i32, y: i32, options: u32,
    rect: *const c_void, str_ptr: *const u16, count: u32, dx: *const i32,
) -> i32 {
    // Guard against re-entrancy (our own GDI calls, or nested draws) on this
    // thread only.
    if IN_DETOUR.with(|f| f.replace(true)) {
        return (orig(&ORIG))(hdc_i, x, y, options, rect, str_ptr, count, dx);
    }
    let r = render_into_dc(hdc_i, x, y, options, rect, str_ptr, count, dx);
    IN_DETOUR.with(|f| f.set(false));
    match r {
        Some(v) => v,
        None => (orig(&ORIG))(hdc_i, x, y, options, rect, str_ptr, count, dx),
    }
}

/// Returns Some(retval) if we handled the draw, None to fall back to GDI.
unsafe fn render_into_dc(hdc_i: isize, x: i32, y: i32, options: u32,
                         rect_ptr: *const c_void, str_ptr: *const u16, count: u32,
                         dx_ptr: *const i32) -> Option<i32> {
    if str_ptr.is_null() || count == 0 {
        return None;
    }
    let mut guard = RENDER.lock().ok()?; // serialises every draw
    let RenderState { ft, tables, profile, font_key } = guard.as_mut()?;
    let hdc = HDC(hdc_i as *mut c_void);

    let px = resolve_font(hdc, ft, font_key)?;

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
    // ExtTextOutW options + optional rectangle (opaque fill / clip).
    const ETO_OPAQUE: u32 = 0x0002;
    const ETO_CLIPPED: u32 = 0x0004;
    let rect = if rect_ptr.is_null() {
        None
    } else {
        let r = *(rect_ptr as *const RECT);
        Some((r.left, r.top, r.right, r.bottom))
    };
    let dx: Option<&[i32]> = if dx_ptr.is_null() {
        None
    } else {
        Some(std::slice::from_raw_parts(dx_ptr, count as usize))
    };

    // Text-extent region, unioned with the rect so opaque fill / clip fit.
    let (mut rx, mut ry) = (x, baseline - tm.tmAscent);
    let (mut right, mut bottom) = (x + sz.cx + 6, ry + tm.tmHeight + 4);
    if let Some((l, t, r, b)) = rect {
        rx = rx.min(l); ry = ry.min(t);
        right = right.max(r); bottom = bottom.max(b);
    }
    let rw = (right - rx).clamp(1, 8192);
    let rh = (bottom - ry).clamp(1, 8192);

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
    // ETO_OPAQUE: fill the rect with the DC's background colour first.
    if let (true, Some((l, t, r, b))) = (options & ETO_OPAQUE != 0, rect) {
        let bk = GetBkColor(hdc).0;
        canvas.fill_rect((l - rx, t - ry, r - rx, b - ry),
                         [(bk & 0xFF) as u8, ((bk >> 8) & 0xFF) as u8, ((bk >> 16) & 0xFF) as u8]);
    }
    // ETO_CLIPPED: restrict drawing to the rect.
    if let (true, Some((l, t, r, b))) = (options & ETO_CLIPPED != 0, rect) {
        canvas.set_clip(Some((l - rx, t - ry, r - rx, b - ry)));
    }
    let pen = (x - rx, baseline - ry);
    if glyph_mode {
        let glyphs = std::slice::from_raw_parts(str_ptr, count as usize);
        draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px, pen, dx);
    } else {
        let s: String = String::from_utf16_lossy(std::slice::from_raw_parts(str_ptr, count as usize));
        draw_text_onto(&mut canvas, ft, tables, profile, ink, &s, px, pen, dx);
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
    // Attach once per *process*, across DLL instances. This image can be mapped
    // into one process more than once (Windows keys module identity by the path
    // it was loaded with, so the WH_GETMESSAGE map and another load can become
    // two instances with separate statics). A second attach would detour
    // ExtTextOutW over our own jump, and retour would build a trampoline from
    // that jump — corrupting the call chain and crashing the host. A named
    // kernel mutex is shared across instances, so the first attach owns it and
    // the rest bail. The handle is leaked on purpose: released only at process
    // exit, keeping the claim for the process's life.
    let guard_name: Vec<u16> = format!("Local\\FontTuner.Attached.{pid}\0").encode_utf16().collect();
    let h = CreateMutexW(None, true, PCWSTR(guard_name.as_ptr()));
    if h.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
        if let Ok(hh) = h { let _ = CloseHandle(hh); }
        return 0;
    }
    let mut buf = [0u16; 260];
    let n = GetModuleFileNameW(None, &mut buf);
    let exe = String::from_utf16_lossy(&buf[..n as usize]);
    log(&format!("loaded into pid={pid} exe={exe}"));

    let Ok(ft) = Ft::open(r"C:\Windows\Fonts\meiryo.ttc", 0) else {
        log("Ft::open failed");
        return 1;
    };
    // Use the active font-tuner profile if present, else the default.
    let (path, p) = load_profile();
    log(&format!("profile {}: {p:?}", path.as_deref().unwrap_or("(default)")));
    if let Ok(mut guard) = RENDER.lock() {
        *guard = Some(RenderState { ft, tables: tables_for(&p), profile: p, font_key: None });
    }
    RELOAD_MSG.store(RegisterWindowMessageW(RELOAD_MSG_NAME), Ordering::Relaxed);

    let Ok(gdi32) = GetModuleHandleW(w!("gdi32.dll")) else { return 1 };
    let Some(target) = GetProcAddress(gdi32, s!("ExtTextOutW")) else { return 1 };
    if let Some(tramp) = install_hook(target as *const (), detour as *const ()) {
        let _ = ORIG.set(std::mem::transmute::<*const (), FnEto>(tramp));
        log("hook installed on ExtTextOutW");
    } else {
        log("ExtTextOutW hook failed");
    }
    setup_dwrite_hook();
    setup_d2d_hook();
    0
}

#[no_mangle]
pub extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            let _ = SELF_HINST.set(hinst.0 as usize);
            // Pin ourselves for the life of the process. This DLL is mapped
            // into every GUI process by the tray's WH_GETMESSAGE hook; when
            // that hook goes away (tray off / exit / MSI upgrade / uninstall)
            // Windows FreeLibrary's us in every one of them at once. If any
            // code of ours can still run afterwards — the on_attach thread
            // still starting up, a thread inside a detour rendering glyphs —
            // that process executes unmapped memory and dies, and so does
            // every other process on the machine. Pinning makes FreeLibrary a
            // no-op, so the hooks simply stay installed until the process
            // exits. Contract: turning font-tuner off, switching profile and
            // upgrading all take effect for processes started afterwards;
            // running processes keep what they have.
            let mut me = HMODULE::default();
            let _ = GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                PCWSTR(DllMain as *const () as *const u16),
                &mut me,
            );
            let _ = CreateThread(None, 0, Some(on_attach), None, THREAD_CREATION_FLAGS(0), None);
        }
    } else if reason == DLL_PROCESS_DETACH {
        // Pinned above, so this only ever runs at process termination. The
        // process is being torn down and will not draw again; touching
        // vtables or the detours (thread suspension) here is pointless and can
        // itself fault. Do nothing, as the C++ core does on termination.
    }
    BOOL(1)
}

/// WH_GETMESSAGE hook procedure. Its only purpose is to make Windows map this
/// DLL into every GUI process that pumps messages (which runs DllMain, which
/// installs our text hooks) — the same auto-injection mechanism the C++ core uses.
#[no_mangle]
pub extern "system" fn GetMsgProc(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, HC_ACTION, MSG, PM_REMOVE};
    unsafe {
        // Tray's "reload profile" broadcast: act once per delivered message
        // (PM_REMOVE only, so a PeekMessage(PM_NOREMOVE) doesn't double up).
        if code == HC_ACTION as i32 && wparam.0 as u32 == PM_REMOVE.0 && !(lparam.0 as *const MSG).is_null() {
            let id = RELOAD_MSG.load(Ordering::Relaxed);
            if id != 0 && (*(lparam.0 as *const MSG)).message == id {
                reload_profile();
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}
