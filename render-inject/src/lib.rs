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
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering, AtomicU32};
use std::sync::Mutex;

use std::collections::HashMap;

use render_core::render::{draw_glyphs_onto, draw_text_onto, glyph_run_coverage_lcd, Canvas, Ink};
use render_core::{tables_for, Aa, Ft, Profile, Tables};
use windows::core::{PCWSTR, s, w, Interface, BOOL, GUID, HRESULT};
use windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HINSTANCE, HMODULE, RECT};
use windows::Win32::Graphics::Direct2D::{ID2D1Brush, ID2D1GdiInteropRenderTarget, ID2D1RenderTarget, ID2D1SolidColorBrush, D2D1_DC_INITIALIZE_MODE_COPY};
use windows_numerics::Vector2;
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteFontFile, DWRITE_FACTORY_TYPE_SHARED, DWRITE_GLYPH_RUN, DWRITE_MATRIX,
};
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
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};
use windows::Win32::System::Threading::{
    CreateMutexW, CreateThread, GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread,
    SuspendThread, THREAD_CREATION_FLAGS, THREAD_SUSPEND_RESUME,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use retour::RawDetour;

const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_PROCESS_DETACH: u32 = 0;

type FnEto = unsafe extern "system" fn(
    isize, i32, i32, u32, *const c_void, *const u16, u32, *const i32,
) -> i32;

static mut ORIG: Option<FnEto> = None;
static mut FT: Option<Ft> = None;
static mut TABLES: Option<Tables> = None;
static mut PROFILE: Option<Profile> = None;
thread_local! {
    // Per-thread re-entrancy guard for the GDI detour. Must be thread-local, not
    // a process-global flag: a global one makes every *other* thread's
    // ExtTextOutW fall back to untuned GDI whenever one thread is mid-render, so
    // multi-window apps render inconsistently. We only need to stop the same
    // thread re-entering (our own GDI calls / nested draws).
    static IN_DETOUR: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}
/// Serialises the one-time vtable patches (CreateAlphaTexture, D2D factory
/// render-target creation). Without it two threads racing the first call both
/// pass the `is_null()`/`is_none()` check and both patch the slot; the loser
/// then captures the "original" from a slot already holding our detour, so the
/// detour calls itself — infinite recursion, host crash. (The D2D DrawGlyphRun
/// path already guards with D2D_DGR_ORIG's mutex; these paths did not.)
static VTABLE_PATCH_LOCK: Mutex<()> = Mutex::new(());
/// Registered window message the tray broadcasts for "reload profile"
/// (0 until on_attach registers it). Handled in GetMsgProc on the receiving
/// process's own UI thread: no extra thread, no polling.
static RELOAD_MSG: AtomicU32 = AtomicU32::new(0);
const RELOAD_MSG_NAME: PCWSTR = w!("FontTuner.ReloadProfile");
static CAPTURED: AtomicBool = AtomicBool::new(false);
/// Serialises all rendering (one shared FreeType face) and remembers the last
/// font key, so we only re-extract + re-face when the font actually changes.
static RENDER_LOCK: Mutex<Option<String>> = Mutex::new(None);

type FnDrawGlyphRun = unsafe extern "system" fn(
    *mut c_void, f32, f32, i32, *const DWRITE_GLYPH_RUN, *mut c_void, u32, *mut RECT,
) -> HRESULT;
static mut ORIG_DGR: Option<FnDrawGlyphRun> = None;
static mut DGR_SLOT: *mut usize = std::ptr::null_mut();

// DirectWrite glyph-run *analysis* path (Chromium/Skia / VS Code): capture the
// run at CreateGlyphRunAnalysis, substitute coverage at CreateAlphaTexture.
type FnCreateGlyphRunAnalysis = unsafe extern "system" fn(
    *mut c_void, *const DWRITE_GLYPH_RUN, f32, *const DWRITE_MATRIX, i32, i32, f32, f32, *mut *mut c_void,
) -> HRESULT;
type FnCreateAlphaTexture = unsafe extern "system" fn(
    *mut c_void, i32, *const RECT, *mut u8, u32,
) -> HRESULT;
static mut ORIG_CGRA: Option<FnCreateGlyphRunAnalysis> = None;
static mut CGRA_SLOT: *mut usize = std::ptr::null_mut();
static mut ORIG_CAT: Option<FnCreateAlphaTexture> = None;
static mut CAT_SLOT: *mut usize = std::ptr::null_mut();
static ANALYSES: Mutex<Option<HashMap<usize, RunInfo>>> = Mutex::new(None);

struct RunInfo {
    bytes: Vec<u8>,
    index: u32,
    glyphs: Vec<u16>,
    px: i32,
    baseline: (i32, i32),
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

static mut SELF_HINST: HINSTANCE = HINSTANCE(std::ptr::null_mut());

/// Directory this DLL was loaded from (the install dir: font-tuner.ini + ini\).
unsafe fn self_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    let n = GetModuleFileNameW(Some(SELF_HINST.into()), &mut buf);
    if n == 0 { return None; }
    PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])).parent().map(|p| p.to_path_buf())
}

