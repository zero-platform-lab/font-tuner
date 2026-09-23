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
use std::collections::{HashMap, VecDeque};
use std::ops::Deref;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, Once, OnceLock};

use render_core::render::{render_placed, Ink, RenderedRun};
use render_core::{Aa, GlyphStyle, Placed, Profile};
use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::{E_FAIL, RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteFactory1, IDWriteFactory2,
    IDWriteFactory3, IDWriteFontFace, IDWriteFontFile, IDWriteRenderingParams, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_GLYPH_RUN, DWRITE_GRID_FIT_MODE, DWRITE_GRID_FIT_MODE_DEFAULT, DWRITE_GRID_FIT_MODE_DISABLED,
    DWRITE_GRID_FIT_MODE_ENABLED, DWRITE_MATRIX, DWRITE_PIXEL_GEOMETRY, DWRITE_PIXEL_GEOMETRY_BGR,
    DWRITE_PIXEL_GEOMETRY_FLAT, DWRITE_PIXEL_GEOMETRY_RGB, DWRITE_RENDERING_MODE, DWRITE_RENDERING_MODE1,
    DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC, DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
};

use crate::dib::Dib;
use crate::hook::patch_slot;
use crate::layout::{self, Mapping};
use crate::log;
use crate::state::{orig, round_i32, RenderState, CAPTURED, RENDER};

// ---- Rendering params for the text the OS still draws ----

/// What DirectWrite / Direct2D get told to do for text we do not rasterise
/// ourselves: the profile's `[DirectWrite]` values as
/// `IDWriteRenderingParams`, plus the antialias mode and grid-fit choice
/// derived the way upstream does (`Params::Params` in `directwrite.cpp`).
/// Rebuilt on every profile load.
///
/// Upstream keeps two sets: `GetD2DParams` passes the rendering mode
/// through, `GetDWParams` (the DirectWrite hooks) swaps mode 6 (outline)
/// for natural symmetric, because "DW rendering in mode6 is horrible".
#[derive(Clone)]
pub(crate) struct DwRendering {
    /// For Direct2D (`GetD2DRenderingParams`).
    pub(crate) params: IDWriteRenderingParams,
    /// For DirectWrite (`GetDWRenderingParams`).
    pub(crate) dw_params: IDWriteRenderingParams,
    /// `GetDWParams()->RenderingMode` / `RenderingMode1` / `GridFitMode`,
    /// for the glyph-run analyses DirectWrite still makes.
    pub(crate) dw_mode: i32,
    pub(crate) dw_mode1: i32,
    pub(crate) grid_fit: i32,
    /// `DWRITE_TEXT_ANTIALIAS_MODE` for new glyph-run analyses: greyscale
    /// (1) for a greyscale profile, else ClearType (0). See `cgra2_detour`.
    pub(crate) dw_aa: i32,
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
    let dw_want = if p.dw.rendering_mode == 6 {
        ParamsWanted { mode: DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC, mode1: DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC, ..want }
    } else {
        want
    };
    let dw_params = custom_params(&f, &dw_want)?;
    Some(DwRendering {
        params,
        dw_params,
        dw_mode: dw_want.mode.0,
        dw_mode1: dw_want.mode1.0,
        grid_fit: grid_fit.0,
        dw_aa: i32::from(!p.aa.is_lcd()),
        aa_mode,
        grid_fit_disabled: grid_fit == DWRITE_GRID_FIT_MODE_DISABLED,
    })
}

/// The arguments to `CreateCustomRenderingParams`, in the union of the four
/// factory generations' signatures.
#[derive(Clone, Copy)]
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

    /// Make `st`'s FreeType face this run's font, unless it already is.
    pub(crate) fn reface(&self, st: &mut RenderState, prefix: &str) -> Option<()> {
        reface(st, self.face, prefix)
    }
}

