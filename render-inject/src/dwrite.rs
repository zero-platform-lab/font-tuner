//! DirectWrite text, by two routes.
//!
//! `IDWriteBitmapRenderTarget::DrawGlyphRun` covers apps that draw through a
//! bitmap render target. Chromium/Skia and VS Code instead build a glyph-run
//! *analysis* and ask for its alpha texture, so that route captures the run at
//! `CreateGlyphRunAnalysis` and substitutes coverage at `CreateAlphaTexture`.
//!
//! Each detour turns its raw arguments into a `GlyphRun` borrow at the
//! boundary; the rendering after that is safe code.

use core::ffi::c_void;
use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use render_core::render::{draw_glyphs_onto, glyph_run_coverage_lcd, Ink};
use render_core::{Aa, Profile};
use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteFactory1, IDWriteFactory2,
    IDWriteFactory3, IDWriteFontFace, IDWriteFontFile, IDWriteRenderingParams, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_GLYPH_RUN, DWRITE_GRID_FIT_MODE, DWRITE_GRID_FIT_MODE_DEFAULT, DWRITE_GRID_FIT_MODE_DISABLED,
    DWRITE_GRID_FIT_MODE_ENABLED, DWRITE_MATRIX, DWRITE_PIXEL_GEOMETRY, DWRITE_PIXEL_GEOMETRY_BGR,
    DWRITE_PIXEL_GEOMETRY_FLAT, DWRITE_PIXEL_GEOMETRY_RGB, DWRITE_RENDERING_MODE, DWRITE_RENDERING_MODE1,
};

use crate::dib::Dib;
use crate::hook::{patch_slot, VTABLE_PATCH_LOCK};
use crate::log;
use crate::state::{orig, round_i32, RenderState, CAPTURED, RENDER};

// ---- Rendering params for the text Direct2D draws itself ----

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
    let built = build_dw_rendering(p);
    if built.is_none() {
        log("DirectWrite rendering params: not available");
    }
    if let Ok(mut g) = DW_RENDERING.lock() {
        *g = built;
    }
}

fn build_dw_rendering(p: &Profile) -> Option<DwRendering> {
    // SAFETY: creating the shared DirectWrite factory has no preconditions.
    let f: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ok()?;
    let (geometry, aa_mode) = match p.aa.mode() {
        2 | 4 => (DWRITE_PIXEL_GEOMETRY_RGB, 0),
        3 | 5 => (DWRITE_PIXEL_GEOMETRY_BGR, 0),
        _ => (DWRITE_PIXEL_GEOMETRY_FLAT, 2), // D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE
    };
    let grid_fit = match p.hinting {
        0 => DWRITE_GRID_FIT_MODE_DEFAULT,
        1 => DWRITE_GRID_FIT_MODE_DISABLED,
        _ => DWRITE_GRID_FIT_MODE_ENABLED,
    };
    // RenderingMode is passed through as upstream's D2D params do (its
    // GetD2DParams keeps 6 = OUTLINE; only its analysis-path params remap 6
    // to natural symmetric, and that path is rendered by us, not DirectWrite).
    let want = ParamsWanted {
        gamma: p.dw.gamma,
        contrast: p.dw.contrast,
        cleartype: p.dw.cleartype_level,
        geometry,
        mode: DWRITE_RENDERING_MODE(p.dw.rendering_mode),
        mode1: DWRITE_RENDERING_MODE1(p.dw.rendering_mode),
        grid_fit,
    };
    let params = custom_params(&f, &want)?;
    Some(DwRendering { params, aa_mode, grid_fit_disabled: grid_fit == DWRITE_GRID_FIT_MODE_DISABLED })
}

/// The arguments to `CreateCustomRenderingParams`, in the union of the four
/// factory generations' signatures.
struct ParamsWanted {
    gamma: f32,
    contrast: f32,
    cleartype: f32,
    geometry: DWRITE_PIXEL_GEOMETRY,
    mode: DWRITE_RENDERING_MODE,
    mode1: DWRITE_RENDERING_MODE1,
    grid_fit: DWRITE_GRID_FIT_MODE,
}