/// The profile the tray selected: `[General] AlternativeFile=ini\<name>.ini`
/// in the install dir's font-tuner.ini, resolved relative to that dir. Read once
/// at attach; a switch takes effect for processes started afterwards. (No
/// live reload on purpose: a watcher thread sleeping inside this DLL wakes
/// up after the bootstrap has unmapped us and crashes the host.)
unsafe fn profile_path() -> Option<String> {
    let dir = self_dir()?;
    let text = std::fs::read_to_string(dir.join("font-tuner.ini")).ok()?;
    let rel = text.lines().map(str::trim)
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == "AlternativeFile")
        .map(|(_, v)| v.trim().to_string())?;
    Some(dir.join(rel).to_string_lossy().into_owned())
}

/// The active profile (path it came from, or None for the built-in default).
unsafe fn load_profile() -> (Option<String>, Profile) {
    let path = profile_path();
    let p = path.as_deref().and_then(Profile::from_ini).unwrap_or_else(Profile::clean_greyscale);
    (path, p)
}

/// Re-read font-tuner.ini and swap the profile + tables in, under RENDER_LOCK
/// so no draw observes a half-updated pair. Called on this process's UI
/// thread from GetMsgProc when the tray broadcasts RELOAD_MSG. If the ini is
/// unreadable the built-in default applies, same as at attach.
unsafe fn reload_profile() {
    let (path, p) = load_profile();
    if let Ok(_guard) = RENDER_LOCK.lock() {
        TABLES = Some(tables_for(&p));
        PROFILE = Some(p);
    }
    log(&format!("reloaded profile {}", path.as_deref().unwrap_or("(default)")));
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
        return (ORIG.unwrap())(hdc_i, x, y, options, rect, str_ptr, count, dx);
    }
    let r = render_into_dc(hdc_i, x, y, options, rect, str_ptr, count, dx);
    IN_DETOUR.with(|f| f.set(false));
    match r {
        Some(v) => v,
        None => (ORIG.unwrap())(hdc_i, x, y, options, rect, str_ptr, count, dx),
    }
}