/// Make `st`'s FreeType face `face`'s font, unless it already is. Keyed on
/// the face's address (pinned by the clone `RenderState` keeps) and index.
pub(crate) fn reface(st: &mut RenderState, face: &IDWriteFontFace, prefix: &str) -> Option<()> {
    // SAFETY: a COM getter on a live face.
    let key = format!("{prefix}:{:x}:{}", face.as_raw().addr(), unsafe { face.GetIndex() });
    if st.font_key.as_deref() != Some(key.as_str()) {
        let (bytes, index) = font_bytes(face)?;
        st.ft.reface_memory_index(&bytes, i64::from(index)).ok()?;
        st.font_key = Some(key);
        st.font_face = Some(face.clone());
    }
    Some(())
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
    let drawn = unsafe {
        IDWriteBitmapRenderTarget::from_raw_borrowed(&this)
            .zip(run.as_ref())
            .and_then(|(brt, r)| dgr_render(brt, r, (bx, by), mm, color))
    };
    if let Some(ink) = drawn {
        // SAFETY: `bbox` is null or the caller's RECT, live for the call.
        if let Some(b) = unsafe { bbox.as_mut() } {
            *b = ink;
        }
        return HRESULT(0); // S_OK
    }
    // SAFETY: the app's own arguments, forwarded.
    unsafe { dgr_os(this, bx, by, mm, run, rp, color, bbox) }
}

/// `textColor` is a `COLORREF`.
fn rgb(colorref: u32) -> [u8; 3] {
    let [r, g, b, _] = colorref.to_le_bytes();
    [r, g, b]
}

/// Rasterise `run` into the render target's bitmap with render-core, laid
/// out as DirectWrite would (`layout.rs`). Returns the ink rectangle for
/// `blackBoxRect`, or `None` to leave the run to DirectWrite (a rotated or
/// skewed target transform, an unreadable font, no render state).
fn dgr_render(brt: &IDWriteBitmapRenderTarget, run: &DWRITE_GLYPH_RUN, baseline: (f32, f32), mm: i32, color: u32) -> Option<RECT> {
    let face = run.fontFace.as_ref()?;
    // SAFETY: getters on a live render target.
    let (hdc, size, ppd, m) = unsafe {
        let mut m = DWRITE_MATRIX::default();
        brt.GetCurrentTransform(&raw mut m).ok()?;
        (brt.GetMemoryDC(), brt.GetSize().ok()?, brt.GetPixelsPerDip(), m)
    };
    let map = Mapping::new(&m, ppd)?;
    // SAFETY: `run` is DirectWrite's argument, live for the call.
    let geo = unsafe { layout::lay_out(run, baseline, &map, mm) }?;
    // The render lock serialises every draw; held until the bitmap is written.
    let mut guard = RENDER.lock().ok()?;
    let st = guard.as_mut()?;
    reface(st, face, "dw")?;
    let rendered = render_placed(&st.ft, &st.profile, &geo.glyphs, &geo.style);
    let Some(ink) = rendered.bounds else {
        // No ink (spaces): DirectWrite reports the empty rectangle at the
        // baseline origin.
        let (x, y) = geo.origin;
        return Some(RECT { left: x, top: y, right: x, bottom: y });
    };
    // Only the part inside the bitmap is read back and written; the reported
    // rectangle is not clipped (DirectWrite's is not either).
    let (left, top, right, bottom) = (ink.0.max(0), ink.1.max(0), ink.2.min(size.cx), ink.3.min(size.cy));
    if left < right && top < bottom {
        let mut dib = Dib::new(hdc, right - left, bottom - top)?;
        dib.copy_from(hdc, left, top);
        let mut canvas = dib.canvas();
        rendered.draw_onto(&mut canvas, (left, top), &st.tables, &st.profile, Ink { fg: rgb(color) });
        dib.blit(&canvas);
        dib.copy_to(hdc, left, top);
    }
    drop(guard);
    if !CAPTURED.swap(true, Ordering::SeqCst) {
        log(&format!("substituted DirectWrite DrawGlyphRun via render-core ({} glyphs)", geo.glyphs.len()));
    }
    Some(RECT { left: ink.0, top: ink.1, right: ink.2, bottom: ink.3 })
}

