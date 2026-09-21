//! DirectWrite text, by two routes.
//!
//! `IDWriteBitmapRenderTarget::DrawGlyphRun` covers apps that draw through a
//! bitmap render target. Chromium/Skia and VS Code instead build a glyph-run
//! *analysis* and ask for its alpha texture, so that route captures the run at
//! `CreateGlyphRunAnalysis` and substitutes coverage at `CreateAlphaTexture`.

use core::ffi::c_void;
use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use render_core::render::{draw_glyphs_onto, glyph_run_coverage_lcd, Canvas, Ink};
use render_core::{Aa, Profile};
use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteFactory1, IDWriteFactory2,
    IDWriteFactory3, IDWriteFontFile, IDWriteRenderingParams, DWRITE_FACTORY_TYPE_SHARED, DWRITE_GLYPH_RUN,
    DWRITE_GRID_FIT_MODE, DWRITE_GRID_FIT_MODE_DEFAULT, DWRITE_GRID_FIT_MODE_DISABLED, DWRITE_GRID_FIT_MODE_ENABLED,
    DWRITE_MATRIX, DWRITE_PIXEL_GEOMETRY, DWRITE_PIXEL_GEOMETRY_BGR, DWRITE_PIXEL_GEOMETRY_FLAT,
    DWRITE_PIXEL_GEOMETRY_RGB, DWRITE_RENDERING_MODE, DWRITE_RENDERING_MODE1, DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC,
    DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};

use crate::hook::{patch_slot, VTABLE_PATCH_LOCK};
use crate::log;
use crate::state::{orig, RenderState, CAPTURED, RENDER};

/// What Direct2D gets told to do for text we cannot rasterise ourselves: the
/// profile's `[DirectWrite]` values as `IDWriteRenderingParams`, plus the
/// antialias mode and grid-fit choice derived the way upstream does
/// (`Params::Params` in `directwrite.cpp`). Rebuilt on every profile load.
#[derive(Clone)]
pub(crate) struct DwRendering {
    pub(crate) params: IDWriteRenderingParams,
    /// `D2D1_TEXT_ANTIALIAS_MODE`: greyscale for a greyscale profile, else
    /// DEFAULT (ClearType).
    pub(crate) aa_mode: i32,
    /// Upstream nudges the transform by 1/65535 when grid fitting is off, so
    /// DirectWrite stops snapping glyphs to the pixel grid.
    pub(crate) grid_fit_disabled: bool,
}

static DW_RENDERING: Mutex<Option<DwRendering>> = Mutex::new(None);

/// The current Direct2D rendering setup, if the profile could be turned into one.
pub(crate) fn dw_rendering() -> Option<DwRendering> {
    DW_RENDERING.lock().ok()?.clone()
}

/// (Re)build `DW_RENDERING` from `p`. Tries `IDWriteFactory3` → 2 → 1 → 0
/// like upstream, so the richest `CreateCustomRenderingParams` available on
/// this OS is used.
pub(crate) fn refresh_dw_rendering(p: &Profile) {
    let built = unsafe { build_dw_rendering(p) };
    if built.is_none() { log("DirectWrite rendering params: not available"); }
    if let Ok(mut g) = DW_RENDERING.lock() { *g = built; }
}