/// Returns Some(retval) if we handled the draw, None to fall back to GDI.
unsafe fn render_into_dc(hdc_i: isize, x: i32, y: i32, options: u32,
                         rect_ptr: *const c_void, str_ptr: *const u16, count: u32,
                         dx_ptr: *const i32) -> Option<i32> {
    if str_ptr.is_null() || count == 0 {
        return None;
    }
    let mut cache = RENDER_LOCK.lock().ok()?; // serialise + remember the last font
    let ft = FT.as_ref()?;
    let tables = TABLES.as_ref()?;
    let profile = PROFILE.as_ref()?;
    let hdc = HDC(hdc_i as *mut c_void);

    let px = resolve_font(hdc, ft, &mut cache)?;

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

// ---- DirectWrite (IDWriteBitmapRenderTarget::DrawGlyphRun) ----

/// Extract the font-file bytes + face index for a run's font face.
unsafe fn dwrite_font_bytes(run: &DWRITE_GLYPH_RUN) -> Option<(Vec<u8>, u32)> {
    let face = run.fontFace.deref().as_ref()?;
    let mut n = 0u32;
    face.GetFiles(&mut n, None).ok()?;
    let mut files: Vec<Option<IDWriteFontFile>> = vec![None; n as usize];
    face.GetFiles(&mut n, Some(files.as_mut_ptr())).ok()?;
    let file = files.into_iter().next()??;
    let mut key: *mut c_void = std::ptr::null_mut();
    let mut keysz = 0u32;
    file.GetReferenceKey(&mut key, &mut keysz).ok()?;
    let loader = file.GetLoader().ok()?;
    let stream = loader.CreateStreamFromKey(key as *const c_void, keysz).ok()?;
    let size = stream.GetFileSize().ok()?;
    let mut frag: *mut c_void = std::ptr::null_mut();
    let mut ctx: *mut c_void = std::ptr::null_mut();
    stream.ReadFileFragment(&mut frag, 0, size, &mut ctx).ok()?;
    let bytes = std::slice::from_raw_parts(frag as *const u8, size as usize).to_vec();
    stream.ReleaseFileFragment(ctx);
    Some((bytes, face.GetIndex()))
}

unsafe extern "system" fn dgr_detour(
    this: *mut c_void, bx: f32, by: f32, mm: i32,
    run: *const DWRITE_GLYPH_RUN, rp: *mut c_void, color: u32, bbox: *mut RECT,
) -> HRESULT {
    if !run.is_null() && dgr_render(this, &*run, bx, by, color).is_some() {
        return HRESULT(0); // S_OK
    }
    (ORIG_DGR.unwrap())(this, bx, by, mm, run, rp, color, bbox)
}

unsafe fn dgr_render(this: *mut c_void, r: &DWRITE_GLYPH_RUN, bx: f32, by: f32, color: u32) -> Option<()> {
    let mut cache = RENDER_LOCK.lock().ok()?; // serialise + remember the last font
    let ft = FT.as_ref()?;
    let tables = TABLES.as_ref()?;
    let profile = PROFILE.as_ref()?;
    let brt = IDWriteBitmapRenderTarget::from_raw_borrowed(&this)?;
    let hdc = brt.GetMemoryDC();
    let size = brt.GetSize().ok()?;
    let (w, h) = (size.cx, size.cy);
    if w <= 0 || h <= 0 { return None; }

    // key on the font-face identity; only re-extract on a miss.
    let face = r.fontFace.deref().as_ref()?;
    let key = format!("dw:{:x}:{}", face.as_raw() as usize, face.GetIndex());
    if cache.as_deref() != Some(key.as_str()) {
        let (bytes, index) = dwrite_font_bytes(r)?;
        ft.reface_memory_index(&bytes, index as i64).ok()?;
        *cache = Some(key);
    }
    let px = r.fontEmSize.round() as i32;
    let glyphs = std::slice::from_raw_parts(r.glyphIndices, r.glyphCount as usize);
    let ink = Ink { fg: [(color & 0xFF) as u8, ((color >> 8) & 0xFF) as u8, ((color >> 16) & 0xFF) as u8] };

    let memdc = CreateCompatibleDC(Some(hdc));
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, biCompression: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let old = SelectObject(memdc, hbmp.into());
    let _ = BitBlt(memdc, 0, 0, w, h, Some(hdc), 0, 0, SRCCOPY);
    let dib = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
    let mut canvas = Canvas::from_bgra_topdown(w as usize, h as usize, dib);
    draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px,
                     (bx.round() as i32, by.round() as i32), None);
    canvas.blit_to_bgra_topdown(dib);
    if !CAPTURED.swap(true, Ordering::SeqCst) {
        if let Some(tmp) = std::env::var_os("TEMP") {
            let p = PathBuf::from(tmp).join("render-inject-dwrite.png");
            let _ = canvas.save(&p.to_string_lossy());
            log(&format!("captured DirectWrite render to {}", p.display()));
        }
    }
    let _ = BitBlt(hdc, 0, 0, w, h, Some(memdc), 0, 0, SRCCOPY);
    SelectObject(memdc, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(memdc);
    Some(())
}

/// Patch DrawGlyphRun in the shared IDWriteBitmapRenderTarget vtable (slot 3),
/// so every render target in this process routes through us.
unsafe fn setup_dwrite_hook() {
    let Ok(factory) = DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED) else {
        log("dwrite factory failed"); return;
    };
    let Ok(gdi) = factory.GetGdiInterop() else { return };
    let Ok(brt) = gdi.CreateBitmapRenderTarget(None, 8, 8) else { return };
    let obj = brt.as_raw() as *mut *mut usize;
    let vtbl = *obj;
    let slot = vtbl.add(3);
    let mut oldp = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut oldp).is_err() {
        return;
    }
    ORIG_DGR = Some(std::mem::transmute::<usize, FnDrawGlyphRun>(*slot));
    DGR_SLOT = slot;
    *slot = dgr_detour as usize;
    let _ = VirtualProtect(slot as *const c_void, 8, oldp, &mut oldp);
    log("hook installed on DrawGlyphRun");

    // Patch IDWriteFactory::CreateGlyphRunAnalysis (vtbl slot 23) for the
    // analysis/coverage path (Chromium/Skia). The vtable is shared, so this
    // covers the app's own factory too.
    let fslot = (*(factory.as_raw() as *mut *mut usize)).add(23);
    let mut fp = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(fslot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut fp).is_ok() {
        ORIG_CGRA = Some(std::mem::transmute::<usize, FnCreateGlyphRunAnalysis>(*fslot));
        CGRA_SLOT = fslot;
        *fslot = cgra_detour as usize;
        let _ = VirtualProtect(fslot as *const c_void, 8, fp, &mut fp);
        log("hook installed on CreateGlyphRunAnalysis");
    }
}