/// Hand the run back to DirectWrite the way upstream's
/// `IMPL_BitmapRenderTarget_DrawGlyphRun` does: the profile's rendering
/// params in place of the app's, and - with grid fitting off - a transform
/// nudged by 1/65535 so DirectWrite stops snapping stems. Each step falls
/// back to the next if DirectWrite refuses it; the last is the app's call
/// unchanged.
///
/// # Safety
/// The arguments must be those of a `DrawGlyphRun` call on `this`.
#[allow(clippy::too_many_arguments)] // DrawGlyphRun's own eight parameters
unsafe fn dgr_os(
    this: *mut c_void, bx: f32, by: f32, mm: i32, run: *const DWRITE_GLYPH_RUN, rp: *mut c_void, color: u32, bbox: *mut RECT,
) -> HRESULT {
    // `dw` owns a reference to the params for the whole call: a profile
    // reload may replace `DW_RENDERING` meanwhile, and the raw pointer
    // handed to DirectWrite must not outlive its owner.
    let dw = dw_rendering();
    let params = dw.as_ref().map_or(rp, |d| d.dw_params.as_raw());
    // SAFETY: per the contract above.
    unsafe {
        let mut hr = E_FAIL;
        if dw.as_ref().is_some_and(|d| d.grid_fit_disabled) {
            if let Some(prev) = nudge_transform(this) {
                hr = (orig(&ORIG_DGR))(this, bx, by, mm, run, params, color, bbox);
                set_transform(this, &prev);
            }
        }
        if hr.is_err() {
            hr = (orig(&ORIG_DGR))(this, bx, by, mm, run, params, color, bbox);
        }
        if hr.is_err() {
            hr = (orig(&ORIG_DGR))(this, bx, by, mm, run, rp, color, bbox);
        }
        hr
    }
}

/// `IDWriteBitmapRenderTarget::GetCurrentTransform` / `SetCurrentTransform`
/// (vtable slots 4 and 5).
type FnGetTransform = unsafe extern "system" fn(*mut c_void, *mut DWRITE_MATRIX) -> HRESULT;
type FnSetTransform = unsafe extern "system" fn(*mut c_void, *const DWRITE_MATRIX) -> HRESULT;

/// Tilt the render target's transform by 1/65535 (upstream's grid-fit
/// nudge). Returns the transform to restore, or `None` if it was not changed.
///
/// # Safety
/// `this` must be a live `IDWriteBitmapRenderTarget`.
unsafe fn nudge_transform(this: *mut c_void) -> Option<DWRITE_MATRIX> {
    // SAFETY: a live COM object's first word is its vtable; slots 4 and 5 are
    // Get/SetCurrentTransform, whose signatures the types above name.
    unsafe {
        let vtbl = *this.cast::<*const *const ()>();
        let get = std::mem::transmute::<*const (), FnGetTransform>(*vtbl.add(4));
        let mut prev = DWRITE_MATRIX::default();
        get(this, &raw mut prev).ok().ok()?;
        let mut tilted = prev;
        tilted.m12 += 1.0 / 65535.0;
        tilted.m21 += 1.0 / 65535.0;
        set_transform(this, &tilted).then_some(prev)
    }
}

/// # Safety
/// As `nudge_transform`.
unsafe fn set_transform(this: *mut c_void, m: &DWRITE_MATRIX) -> bool {
    // SAFETY: as in `nudge_transform`.
    unsafe {
        let vtbl = *this.cast::<*const *const ()>();
        let set = std::mem::transmute::<*const (), FnSetTransform>(*vtbl.add(5));
        set(this, m).is_ok()
    }
}

// ---- CreateGlyphRunAnalysis → GetAlphaTextureBounds / CreateAlphaTexture ----
//
// Apps that composite glyphs themselves (WPF, measured; Chromium/Skia, which
// cannot load this DLL) ask DirectWrite for a glyph-run *analysis* and then
// for its alpha texture. Every creation is laid out here (`layout.rs`) and
// remembered by the analysis's address; its bounds and texture then come
// from render-core, so the caller's buffer is sized from the same glyphs it
// is filled with.
//
// Runs we cannot rasterise (a rotated or skewed transform) are left to
// DirectWrite, created the way upstream's `IMPL_CreateGlyphRunAnalysis{,2,3}`
// create them: with the profile's rendering mode and grid fit, and the
// 1/65535 transform nudge when grid fitting is off, each step falling back to
// the app's own arguments. Upstream's `IMPL_GetAlphaBlendParams` is ported
// too: the blend values the caller composites with come from the profile.

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
/// `IDWriteGlyphRunAnalysis::GetAlphaTextureBounds(textureType, bounds)` (slot 3).
type FnGetAlphaTextureBounds = unsafe extern "system" fn(*mut c_void, i32, *mut RECT) -> HRESULT;
/// `IDWriteGlyphRunAnalysis::CreateAlphaTexture(textureType, bounds, alphaValues, bufferSize)` (slot 4).
type FnCreateAlphaTexture = unsafe extern "system" fn(*mut c_void, i32, *const RECT, *mut u8, u32) -> HRESULT;
/// `IDWriteGlyphRunAnalysis::GetAlphaBlendParams(renderingParams, gamma,
/// enhancedContrast, clearTypeLevel)` (slot 5).
type FnGetAlphaBlendParams = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut f32, *mut f32, *mut f32) -> HRESULT;