unsafe fn build_dw_rendering(p: &Profile) -> Option<DwRendering> {
    let f: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
    let (geometry, aa_mode) = match p.aa.mode() {
        2 | 4 => (DWRITE_PIXEL_GEOMETRY_RGB, 0),
        3 | 5 => (DWRITE_PIXEL_GEOMETRY_BGR, 0),
        _ => (DWRITE_PIXEL_GEOMETRY_FLAT, 2), // D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE
    };
    let grid_fit = match p.hinting { 0 => DWRITE_GRID_FIT_MODE_DEFAULT, 1 => DWRITE_GRID_FIT_MODE_DISABLED, _ => DWRITE_GRID_FIT_MODE_ENABLED };
    // RenderingMode 6 is not a DWRITE_RENDERING_MODE; upstream maps it to
    // natural symmetric.
    let (mode, mode1) = if p.dw.rendering_mode == 6 {
        (DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC, DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC)
    } else {
        (DWRITE_RENDERING_MODE(p.dw.rendering_mode), DWRITE_RENDERING_MODE1(p.dw.rendering_mode))
    };
    let d = &p.dw;
    let params = custom_params(&f, d.gamma, d.contrast, d.cleartype_level, geometry, mode, mode1, grid_fit)?;
    Some(DwRendering { params, aa_mode, grid_fit_disabled: grid_fit == DWRITE_GRID_FIT_MODE_DISABLED })
}

unsafe fn custom_params(
    f: &IDWriteFactory, gamma: f32, contrast: f32, cleartype: f32, geometry: DWRITE_PIXEL_GEOMETRY,
    mode: DWRITE_RENDERING_MODE, mode1: DWRITE_RENDERING_MODE1, grid_fit: DWRITE_GRID_FIT_MODE,
) -> Option<IDWriteRenderingParams> {
    if let Ok(f3) = f.cast::<IDWriteFactory3>() {
        if let Ok(r) = f3.CreateCustomRenderingParams(gamma, contrast, contrast, cleartype, geometry, mode1, grid_fit) {
            return r.cast().ok();
        }
    }
    if let Ok(f2) = f.cast::<IDWriteFactory2>() {
        if let Ok(r) = f2.CreateCustomRenderingParams(gamma, contrast, contrast, cleartype, geometry, mode, grid_fit) {
            return r.cast().ok();
        }
    }
    if let Ok(f1) = f.cast::<IDWriteFactory1>() {
        if let Ok(r) = f1.CreateCustomRenderingParams(gamma, contrast, contrast, cleartype, geometry, mode) {
            return r.cast().ok();
        }
    }
    f.CreateCustomRenderingParams(gamma, contrast, cleartype, geometry, mode).ok()
}

type FnDrawGlyphRun = unsafe extern "system" fn(
    *mut c_void, f32, f32, i32, *const DWRITE_GLYPH_RUN, *mut c_void, u32, *mut RECT,
) -> HRESULT;
static ORIG_DGR: OnceLock<FnDrawGlyphRun> = OnceLock::new();

// DirectWrite glyph-run *analysis* path (Chromium/Skia / VS Code): capture the
// run at CreateGlyphRunAnalysis, substitute coverage at CreateAlphaTexture.
type FnCreateGlyphRunAnalysis = unsafe extern "system" fn(
    *mut c_void, *const DWRITE_GLYPH_RUN, f32, *const DWRITE_MATRIX, i32, i32, f32, f32, *mut *mut c_void,
) -> HRESULT;
type FnCreateAlphaTexture = unsafe extern "system" fn(
    *mut c_void, i32, *const RECT, *mut u8, u32,
) -> HRESULT;
static ORIG_CGRA: OnceLock<FnCreateGlyphRunAnalysis> = OnceLock::new();
type FnCreateGlyphRunAnalysis2 = unsafe extern "system" fn(
    *mut c_void, *const DWRITE_GLYPH_RUN, *const DWRITE_MATRIX, i32, i32, i32, i32, f32, f32, *mut *mut c_void,
) -> HRESULT;
type FnCreateGlyphRunAnalysis3 = FnCreateGlyphRunAnalysis2;
static ORIG_CGRA2: OnceLock<FnCreateGlyphRunAnalysis2> = OnceLock::new();
static ORIG_CGRA3: OnceLock<FnCreateGlyphRunAnalysis3> = OnceLock::new();
/// Published before the vtable write, so `cat_detour` (which can only run
/// once that write has happened) always finds it.
static ORIG_CAT: OnceLock<FnCreateAlphaTexture> = OnceLock::new();
/// Set *after* the vtable write. `patch_cat_vtable`'s lock-free fast path
/// keys off this, not `ORIG_CAT`: between the two stores another thread
/// would otherwise see "patched", skip, and get an untuned texture.
static CAT_PATCHED: AtomicBool = AtomicBool::new(false);
static ANALYSES: Mutex<Option<HashMap<usize, RunInfo>>> = Mutex::new(None);