// CreateGlyphRunAnalysis: create the analysis, capture its run, patch its
// CreateAlphaTexture slot the first time.
unsafe extern "system" fn cgra_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, ppd: f32, transform: *const DWRITE_MATRIX,
    rmode: i32, mmode: i32, bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    let hr = (ORIG_CGRA.unwrap())(this, run, ppd, transform, rmode, mmode, bx, by, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() && !run.is_null() {
        let r = &*run;
        if let Some((bytes, index)) = dwrite_font_bytes(r) {
            let glyphs = std::slice::from_raw_parts(r.glyphIndices, r.glyphCount as usize).to_vec();
            let info = RunInfo { bytes, index, glyphs, px: r.fontEmSize.round() as i32,
                                 baseline: (bx.round() as i32, by.round() as i32) };
            if let Ok(mut m) = ANALYSES.lock() {
                let map = m.get_or_insert_with(HashMap::new);
                // Bound the map: analysis objects that are never followed by a
                // CreateAlphaTexture (so never evicted below) would otherwise
                // leak an entry each. If it grows past the cap, drop everything;
                // in-flight analyses then fall back to untuned rendering — a
                // one-off visual blip, never a crash or unbounded growth.
                if map.len() >= 4096 { map.clear(); }
                map.insert(*out as usize, info);
            }
            patch_cat_vtable(*out);
        }
    }
    hr
}