static ORIG_CGRA: OnceLock<FnCreateGlyphRunAnalysis> = OnceLock::new();
static ORIG_CGRA2: OnceLock<FnCreateGlyphRunAnalysis2> = OnceLock::new();
static ORIG_CGRA3: OnceLock<FnCreateGlyphRunAnalysis2> = OnceLock::new();
static ORIG_GATB: OnceLock<FnGetAlphaTextureBounds> = OnceLock::new();
static ORIG_CAT: OnceLock<FnCreateAlphaTexture> = OnceLock::new();
static ORIG_GABP: OnceLock<FnGetAlphaBlendParams> = OnceLock::new();
/// The analysis vtable is patched from the first analysis any overload
/// creates, once; `Once` also keeps two threads from both capturing a slot
/// that already holds our detour.
static ANALYSIS_VTABLE: Once = Once::new();

const SLOT_CGRA: usize = 23;
const SLOT_CGRA2: usize = 30;
const SLOT_CGRA3: usize = 31;
const SLOT_GATB: usize = 3;
const SLOT_CAT: usize = 4;
const SLOT_GABP: usize = 5;
/// `DWRITE_RENDERING_MODE_ALIASED` and `DWRITE_RENDERING_MODE1_ALIASED`.
const MODE_ALIASED: i32 = 1;
/// `DWRITE_GRID_FIT_MODE_DEFAULT` and `DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE`.
const GRID_FIT_DEFAULT: i32 = 0;
const AA_CLEARTYPE: i32 = 0;
/// `DWRITE_TEXTURE_ALIASED_1x1` (one byte per pixel) and
/// `DWRITE_TEXTURE_CLEARTYPE_3x1` (three).
const TEXTURE_ALIASED_1X1: i32 = 0;
const TEXTURE_CLEARTYPE_3X1: i32 = 1;
/// The skew upstream adds to take DirectWrite off its grid-fitted path.
const NUDGE: f32 = 1.0 / 65535.0;
const IDENTITY: DWRITE_MATRIX = DWRITE_MATRIX { m11: 1.0, m12: 0.0, m21: 0.0, m22: 1.0, dx: 0.0, dy: 0.0 };

/// An analysis we rasterise: the face (kept alive by the clone), the glyphs
/// in device pixels, and the run once rendered (greyscale or not).
struct Analysis {
    face: IDWriteFontFace,
    glyphs: Vec<Placed>,
    style: GlyphStyle,
    rendered: Option<(bool, RenderedRun)>,
}

/// Analysis address → what it was made from. An address is reused only after
/// the analysis is released, and every creation passes a hook here that
/// overwrites or removes its entry, so a stale entry is never read. Analyses
/// are never announced as released, so the oldest entries are dropped past
/// `ANALYSES_CAP` (an evicted analysis then falls back to DirectWrite for
/// both its bounds and its texture, which stay consistent).
struct Analyses {
    map: HashMap<usize, Analysis>,
    order: VecDeque<usize>,
}

// SAFETY: the only non-Send member is the IDWriteFontFace, and DirectWrite's
// shared-factory objects are free-threaded; it is used only under the mutex.
unsafe impl Send for Analyses {}

static ANALYSES: Mutex<Option<Analyses>> = Mutex::new(None);
const ANALYSES_CAP: usize = 256;