fn custom_params(f: &IDWriteFactory, w: &ParamsWanted) -> Option<IDWriteRenderingParams> {
    // SAFETY: plain COM calls on a live factory; each result is a new params
    // object we own.
    unsafe {
        if let Ok(f3) = f.cast::<IDWriteFactory3>() {
            if let Ok(r) = f3.CreateCustomRenderingParams(w.gamma, w.contrast, w.contrast, w.cleartype, w.geometry, w.mode1, w.grid_fit) {
                return r.cast().ok();
            }
        }
        if let Ok(f2) = f.cast::<IDWriteFactory2>() {
            if let Ok(r) = f2.CreateCustomRenderingParams(w.gamma, w.contrast, w.contrast, w.cleartype, w.geometry, w.mode, w.grid_fit) {
                return r.cast().ok();
            }
        }
        if let Ok(f1) = f.cast::<IDWriteFactory1>() {
            if let Ok(r) = f1.CreateCustomRenderingParams(w.gamma, w.contrast, w.contrast, w.cleartype, w.geometry, w.mode) {
                return r.cast().ok();
            }
        }
        f.CreateCustomRenderingParams(w.gamma, w.contrast, w.cleartype, w.geometry, w.mode).ok()
    }
}

// ---- Glyph runs ----

/// A `DWRITE_GLYPH_RUN` with its pointers turned into borrows.
pub(crate) struct GlyphRun<'a> {
    pub(crate) face: &'a IDWriteFontFace,
    pub(crate) glyphs: &'a [u16],
    pub(crate) advances: Option<&'a [f32]>,
    /// Em size in pixels, rounded.
    pub(crate) px: i32,
}

impl GlyphRun<'_> {
    /// Borrow the run behind `r`, if it has a face and glyphs.
    ///
    /// # Safety
    /// `r` must be a run DirectWrite handed to a `DrawGlyphRun` /
    /// `CreateGlyphRunAnalysis` call: `glyphIndices` (and `glyphAdvances`
    /// when non-null) hold `glyphCount` entries for the call's duration.
    pub(crate) unsafe fn borrow(r: &DWRITE_GLYPH_RUN) -> Option<GlyphRun<'_>> {
        let face = r.fontFace.deref().as_ref()?;
        let n = r.glyphCount as usize;
        if n == 0 || r.glyphIndices.is_null() {
            return None;
        }
        // SAFETY: per the contract above.
        let (glyphs, advances) = unsafe {
            (
                core::slice::from_raw_parts(r.glyphIndices, n),
                (!r.glyphAdvances.is_null()).then(|| core::slice::from_raw_parts(r.glyphAdvances, n)),
            )
        };
        Some(GlyphRun { face, glyphs, advances, px: round_i32(r.fontEmSize) })
    }

    /// The cache key for this run's face: its address (pinned by the clone
    /// `RenderState` keeps) plus the face index.
    pub(crate) fn face_key(&self, prefix: &str) -> String {
        format!("{prefix}:{:x}:{}", self.face.as_raw().addr(), self.face_index())
    }

    pub(crate) fn face_index(&self) -> u32 {
        // SAFETY: a COM getter on a live face.
        unsafe { self.face.GetIndex() }
    }

    /// Make `st`'s FreeType face this run's font, unless it already is.
    pub(crate) fn reface(&self, st: &mut RenderState, prefix: &str) -> Option<()> {
        let key = self.face_key(prefix);
        if st.font_key.as_deref() != Some(key.as_str()) {
            let (bytes, index) = font_bytes(self.face)?;
            st.ft.reface_memory_index(&bytes, i64::from(index)).ok()?;
            st.font_key = Some(key);
            st.font_face = Some(self.face.clone());
        }
        Some(())
    }
}