unsafe fn patch_cat_vtable(analysis: *mut c_void) {
    if !CAT_SLOT.is_null() { return; } // fast path, no lock once patched
    let _guard = VTABLE_PATCH_LOCK.lock();
    if !CAT_SLOT.is_null() { return; } // re-check under the lock
    let vtbl = *(analysis as *mut *mut usize);
    let slot = vtbl.add(4);
    let mut oldp = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut oldp).is_err() { return; }
    ORIG_CAT = Some(std::mem::transmute::<usize, FnCreateAlphaTexture>(*slot));
    *slot = cat_detour as usize;
    // Publish CAT_SLOT last: cat_detour keys off ORIG_CAT, and another thread's
    // fast-path check keys off CAT_SLOT, so ORIG_CAT must be set before CAT_SLOT.
    CAT_SLOT = slot;
    let _ = VirtualProtect(slot as *const c_void, 8, oldp, &mut oldp);
    log("hook installed on CreateAlphaTexture");
}

unsafe extern "system" fn cat_detour(
    this: *mut c_void, tex_type: i32, bounds: *const RECT, alpha: *mut u8, size: u32,
) -> HRESULT {
    if tex_type == 1 && !bounds.is_null() && !alpha.is_null() && cat_fill(this, &*bounds, alpha, size).is_some() {
        // The analysis has produced its texture; drop its captured run so the
        // map does not grow for the life of the process.
        if let Ok(mut m) = ANALYSES.lock() {
            if let Some(map) = m.as_mut() { map.remove(&(this as usize)); }
        }
        return HRESULT(0);
    }
    (ORIG_CAT.unwrap())(this, tex_type, bounds, alpha, size)
}

unsafe fn cat_fill(this: *mut c_void, b: &RECT, alpha: *mut u8, size: u32) -> Option<()> {
    let (w, h) = ((b.right - b.left) as usize, (b.bottom - b.top) as usize);
    if w == 0 || h == 0 || w * h * 3 != size as usize { return None; }
    let mut cache = RENDER_LOCK.lock().ok()?;
    let ft = FT.as_ref()?;
    let profile = PROFILE.as_ref()?;
    let m = ANALYSES.lock().ok()?;
    let info = m.as_ref()?.get(&(this as usize))?;
    // Key on the font identity (face index + file length), not the analysis
    // object address: addresses are recycled, so keying on `this` would reuse a
    // stale face when a freed analysis's pointer is handed to a different font.
    let key = format!("dwa:{}:{}", info.index, info.bytes.len());
    if cache.as_deref() != Some(key.as_str()) {
        ft.reface_memory_index(&info.bytes, info.index as i64).ok()?;
        *cache = Some(key);
    }
    // force LCD subpixel for the CLEARTYPE_3x1 texture, keep the profile's hinting
    let lcd = Profile { aa: Aa::LcdRgb, ..*profile };
    let pen = (info.baseline.0 - b.left, info.baseline.1 - b.top);
    let cov = glyph_run_coverage_lcd(ft, &lcd, &info.glyphs, info.px, pen, w, h);
    if !CAPTURED.swap(true, Ordering::SeqCst) {
        if let Some(tmp) = std::env::var_os("TEMP") {
            let inv: Vec<u8> = cov.iter().map(|&v| 255 - v).collect();
            let _ = inv;
            let p = PathBuf::from(tmp).join("render-inject-analysis.txt");
            let _ = std::fs::write(p, format!("analysis substituted: {w}x{h}, glyphs={}", info.glyphs.len()));
            log("substituted CreateAlphaTexture via render-core");
        }
    }
    std::ptr::copy_nonoverlapping(cov.as_ptr(), alpha, size as usize);
    Some(())
}