/// `transform` with upstream's grid-fit nudge applied, written to `m`
/// (identity plus the nudge when there is no transform).
///
/// # Safety
/// `transform` must be null or point at a live matrix.
unsafe fn nudged(transform: *const DWRITE_MATRIX, m: &mut DWRITE_MATRIX) -> *const DWRITE_MATRIX {
    // SAFETY: per the contract above.
    *m = unsafe { transform.as_ref() }.copied().unwrap_or(IDENTITY);
    m.m12 += NUDGE;
    m.m21 += NUDGE;
    m
}

/// Method `slot` of COM object `obj`, typed as `F`.
///
/// # Safety
/// `obj` must be a live COM object whose vtable has `slot`, and `F` that
/// method's exact function-pointer type.
unsafe fn method<F: Copy>(obj: *mut c_void, slot: usize) -> F {
    // SAFETY: per the contract above.
    unsafe {
        let vtbl = *obj.cast::<*const *const ()>();
        std::mem::transmute_copy::<*const (), F>(&*vtbl.add(slot))
    }
}

unsafe extern "system" fn cgra_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, ppd: f32, transform: *const DWRITE_MATRIX, rmode: i32, mmode: i32,
    bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    let dw = dw_rendering();
    // SAFETY: `this` is the factory the app called; the other arguments are
    // the app's, forwarded under DirectWrite's contract for this call.
    unsafe {
        // Upstream: prefer the Factory2 overload (called through the vtable,
        // so it lands in `cgra2_detour`) with pixelsPerDip folded into the
        // transform; else the profile's rendering mode here; else the app's
        // own arguments.
        let mut hr = E_FAIL;
        if rmode != MODE_ALIASED {
            if let Some(f2) = IDWriteFactory::from_raw_borrowed(&this).and_then(|f| f.cast::<IDWriteFactory2>().ok()) {
                let m = match transform.as_ref() {
                    Some(t) => DWRITE_MATRIX {
                        m11: t.m11 * ppd,
                        m12: t.m12 * ppd,
                        m21: t.m21 * ppd,
                        m22: t.m22 * ppd,
                        dx: t.dx * ppd,
                        dy: t.dy * ppd,
                    },
                    None => DWRITE_MATRIX { m11: ppd, m22: ppd, ..Default::default() },
                };
                let f: FnCreateGlyphRunAnalysis2 = method(f2.as_raw(), SLOT_CGRA2);
                let aa = dw.as_ref().map_or(AA_CLEARTYPE, |d| d.dw_aa);
                hr = f(f2.as_raw(), run, &raw const m, rmode, mmode, GRID_FIT_DEFAULT, aa, bx, by, out);
            }
        }
        if let Some(d) = dw.as_ref().filter(|_| hr.is_err() && rmode != MODE_ALIASED) {
            let mut m = IDENTITY;
            let pm = if d.grid_fit_disabled { nudged(transform, &mut m) } else { transform };
            hr = (orig(&ORIG_CGRA))(this, run, ppd, pm, d.dw_mode, mmode, bx, by, out);
        }
        if hr.is_err() {
            hr = (orig(&ORIG_CGRA))(this, run, ppd, transform, rmode, mmode, bx, by, out);
        }
        // Factory 1's transform is in DIPs, scaled by pixelsPerDip after it
        // (measured: ppd 1.5 with a (10, 5) translation puts the origin at
        // (75, 97.5)).
        adopt_analysis(hr, out, run, transform, ppd, (bx, by), mmode);
        hr
    }
}