struct RunInfo {
    bytes: Vec<u8>,
    index: u32,
    glyphs: Vec<u16>,
    px: i32,
    baseline: (i32, i32),
}


// ---- DirectWrite (IDWriteBitmapRenderTarget::DrawGlyphRun) ----

/// Extract the font-file bytes + face index for a run's font face.
pub(crate) unsafe fn dwrite_font_bytes(run: &DWRITE_GLYPH_RUN) -> Option<(Vec<u8>, u32)> {
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
    (orig(&ORIG_DGR))(this, bx, by, mm, run, rp, color, bbox)
}

unsafe fn dgr_render(this: *mut c_void, r: &DWRITE_GLYPH_RUN, bx: f32, by: f32, color: u32) -> Option<()> {
    let mut guard = RENDER.lock().ok()?; // serialises every draw
    let RenderState { ft, tables, profile, font_key } = guard.as_mut()?;
    let brt = IDWriteBitmapRenderTarget::from_raw_borrowed(&this)?;
    let hdc = brt.GetMemoryDC();
    let size = brt.GetSize().ok()?;
    let (w, h) = (size.cx, size.cy);
    if w <= 0 || h <= 0 { return None; }

    // key on the font-face identity; only re-extract on a miss.
    let face = r.fontFace.deref().as_ref()?;
    let key = format!("dw:{:x}:{}", face.as_raw() as usize, face.GetIndex());
    if font_key.as_deref() != Some(key.as_str()) {
        let (bytes, index) = dwrite_font_bytes(r)?;
        ft.reface_memory_index(&bytes, index as i64).ok()?;
        *font_key = Some(key);
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
pub(crate) unsafe fn setup_dwrite_hook() {
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
    let _ = ORIG_DGR.set(std::mem::transmute::<usize, FnDrawGlyphRun>(*slot));
    *slot = dgr_detour as *const () as usize;
    let _ = VirtualProtect(slot as *const c_void, 8, oldp, &mut oldp);
    log("hook installed on DrawGlyphRun");

    // Patch CreateGlyphRunAnalysis for the analysis/coverage path
    // (Chromium/Skia): IDWriteFactory slot 23, plus the IDWriteFactory2 (30)
    // and IDWriteFactory3 (31) overloads Skia prefers when they exist — an
    // analysis made through those never passed slot 23, so its
    // CreateAlphaTexture found no run and drew untuned. The vtable is shared,
    // so this covers the app's own factory too.
    let fvtbl = *(factory.as_raw() as *mut *mut usize);
    patch_slot(fvtbl.add(23), cgra_detour as *const () as usize, |o| { let _ = ORIG_CGRA.set(std::mem::transmute::<usize, FnCreateGlyphRunAnalysis>(o)); });
    log("hook installed on CreateGlyphRunAnalysis");
    if factory.cast::<IDWriteFactory2>().is_ok() {
        patch_slot(fvtbl.add(30), cgra2_detour as *const () as usize, |o| { let _ = ORIG_CGRA2.set(std::mem::transmute::<usize, FnCreateGlyphRunAnalysis2>(o)); });
        log("hook installed on IDWriteFactory2::CreateGlyphRunAnalysis");
    }
    if factory.cast::<IDWriteFactory3>().is_ok() {
        patch_slot(fvtbl.add(31), cgra3_detour as *const () as usize, |o| { let _ = ORIG_CGRA3.set(std::mem::transmute::<usize, FnCreateGlyphRunAnalysis3>(o)); });
        log("hook installed on IDWriteFactory3::CreateGlyphRunAnalysis");
    }
}

// CreateGlyphRunAnalysis: create the analysis, capture its run, patch its
// CreateAlphaTexture slot the first time.
unsafe extern "system" fn cgra_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, ppd: f32, transform: *const DWRITE_MATRIX,
    rmode: i32, mmode: i32, bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    let hr = (orig(&ORIG_CGRA))(this, run, ppd, transform, rmode, mmode, bx, by, out);
    record_analysis(hr, run, bx, by, out);
    hr
}

/// `IDWriteFactory2::CreateGlyphRunAnalysis(run, transform, renderingMode,
/// measuringMode, gridFitMode, antialiasMode, baselineX, baselineY, out)`.
unsafe extern "system" fn cgra2_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, transform: *const DWRITE_MATRIX,
    rmode: i32, mmode: i32, grid: i32, aa: i32, bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    let hr = (orig(&ORIG_CGRA2))(this, run, transform, rmode, mmode, grid, aa, bx, by, out);
    record_analysis(hr, run, bx, by, out);
    hr
}