/// The font-file bytes + face index behind a DirectWrite font face.
pub(crate) fn font_bytes(face: &IDWriteFontFace) -> Option<(Vec<u8>, u32)> {
    // SAFETY: COM calls on a live face; `files` is sized by the first
    // GetFiles call, the fragment is copied out before it is released.
    unsafe {
        let mut n = 0u32;
        face.GetFiles(&raw mut n, None).ok()?;
        let mut files: Vec<Option<IDWriteFontFile>> = vec![None; n as usize];
        face.GetFiles(&raw mut n, Some(files.as_mut_ptr())).ok()?;
        let file = files.into_iter().next()??;
        let mut key: *mut c_void = core::ptr::null_mut();
        let mut keysz = 0u32;
        file.GetReferenceKey(&raw mut key, &raw mut keysz).ok()?;
        let stream = file.GetLoader().ok()?.CreateStreamFromKey(key.cast_const(), keysz).ok()?;
        let size = stream.GetFileSize().ok()?;
        let mut frag: *mut c_void = core::ptr::null_mut();
        let mut ctx: *mut c_void = core::ptr::null_mut();
        stream.ReadFileFragment(&raw mut frag, 0, size, &raw mut ctx).ok()?;
        let bytes = core::slice::from_raw_parts(frag.cast_const().cast::<u8>(), usize::try_from(size).ok()?).to_vec();
        stream.ReleaseFileFragment(ctx);
        Some((bytes, face.GetIndex()))
    }
}

// ---- IDWriteBitmapRenderTarget::DrawGlyphRun (vtable slot 3) ----

/// `DrawGlyphRun(baselineOriginX, baselineOriginY, measuringMode, glyphRun,
/// renderingParams, textColor, blackBoxRect)`.
type FnDrawGlyphRun =
    unsafe extern "system" fn(*mut c_void, f32, f32, i32, *const DWRITE_GLYPH_RUN, *mut c_void, u32, *mut RECT) -> HRESULT;
static ORIG_DGR: OnceLock<FnDrawGlyphRun> = OnceLock::new();

unsafe extern "system" fn dgr_detour(
    this: *mut c_void, bx: f32, by: f32, mm: i32, run: *const DWRITE_GLYPH_RUN, rp: *mut c_void, color: u32, bbox: *mut RECT,
) -> HRESULT {
    // SAFETY: `this` is the render target the call was made on, `run` is
    // DirectWrite's argument (null-checked); both live for the call.
    let handled = unsafe {
        IDWriteBitmapRenderTarget::from_raw_borrowed(&this)
            .zip(run.as_ref())
            .and_then(|(brt, r)| GlyphRun::borrow(r).map(|g| (brt, g)))
            .is_some_and(|(brt, g)| dgr_render(brt, &g, (bx, by), color).is_some())
    };
    if handled {
        return HRESULT(0); // S_OK
    }
    // SAFETY: arguments forwarded untouched.
    unsafe { (orig(&ORIG_DGR))(this, bx, by, mm, run, rp, color, bbox) }
}

/// `textColor` is a `COLORREF`.
fn rgb(colorref: u32) -> [u8; 3] {
    let [r, g, b, _] = colorref.to_le_bytes();
    [r, g, b]
}

fn dgr_render(brt: &IDWriteBitmapRenderTarget, g: &GlyphRun<'_>, baseline: (f32, f32), color: u32) -> Option<()> {
    // SAFETY: getters on a live render target.
    let (hdc, size) = unsafe { (brt.GetMemoryDC(), brt.GetSize().ok()?) };
    if size.cx <= 0 || size.cy <= 0 {
        return None;
    }
    let mut dib = Dib::new(hdc, size.cx, size.cy)?;
    dib.copy_from(hdc, 0, 0);
    let mut canvas = dib.canvas();
    let pen = (round_i32(baseline.0), round_i32(baseline.1));
    // The render lock serialises every draw; held for this statement only.
    RENDER.lock().ok()?.as_mut().and_then(|st| {
        g.reface(st, "dw")?;
        let RenderState { ft, tables, profile, .. } = st;
        draw_glyphs_onto(&mut canvas, ft, tables, profile, Ink { fg: rgb(color) }, g.glyphs, g.px, pen, None);
        Some(())
    })?;
    dib.blit(&canvas);
    if !CAPTURED.swap(true, Ordering::SeqCst) {
        if let Some(tmp) = std::env::var_os("TEMP") {
            let p = PathBuf::from(tmp).join("render-inject-dwrite.png");
            let _ = canvas.save(&p.to_string_lossy());
            log(&format!("captured DirectWrite render to {}", p.display()));
        }
    }
    dib.copy_to(hdc, 0, 0);
    Some(())
}

// ---- CreateGlyphRunAnalysis → CreateAlphaTexture (Chromium/Skia, VS Code) ----