unsafe extern "system" fn cgra2_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, transform: *const DWRITE_MATRIX, rmode: i32, mmode: i32, grid: i32,
    aa: i32, bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    let dw = dw_rendering();
    // The profile decides the antialiasing, as it does for GDI text: a
    // greyscale profile gets a greyscale analysis, whose only texture is the
    // 1x1 one. (Upstream keeps the app's mode.) A ClearType 3x1 texture holds
    // coverage at three subpixel positions, and callers shift it by
    // subpixels to place glyphs at fractional positions (WPF does, measured),
    // so a greyscale look cannot be delivered through it.
    let aa_p = dw.as_ref().map_or(aa, |d| d.dw_aa);
    // SAFETY: as in `cgra_detour`.
    unsafe {
        // Upstream: prefer the Factory3 overload (-> `cgra3_detour`); else the
        // profile's rendering mode and grid fit here; else the app's modes with
        // the grid-fit nudge; else the app's own arguments.
        let mut hr = E_FAIL;
        if rmode != MODE_ALIASED {
            if let Some(f3) = IDWriteFactory::from_raw_borrowed(&this).and_then(|f| f.cast::<IDWriteFactory3>().ok()) {
                let f: FnCreateGlyphRunAnalysis2 = method(f3.as_raw(), SLOT_CGRA3);
                hr = f(f3.as_raw(), run, transform, rmode, mmode, grid, aa_p, bx, by, out);
            }
        }
        if let Some(d) = dw.as_ref() {
            if hr.is_err() && rmode != MODE_ALIASED {
                hr = (orig(&ORIG_CGRA2))(this, run, transform, d.dw_mode, mmode, d.grid_fit, aa_p, bx, by, out);
            }
            if hr.is_err() {
                let mut m = IDENTITY;
                let pm = if d.grid_fit_disabled { nudged(transform, &mut m) } else { transform };
                hr = (orig(&ORIG_CGRA2))(this, run, pm, rmode, mmode, grid, aa_p, bx, by, out);
            }
        }
        if hr.is_err() {
            hr = (orig(&ORIG_CGRA2))(this, run, transform, rmode, mmode, grid, aa, bx, by, out);
        }
        // Factories 2 and 3 take no pixelsPerDip: it is in the transform,
        // whose translation is in pixels.
        adopt_analysis(hr, out, run, transform, 1.0, (bx, by), mmode);
        hr
    }
}

unsafe extern "system" fn cgra3_detour(
    this: *mut c_void, run: *const DWRITE_GLYPH_RUN, transform: *const DWRITE_MATRIX, rmode1: i32, mmode: i32, grid: i32,
    aa: i32, bx: f32, by: f32, out: *mut *mut c_void,
) -> HRESULT {
    let dw = dw_rendering();
    // The profile's antialiasing, as in `cgra2_detour`.
    let aa_p = dw.as_ref().map_or(aa, |d| d.dw_aa);
    // SAFETY: as in `cgra_detour`.
    unsafe {
        // Upstream: the profile's rendering mode and grid fit; else the app's
        // modes with the grid-fit nudge; else the app's own arguments.
        let mut hr = E_FAIL;
        if let Some(d) = dw.as_ref() {
            if rmode1 != MODE_ALIASED {
                hr = (orig(&ORIG_CGRA3))(this, run, transform, d.dw_mode1, mmode, d.grid_fit, aa_p, bx, by, out);
            }
            if hr.is_err() {
                let mut m = IDENTITY;
                let pm = if d.grid_fit_disabled { nudged(transform, &mut m) } else { transform };
                hr = (orig(&ORIG_CGRA3))(this, run, pm, rmode1, mmode, grid, aa_p, bx, by, out);
            }
        }
        if hr.is_err() {
            hr = (orig(&ORIG_CGRA3))(this, run, transform, rmode1, mmode, grid, aa, bx, by, out);
        }
        adopt_analysis(hr, out, run, transform, 1.0, (bx, by), mmode);
        hr
    }
}

/// After a successful creation: make sure the analysis vtable is ours, then
/// remember the run behind the new analysis — or forget whatever an earlier
/// analysis at the same address left, when this run is DirectWrite's to draw.
///
/// # Safety
/// The arguments are the creation call's: `out` holds the new analysis when
/// `hr` succeeded, and `run` / `transform` are live for the call.
unsafe fn adopt_analysis(
    hr: HRESULT, out: *mut *mut c_void, run: *const DWRITE_GLYPH_RUN, transform: *const DWRITE_MATRIX, ppd: f32,
    baseline: (f32, f32), mmode: i32,
) {
    if hr.is_err() {
        return;
    }
    // SAFETY: per the contract above.
    let Some(analysis) = (unsafe { out.as_ref() }).copied().filter(|p| !p.is_null()) else { return };
    ANALYSIS_VTABLE.call_once(|| {
        // SAFETY: `analysis` is live; slots 3-5 are the methods the types name.
        unsafe { patch_analysis_vtable(analysis) };
    });
    // SAFETY: per the contract above.
    let entry = unsafe {
        let m = transform.as_ref().copied().unwrap_or(IDENTITY);
        run.as_ref().zip(Mapping::new(&m, ppd)).and_then(|(r, map)| {
            let face = r.fontFace.as_ref()?.clone();
            let geo = layout::lay_out(r, baseline, &map, mmode)?;
            Some(Analysis { face, glyphs: geo.glyphs, style: geo.style, rendered: None })
        })
    };
    let Ok(mut guard) = ANALYSES.lock() else { return };
    let all = guard.get_or_insert_with(|| Analyses { map: HashMap::new(), order: VecDeque::new() });
    let key = analysis.addr();
    match entry {
        Some(e) => {
            if all.map.insert(key, e).is_none() {
                all.order.push_back(key);
            }
            while all.map.len() > ANALYSES_CAP {
                let Some(old) = all.order.pop_front() else { break };
                all.map.remove(&old);
            }
        }
        None => {
            if all.map.remove(&key).is_some() {
                all.order.retain(|&k| k != key);
            }
        }
    }
}