// ---- Direct2D (ID2D1RenderTarget::DrawGlyphRun) ----
type FnD2DCreateFactory = unsafe extern "system" fn(i32, *const GUID, *const c_void, *mut *mut c_void) -> HRESULT;
type FnCreateDCRT = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
type FnCreateHwndRT = unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
type FnD2DDrawGlyphRun = unsafe extern "system" fn(*mut c_void, Vector2, *const DWRITE_GLYPH_RUN, *mut c_void, i32);
static mut ORIG_D2DCF: Option<FnD2DCreateFactory> = None;
static mut ORIG_DCRT: Option<FnCreateDCRT> = None;
static mut DCRT_SLOT: *mut usize = std::ptr::null_mut();
static mut ORIG_HWNDRT: Option<FnCreateHwndRT> = None;
static mut HWNDRT_SLOT: *mut usize = std::ptr::null_mut();
static D2D_DGR_ORIG: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None); // rt vtable -> orig DrawGlyphRun
static D2D_CAPTURED: AtomicBool = AtomicBool::new(false); // log the first D2D substitution once

unsafe fn patch_slot(slot: *mut usize, newv: usize) -> Option<usize> {
    let mut oldp = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut oldp).is_err() { return None; }
    let old = *slot;
    *slot = newv;
    let _ = VirtualProtect(slot as *const c_void, 8, oldp, &mut oldp);
    Some(old)
}