/// `IDWriteFactory3::CreateGlyphRunAnalysis`: same shape with `DWRITE_RENDERING_MODE1`.
unsafe extern "system" fn cgra3_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, transform: *const DWRITE_MATRIX,
    rmode1: i32, mmode: i32, grid: i32, aa: i32, bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    let hr = (orig(&ORIG_CGRA3))(this, run, transform, rmode1, mmode, grid, aa, bx, by, out);
    record_analysis(hr, run, bx, by, out);
    hr
}

/// After any CreateGlyphRunAnalysis overload: remember the run behind the new
/// analysis object and make sure its CreateAlphaTexture is ours.
unsafe fn record_analysis(hr: HRESULT, run: *const DWRITE_GLYPH_RUN, bx: f32, by: f32, out: *mut *mut c_void) {
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
}

unsafe fn patch_cat_vtable(analysis: *mut c_void) {
    if CAT_PATCHED.load(Ordering::Acquire) { return; } // fast path, no lock once patched
    let _guard = VTABLE_PATCH_LOCK.lock();
    if CAT_PATCHED.load(Ordering::Acquire) { return; } // re-check under the lock
    let vtbl = *(analysis as *mut *mut usize);
    let slot = vtbl.add(4);
    let mut oldp = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut oldp).is_err() { return; }
    // Publish the trampoline before the vtable write: cat_detour reads ORIG_CAT,
    // and it can only run once the slot below points at it.
    let _ = ORIG_CAT.set(std::mem::transmute::<usize, FnCreateAlphaTexture>(*slot));
    *slot = cat_detour as *const () as usize;
    let _ = VirtualProtect(slot as *const c_void, 8, oldp, &mut oldp);
    CAT_PATCHED.store(true, Ordering::Release);
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
    (orig(&ORIG_CAT))(this, tex_type, bounds, alpha, size)
}

unsafe fn cat_fill(this: *mut c_void, b: &RECT, alpha: *mut u8, size: u32) -> Option<()> {
    let (w, h) = ((b.right - b.left) as usize, (b.bottom - b.top) as usize);
    if w == 0 || h == 0 || w * h * 3 != size as usize { return None; }
    let mut guard = RENDER.lock().ok()?;
    let RenderState { ft, profile, font_key, .. } = guard.as_mut()?;
    let m = ANALYSES.lock().ok()?;
    let info = m.as_ref()?.get(&(this as usize))?;
    // Key on the font identity (face index + file length), not the analysis
    // object address: addresses are recycled, so keying on `this` would reuse a
    // stale face when a freed analysis's pointer is handed to a different font.
    let key = format!("dwa:{}:{}", info.index, info.bytes.len());
    if font_key.as_deref() != Some(key.as_str()) {
        ft.reface_memory_index(&info.bytes, info.index as i64).ok()?;
        *font_key = Some(key);
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