/// Patch GetAlphaTextureBounds (3), CreateAlphaTexture (4) and
/// GetAlphaBlendParams (5) in the analysis vtable.
///
/// # Safety
/// `analysis` must be a live `IDWriteGlyphRunAnalysis`.
unsafe fn patch_analysis_vtable(analysis: *mut c_void) {
    // SAFETY: a live COM object's first word is its vtable; each transmute
    // types the slot's previous value, which is the method that slot names.
    unsafe {
        let vtbl = *analysis.cast::<*mut *const ()>();
        let ok = patch_slot(vtbl.add(SLOT_GATB), gatb_detour as *const (), |old| {
            let _ = ORIG_GATB.set(std::mem::transmute::<*const (), FnGetAlphaTextureBounds>(old));
        }) && patch_slot(vtbl.add(SLOT_CAT), cat_detour as *const (), |old| {
            let _ = ORIG_CAT.set(std::mem::transmute::<*const (), FnCreateAlphaTexture>(old));
        }) && patch_slot(vtbl.add(SLOT_GABP), gabp_detour as *const (), |old| {
            let _ = ORIG_GABP.set(std::mem::transmute::<*const (), FnGetAlphaBlendParams>(old));
        });
        log(if ok { "hook installed on IDWriteGlyphRunAnalysis (3 slots)" } else { "IDWriteGlyphRunAnalysis hook failed" });
    }
}

unsafe extern "system" fn gatb_detour(this: *mut c_void, ty: i32, bounds: *mut RECT) -> HRESULT {
    // SAFETY: the app's arguments, forwarded; `bounds` is its out-parameter.
    unsafe {
        let hr = (orig(&ORIG_GATB))(this, ty, bounds);
        // DirectWrite answers an empty rectangle for the texture type this
        // analysis does not produce (1x1 for ClearType, 3x1 for greyscale or
        // aliased — measured) and for a run with no ink; keep those as-is.
        let Some(b) = bounds.as_mut().filter(|b| hr.is_ok() && b.right > b.left && b.bottom > b.top) else { return hr };
        if let Some(ours) = with_rendered(this.addr(), ty, |r, _| r.bounds) {
            *b = ours.map_or_else(RECT::default, |(left, top, right, bottom)| RECT { left, top, right, bottom });
        }
        hr
    }
}

unsafe extern "system" fn cat_detour(this: *mut c_void, ty: i32, bounds: *const RECT, alpha: *mut u8, size: u32) -> HRESULT {
    let channels = match ty {
        TEXTURE_CLEARTYPE_3X1 => 3,
        TEXTURE_ALIASED_1X1 => 1,
        _ => 0,
    };
    // SAFETY: DirectWrite's contract for CreateAlphaTexture — `bounds` is one
    // RECT and `alpha` holds `size` bytes, for the call's duration.
    let target = unsafe { bounds.as_ref().zip((!alpha.is_null()).then(|| core::slice::from_raw_parts_mut(alpha, size as usize))) };
    let filled = channels > 0
        && target.is_some_and(|(b, out)| {
            let (w, h) = (usize::try_from(b.right - b.left).unwrap_or(0), usize::try_from(b.bottom - b.top).unwrap_or(0));
            w * h * channels == out.len()
                && with_rendered(this.addr(), ty, |r, bgr| {
                    out.copy_from_slice(&r.coverage((b.left, b.top, b.right, b.bottom), channels, bgr));
                })
                .is_some()
        });
    if filled {
        if !CAPTURED.swap(true, Ordering::SeqCst) {
            log("substituted CreateAlphaTexture via render-core");
        }
        return HRESULT(0);
    }
    // SAFETY: arguments forwarded untouched.
    unsafe { (orig(&ORIG_CAT))(this, ty, bounds, alpha, size) }
}