unsafe extern "system" fn d2dcf_detour(ftype: i32, riid: *const GUID, opts: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let hr = (ORIG_D2DCF.unwrap())(ftype, riid, opts, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() {
        let vtbl = *(*out as *mut *mut usize);
        // ID2D1Factory: CreateHwndRenderTarget = slot 14, CreateDCRenderTarget = slot 16.
        // Serialise the one-time patch: two threads creating factories at once
        // must not both capture the "original" (see VTABLE_PATCH_LOCK).
        let _guard = VTABLE_PATCH_LOCK.lock();
        if ORIG_HWNDRT.is_none() {
            if let Some(o) = patch_slot(vtbl.add(14), create_hwnd_detour as usize) { HWNDRT_SLOT = vtbl.add(14); ORIG_HWNDRT = Some(std::mem::transmute(o)); }
        }
        if ORIG_DCRT.is_none() {
            if let Some(o) = patch_slot(vtbl.add(16), create_dc_detour as usize) { DCRT_SLOT = vtbl.add(16); ORIG_DCRT = Some(std::mem::transmute(o)); }
        }
        log("hook installed on D2D1Factory render-target creation");
    }
    hr
}

unsafe extern "system" fn create_dc_detour(this: *mut c_void, props: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let hr = (ORIG_DCRT.unwrap())(this, props, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { patch_rt_dgr(*out); }
    hr
}
unsafe extern "system" fn create_hwnd_detour(this: *mut c_void, p1: *const c_void, p2: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let hr = (ORIG_HWNDRT.unwrap())(this, p1, p2, out);
    if hr.is_ok() && !out.is_null() && !(*out).is_null() { patch_rt_dgr(*out); }
    hr
}

unsafe fn patch_rt_dgr(rt: *mut c_void) {
    let vtbl = *(rt as *mut *mut usize);
    let vptr = vtbl as usize;
    let Ok(mut m) = D2D_DGR_ORIG.lock() else { return };
    let map = m.get_or_insert_with(HashMap::new);
    if map.contains_key(&vptr) { return; }
    if let Some(old) = patch_slot(vtbl.add(29), d2d_dgr_detour as usize) {
        map.insert(vptr, old);
        log("hook installed on D2D DrawGlyphRun");
    }
}

unsafe extern "system" fn d2d_dgr_detour(this: *mut c_void, baseline: Vector2, run: *const DWRITE_GLYPH_RUN, brush: *mut c_void, measuring: i32) {
    if !run.is_null() && d2d_substitute(this, baseline, &*run, brush).is_some() {
        return;
    }
    let vptr = *(this as *const usize); // this's vtable pointer
    let orig = D2D_DGR_ORIG.lock().ok().and_then(|m| m.as_ref().and_then(|map| map.get(&vptr).copied()));
    if let Some(o) = orig {
        let f: FnD2DDrawGlyphRun = std::mem::transmute(o);
        f(this, baseline, run, brush, measuring);
    }
}

/// Read the run's ink color from the D2D brush. D2D DrawGlyphRun paints with
/// the given brush, so mirroring the GDI/DWrite paths means honoring it — a
/// solid-color brush yields its RGB; anything else falls back to black. Colors
/// are premultiplied-free sRGB floats in 0..1.
unsafe fn d2d_brush_ink(brush: *mut c_void) -> Ink {
    if brush.is_null() { return Ink::default(); }
    let Some(b) = ID2D1Brush::from_raw_borrowed(&brush) else { return Ink::default() };
    let Ok(scb) = b.cast::<ID2D1SolidColorBrush>() else { return Ink::default() };
    let c = scb.GetColor();
    let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    Ink { fg: [to8(c.r), to8(c.g), to8(c.b)] }
}

unsafe fn d2d_substitute(this: *mut c_void, baseline: Vector2, r: &DWRITE_GLYPH_RUN, brush: *mut c_void) -> Option<()> {
    let mut cache = RENDER_LOCK.lock().ok()?;
    let ft = FT.as_ref()?;
    let tables = TABLES.as_ref()?;
    let profile = PROFILE.as_ref()?;
    let (bytes, index) = dwrite_font_bytes(r)?;
    let key = format!("d2d:{index}:{}", bytes.len());
    if cache.as_deref() != Some(key.as_str()) {
        ft.reface_memory_index(&bytes, index as i64).ok()?;
        *cache = Some(key);
    }
    let px = r.fontEmSize.round() as i32;
    let glyphs = std::slice::from_raw_parts(r.glyphIndices, r.glyphCount as usize);
    let adv: f32 = if r.glyphAdvances.is_null() { 0.0 }
        else { std::slice::from_raw_parts(r.glyphAdvances, r.glyphCount as usize).iter().sum() };

    let rt = ID2D1RenderTarget::from_raw_borrowed(&this)?;
    let gi: ID2D1GdiInteropRenderTarget = rt.cast().ok()?;
    let hdc = gi.GetDC(D2D1_DC_INITIALIZE_MODE_COPY).ok()?;

    // region around the text baseline
    let bx = baseline.X.round() as i32;
    let by = baseline.Y.round() as i32;
    let rw = (adv.ceil() as i32 + px).clamp(1, 8192);
    let rh = (px * 2).clamp(1, 8192);
    let rx = bx;
    let ry = by - px - px / 4;

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
    let hbmp = CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let old = SelectObject(memdc, hbmp.into());
    let _ = BitBlt(memdc, 0, 0, rw, rh, Some(hdc), rx, ry, SRCCOPY);
    let dib = std::slice::from_raw_parts_mut(bits as *mut u8, (rw * rh * 4) as usize);
    let mut canvas = Canvas::from_bgra_topdown(rw as usize, rh as usize, dib);
    let ink = d2d_brush_ink(brush);
    draw_glyphs_onto(&mut canvas, ft, tables, profile, ink, glyphs, px, (bx - rx, by - ry), None);
    canvas.blit_to_bgra_topdown(dib);
    let _ = BitBlt(hdc, rx, ry, rw, rh, Some(memdc), 0, 0, SRCCOPY);
    SelectObject(memdc, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(memdc);
    let _ = gi.ReleaseDC(None);
    if !D2D_CAPTURED.swap(true, Ordering::SeqCst) {
        log(&format!("substituted D2D DrawGlyphRun via render-core ({} glyphs, {px}px)", glyphs.len()));
    }
    Some(())
}

/// Installed inline detours, kept alive for the life of the process (the DLL
/// pins itself, so they are never disabled). retour patches the target's first
/// bytes non-atomically and does NOT stop other threads while doing so, so a
/// thread executing inside those bytes at that instant would fault. We suspend
/// every other thread in this process around the patch — the same window
/// MinHook closes internally — then leak the detour so it stays enabled.
static DETOURS: Mutex<Vec<RawDetour>> = Mutex::new(Vec::new());

/// Suspend all threads in this process except the caller, for the duration of
/// the returned guard. Resumed (in reverse) on drop. Best-effort: threads that
/// cannot be opened/suspended are skipped.
struct FrozenThreads(Vec<isize>);
impl FrozenThreads {
    unsafe fn all_but_current() -> FrozenThreads {
        let pid = GetCurrentProcessId();
        let me = GetCurrentThreadId();
        let mut handles = Vec::new();
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
            let mut e = THREADENTRY32 { dwSize: core::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
            if Thread32First(snap, &mut e).is_ok() {
                loop {
                    if e.th32OwnerProcessID == pid && e.th32ThreadID != me {
                        if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, e.th32ThreadID) {
                            // SuspendThread returns (DWORD)-1 on failure.
                            if SuspendThread(h) != u32::MAX {
                                handles.push(h.0 as isize);
                            } else {
                                let _ = CloseHandle(h);
                            }
                        }
                    }
                    if Thread32Next(snap, &mut e).is_err() { break; }
                }
            }
            let _ = CloseHandle(snap);
        }
        FrozenThreads(handles)
    }
}
impl Drop for FrozenThreads {
    fn drop(&mut self) {
        for &h in self.0.iter().rev() {
            unsafe {
                let hh = HANDLE(h as *mut c_void);
                ResumeThread(hh);
                let _ = CloseHandle(hh);
            }
        }
    }
}

/// Create + enable an inline detour on `target`, with other threads frozen
/// around the byte patch. Returns the trampoline (original) on success.
unsafe fn install_hook(target: *const (), detour: *const ()) -> Option<*const ()> {
    let d = match RawDetour::new(target, detour) {
        Ok(d) => d,
        Err(e) => { log(&format!("detour new failed: {e:?}")); return None; }
    };
    let tramp = d.trampoline() as *const () as *const ();
    let ok = {
        let _frozen = FrozenThreads::all_but_current();
        d.enable().is_ok()
    };
    if !ok { log("detour enable failed"); return None; }
    if let Ok(mut v) = DETOURS.lock() { v.push(d); }
    Some(tramp)
}

/// Hook d2d1!D2D1CreateFactory so we can patch render-target DrawGlyphRun.
unsafe fn setup_d2d_hook() {
    let d2d1 = match GetModuleHandleW(w!("d2d1.dll")) {
        Ok(h) if !h.is_invalid() => h,
        _ => match windows::Win32::System::LibraryLoader::LoadLibraryW(w!("d2d1.dll")) {
            Ok(h) => h.into(),
            Err(_) => { log("d2d1.dll not available"); return; }
        },
    };
    let Some(target) = GetProcAddress(d2d1, s!("D2D1CreateFactory")) else { return };
    if let Some(tramp) = install_hook(target as *const (), d2dcf_detour as *const ()) {
        ORIG_D2DCF = Some(std::mem::transmute::<*const (), FnD2DCreateFactory>(tramp));
        log("hook installed on D2D1CreateFactory");
    } else {
        log("D2D1CreateFactory hook failed");
    }
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
    FT = Some(ft);
    // Use the active font-tuner profile if present, else the default.
    let (path, p) = load_profile();
    log(&format!("profile {}: {p:?}", path.as_deref().unwrap_or("(default)")));
    TABLES = Some(tables_for(&p));
    PROFILE = Some(p);
    RELOAD_MSG.store(RegisterWindowMessageW(RELOAD_MSG_NAME), Ordering::Relaxed);

    let Ok(gdi32) = GetModuleHandleW(w!("gdi32.dll")) else { return 1 };
    let Some(target) = GetProcAddress(gdi32, s!("ExtTextOutW")) else { return 1 };
    if let Some(tramp) = install_hook(target as *const (), detour as *const ()) {
        ORIG = Some(std::mem::transmute::<*const (), FnEto>(tramp));
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
            SELF_HINST = hinst;
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