/// `IDWriteFactory::CreateGlyphRunAnalysis(run, pixelsPerDip, transform,
/// renderingMode, measuringMode, baselineX, baselineY, out)`.
type FnCreateGlyphRunAnalysis = unsafe extern "system" fn(
    *mut c_void, *const DWRITE_GLYPH_RUN, f32, *const DWRITE_MATRIX, i32, i32, f32, f32, *mut *mut c_void,
) -> HRESULT;
/// `IDWriteFactory2::CreateGlyphRunAnalysis(run, transform, renderingMode,
/// measuringMode, gridFitMode, antialiasMode, baselineX, baselineY, out)`;
/// `IDWriteFactory3`'s has the same shape with `DWRITE_RENDERING_MODE1`.
type FnCreateGlyphRunAnalysis2 = unsafe extern "system" fn(
    *mut c_void, *const DWRITE_GLYPH_RUN, *const DWRITE_MATRIX, i32, i32, i32, i32, f32, f32, *mut *mut c_void,
) -> HRESULT;
/// `IDWriteGlyphRunAnalysis::CreateAlphaTexture(textureType, bounds, alphaValues, bufferSize)`.
type FnCreateAlphaTexture = unsafe extern "system" fn(*mut c_void, i32, *const RECT, *mut u8, u32) -> HRESULT;

static ORIG_CGRA: OnceLock<FnCreateGlyphRunAnalysis> = OnceLock::new();
static ORIG_CGRA2: OnceLock<FnCreateGlyphRunAnalysis2> = OnceLock::new();
static ORIG_CGRA3: OnceLock<FnCreateGlyphRunAnalysis2> = OnceLock::new();
/// Published before the vtable write, so `cat_detour` (which can only run
/// once that write has happened) always finds it.
static ORIG_CAT: OnceLock<FnCreateAlphaTexture> = OnceLock::new();
/// Set *after* the vtable write. `patch_cat_vtable`'s lock-free fast path
/// keys off this, not `ORIG_CAT`: between the two stores another thread
/// would otherwise see "patched", skip, and get an untuned texture.
static CAT_PATCHED: AtomicBool = AtomicBool::new(false);
/// Analysis object address → the run it was made from.
static ANALYSES: Mutex<Option<HashMap<usize, RunInfo>>> = Mutex::new(None);
/// Analyses never followed by a CreateAlphaTexture would leak an entry each;
/// past this many, the map is cleared (in-flight ones then draw untuned).
const ANALYSES_CAP: usize = 4096;
/// `DWRITE_TEXTURE_CLEARTYPE_3x1`.
const TEXTURE_CLEARTYPE_3X1: i32 = 1;

struct RunInfo {
    bytes: Vec<u8>,
    index: u32,
    glyphs: Vec<u16>,
    px: i32,
    baseline: (i32, i32),
}

unsafe extern "system" fn cgra_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, ppd: f32, transform: *const DWRITE_MATRIX, rmode: i32, mmode: i32,
    bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: arguments forwarded untouched; `run`/`out` are then inspected
    // under DirectWrite's contract for this call.
    unsafe {
        let hr = (orig(&ORIG_CGRA))(this, run, ppd, transform, rmode, mmode, bx, by, out);
        record_analysis(hr, run, (bx, by), out);
        hr
    }
}

unsafe extern "system" fn cgra2_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, transform: *const DWRITE_MATRIX, rmode: i32, mmode: i32, grid: i32,
    aa: i32, bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: as in `cgra_detour`.
    unsafe {
        let hr = (orig(&ORIG_CGRA2))(this, run, transform, rmode, mmode, grid, aa, bx, by, out);
        record_analysis(hr, run, (bx, by), out);
        hr
    }
}

unsafe extern "system" fn cgra3_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, transform: *const DWRITE_MATRIX, rmode1: i32, mmode: i32, grid: i32,
    aa: i32, bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: as in `cgra_detour`.
    unsafe {
        let hr = (orig(&ORIG_CGRA3))(this, run, transform, rmode1, mmode, grid, aa, bx, by, out);
        record_analysis(hr, run, (bx, by), out);
        hr
    }
}