/// Run `f` on analysis `key`'s run rendered for texture `ty`, rendering it
/// on first use: greyscale for the 1x1 texture, and subpixel (LCD) coverage
/// for the ClearType 3x1 one — its three values are coverage at three
/// subpixel positions, so even a greyscale profile renders LCD here (a
/// greyscale profile normally never gets a 3x1 texture; see
/// `cgra2_detour`). `None` when the analysis is not ours or cannot be rendered —
/// the caller then leaves it to DirectWrite (and a failed render forgets the
/// analysis, so its bounds and texture both stay DirectWrite's).
fn with_rendered<T>(key: usize, ty: i32, f: impl FnOnce(&RenderedRun, bool) -> T) -> Option<T> {
    let mut guard = ANALYSES.lock().ok()?;
    let all = guard.as_mut()?;
    let a = all.map.get_mut(&key)?;
    let grey = ty == TEXTURE_ALIASED_1X1;
    // Lock order: ANALYSES, then RENDER (nothing takes them the other way).
    let mut render = RENDER.lock().ok()?;
    let st = render.as_mut()?;
    let profile = if grey {
        Profile { aa: Aa::Grey, ..st.profile }
    } else if st.profile.aa.is_lcd() {
        st.profile
    } else {
        Profile { aa: Aa::LcdRgb, ..st.profile }
    };
    if a.rendered.as_ref().is_none_or(|(g, _)| *g != grey) {
        if reface(st, &a.face, "dw").is_none() {
            all.map.remove(&key);
            return None;
        }
        a.rendered = Some((grey, render_placed(&st.ft, &profile, &a.glyphs, &a.style)));
    }
    let bgr = render_core::ft::is_bgr(profile.aa);
    let out = a.rendered.as_ref().map(|(_, r)| f(r, bgr));
    drop(render);
    drop(guard);
    out
}

unsafe extern "system" fn gabp_detour(this: *mut c_void, rp: *mut c_void, gamma: *mut f32, contrast: *mut f32, level: *mut f32) -> HRESULT {
    // Upstream's `IMPL_GetAlphaBlendParams`: the blend values for the
    // profile's rendering params, else for the app's.
    let dw = dw_rendering();
    // SAFETY: arguments are the app's, forwarded; `dw` keeps the params alive.
    unsafe {
        let mut hr = E_FAIL;
        if let Some(d) = dw.as_ref() {
            hr = (orig(&ORIG_GABP))(this, d.dw_params.as_raw(), gamma, contrast, level);
        }
        if hr.is_err() {
            hr = (orig(&ORIG_GABP))(this, rp, gamma, contrast, level);
        }
        hr
    }
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
        if patch_slot(fvtbl.add(SLOT_CGRA), cgra_detour as *const (), |old| {
            let _ = ORIG_CGRA.set(std::mem::transmute::<*const (), FnCreateGlyphRunAnalysis>(old));
        }) {
            log("hook installed on CreateGlyphRunAnalysis");
        }
        if factory.cast::<IDWriteFactory2>().is_ok()
            && patch_slot(fvtbl.add(SLOT_CGRA2), cgra2_detour as *const (), |old| {
                let _ = ORIG_CGRA2.set(std::mem::transmute::<*const (), FnCreateGlyphRunAnalysis2>(old));
            })
        {
            log("hook installed on IDWriteFactory2::CreateGlyphRunAnalysis");
        }
        if factory.cast::<IDWriteFactory3>().is_ok()
            && patch_slot(fvtbl.add(SLOT_CGRA3), cgra3_detour as *const (), |old| {
                let _ = ORIG_CGRA3.set(std::mem::transmute::<*const (), FnCreateGlyphRunAnalysis2>(old));
            })
        {
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