/// After any CreateGlyphRunAnalysis overload: remember the run behind the new
/// analysis object and make sure its CreateAlphaTexture is ours.
///
/// # Safety
/// `run` and `out` are the call's arguments, valid for its duration.
unsafe fn record_analysis(hr: HRESULT, run: *const DWRITE_GLYPH_RUN, baseline: (f32, f32), out: *mut *mut c_void) {
    if !hr.is_ok() {
        return;
    }
    // SAFETY: per the contract above; `*out` is the new analysis object.
    let (analysis, g) = unsafe {
        let Some(analysis) = out.as_ref().copied().filter(|p| !p.is_null()) else { return };
        let Some(g) = run.as_ref().and_then(|r| GlyphRun::borrow(r)) else { return };
        (analysis, g)
    };
    let Some((bytes, index)) = font_bytes(g.face) else { return };
    let info = RunInfo {
        bytes,
        index,
        glyphs: g.glyphs.to_vec(),
        px: g.px,
        baseline: (round_i32(baseline.0), round_i32(baseline.1)),
    };
    if let Ok(mut m) = ANALYSES.lock() {
        let map = m.get_or_insert_with(HashMap::new);
        if map.len() >= ANALYSES_CAP {
            map.clear();
        }
        map.insert(analysis.addr(), info);
    }
    patch_cat_vtable(analysis);
}

/// Patch `CreateAlphaTexture` (slot 4) in the analysis object's vtable, once.
fn patch_cat_vtable(analysis: *mut c_void) {
    if CAT_PATCHED.load(Ordering::Acquire) {
        return; // fast path, no lock once patched
    }
    let _guard = VTABLE_PATCH_LOCK.lock();
    if CAT_PATCHED.load(Ordering::Acquire) {
        return; // re-check under the lock
    }
    // SAFETY: `analysis` is a live COM object, so its first word is its
    // vtable and slot 4 is CreateAlphaTexture; the transmute types the
    // previous slot value, which is that method.
    let patched = unsafe {
        let vtbl = *analysis.cast::<*mut *const ()>();
        patch_slot(vtbl.add(4), cat_detour as *const (), |old| {
            let _ = ORIG_CAT.set(std::mem::transmute::<*const (), FnCreateAlphaTexture>(old));
        })
    };
    if patched {
        CAT_PATCHED.store(true, Ordering::Release);
        log("hook installed on CreateAlphaTexture");
    }
}

unsafe extern "system" fn cat_detour(this: *mut c_void, tex_type: i32, bounds: *const RECT, alpha: *mut u8, size: u32) -> HRESULT {
    let filled = if tex_type == TEXTURE_CLEARTYPE_3X1 {
        // SAFETY: DirectWrite's contract for CreateAlphaTexture — `bounds`
        // is one RECT and `alpha` holds `size` bytes, for the call's duration.
        unsafe {
            bounds.as_ref().zip((!alpha.is_null()).then(|| core::slice::from_raw_parts_mut(alpha, size as usize)))
        }
        .is_some_and(|(b, alpha)| cat_fill(this.addr(), b, alpha).is_some())
    } else {
        false
    };
    if filled {
        return HRESULT(0);
    }
    // SAFETY: arguments forwarded untouched.
    unsafe { (orig(&ORIG_CAT))(this, tex_type, bounds, alpha, size) }
}

/// Fill `alpha` (a `w*h*3` ClearType 3x1 texture) with render-core coverage
/// for the run captured for analysis `this`. The captured run is consumed:
/// an analysis produces its texture once, and the map must not grow for
/// the life of the process.
fn cat_fill(this: usize, bounds: &RECT, alpha: &mut [u8]) -> Option<()> {
    let width = usize::try_from(bounds.right - bounds.left).ok()?;
    let height = usize::try_from(bounds.bottom - bounds.top).ok()?;
    if width == 0 || height == 0 || width * height * 3 != alpha.len() {
        return None;
    }
    let info = ANALYSES.lock().ok()?.as_mut()?.remove(&this)?;
    let coverage = RENDER.lock().ok()?.as_mut().and_then(|st| coverage_for(st, &info, bounds, width, height))?;
    if !CAPTURED.swap(true, Ordering::SeqCst) {
        if let Some(tmp) = std::env::var_os("TEMP") {
            let p = PathBuf::from(tmp).join("render-inject-analysis.txt");
            let _ = std::fs::write(p, format!("analysis substituted: {width}x{height}, glyphs={}", info.glyphs.len()));
            log("substituted CreateAlphaTexture via render-core");
        }
    }
    alpha.copy_from_slice(&coverage);
    Some(())
}

/// Render `info`'s run as LCD coverage with the shared face.
fn coverage_for(st: &mut RenderState, info: &RunInfo, bounds: &RECT, width: usize, height: usize) -> Option<Vec<u8>> {
    // Key on the font identity (face index + file length), not the analysis
    // object address: addresses are recycled, so keying on the analysis
    // would reuse a stale face when a freed pointer is handed to another font.
    let key = format!("dwa:{}:{}", info.index, info.bytes.len());
    if st.font_key.as_deref() != Some(key.as_str()) {
        st.ft.reface_memory_index(&info.bytes, i64::from(info.index)).ok()?;
        st.font_key = Some(key);
        st.font_face = None;
    }
    // Force LCD subpixel for the CLEARTYPE_3x1 texture, keep the profile's hinting.
    let lcd = Profile { aa: Aa::LcdRgb, ..st.profile };
    let pen = (info.baseline.0 - bounds.left, info.baseline.1 - bounds.top);
    Some(glyph_run_coverage_lcd(&st.ft, &lcd, &info.glyphs, info.px, pen, width, height))
}

// ---- setup ----

/// Patch the shared vtables: `IDWriteBitmapRenderTarget::DrawGlyphRun` (slot
/// 3) and the three `CreateGlyphRunAnalysis` overloads (IDWriteFactory 23,
/// IDWriteFactory2 30, IDWriteFactory3 31 — Skia prefers the later ones when
/// they exist, and an analysis made through those never passed slot 23, so
/// its CreateAlphaTexture found no run). The vtables are shared, so patching
/// them through our own factory covers the app's too.
pub(crate) fn setup_dwrite_hook() {
    let Some((factory, brt)) = factory_and_probe_target() else {
        log("dwrite factory failed");
        return;
    };
    // SAFETY: both objects are live COM objects, so their first word is a
    // vtable; each transmute types the slot's previous value, which is the
    // method that slot names.
    unsafe {
        let vtbl = *brt.as_raw().cast::<*mut *const ()>();
        if patch_slot(vtbl.add(3), dgr_detour as *const (), |old| {
            let _ = ORIG_DGR.set(std::mem::transmute::<*const (), FnDrawGlyphRun>(old));
        }) {
            log("hook installed on DrawGlyphRun");
        }
        let fvtbl = *factory.as_raw().cast::<*mut *const ()>();
        patch_slot(fvtbl.add(23), cgra_detour as *const (), |old| {
            let _ = ORIG_CGRA.set(std::mem::transmute::<*const (), FnCreateGlyphRunAnalysis>(old));
        });
        log("hook installed on CreateGlyphRunAnalysis");
        if factory.cast::<IDWriteFactory2>().is_ok() {
            patch_slot(fvtbl.add(30), cgra2_detour as *const (), |old| {
                let _ = ORIG_CGRA2.set(std::mem::transmute::<*const (), FnCreateGlyphRunAnalysis2>(old));
            });
            log("hook installed on IDWriteFactory2::CreateGlyphRunAnalysis");
        }
        if factory.cast::<IDWriteFactory3>().is_ok() {
            patch_slot(fvtbl.add(31), cgra3_detour as *const (), |old| {
                let _ = ORIG_CGRA3.set(std::mem::transmute::<*const (), FnCreateGlyphRunAnalysis2>(old));
            });
            log("hook installed on IDWriteFactory3::CreateGlyphRunAnalysis");
        }
    }
}

/// The shared factory plus one throwaway 8x8 bitmap render target, whose
/// vtables are the ones every app in this process shares.
fn factory_and_probe_target() -> Option<(IDWriteFactory, IDWriteBitmapRenderTarget)> {
    // SAFETY: creating these objects has no preconditions.
    unsafe {
        let f: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
        let brt = f.GetGdiInterop().ok()?.CreateBitmapRenderTarget(None, 8, 8).ok()?;
        Some((f, brt))
    }
}
