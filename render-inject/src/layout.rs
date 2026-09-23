//! Where DirectWrite puts each glyph of a `DWRITE_GLYPH_RUN`, in device
//! pixels — shared by every path that rasterises a run itself (the bitmap
//! render target, the glyph-run analysis, Direct2D).
//!
//! The rules, each measured against plain DirectWrite with
//! `verify/dwrite-probe`:
//!
//! * The pen starts at the baseline origin and moves by `glyphAdvances`, or
//!   by the font's own advances when that is null (design metrics for the
//!   natural measuring mode, GDI-compatible ones for the GDI modes).
//! * `advanceOffset` shifts a glyph along the reading direction and
//!   `ascenderOffset` shifts it up.
//! * An odd `bidiLevel` runs right to left: the glyph's right edge — by its
//!   *own* advance, not the run's — sits on the pen, then the pen moves left
//!   by the run's advance; a positive `advanceOffset` moves it left. (With
//!   12-DIP advances "AVATo" ends at the origin minus the last glyph's own
//!   width, not minus 60.)
//! * `isSideways` turns each glyph 90 degrees counter-clockwise, advances by
//!   vertical metrics, and centres it on the baseline: its vertical origin
//!   `(advanceWidth / 2, verticalOriginY)` lands on the pen.
//! * Device pixel = pixelsPerDip x (transform x DIP point). Only a uniform
//!   positive scale plus translation is handled here; any other transform is
//!   left to DirectWrite.

use render_core::{GlyphStyle, Placed};
use windows::Win32::Graphics::DirectWrite::{
    IDWriteFontFace, DWRITE_FONT_METRICS, DWRITE_FONT_SIMULATIONS_BOLD, DWRITE_FONT_SIMULATIONS_OBLIQUE,
    DWRITE_GLYPH_METRICS, DWRITE_GLYPH_RUN, DWRITE_MATRIX, DWRITE_MEASURING_MODE_GDI_NATURAL,
    DWRITE_MEASURING_MODE_NATURAL,
};

use crate::state::round_i32;

/// Below this, a transform's off-diagonal terms (or the difference between
/// its two scales) are treated as zero. Upstream's own grid-fit nudge is
/// 1/65535, well under it.
const EPSILON: f32 = 1.0e-4;

/// DIP → device pixel, for a uniform scale plus translation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Mapping {
    scale: f32,
    dx: f32,
    dy: f32,
}

impl Mapping {
    /// `pixel = ppd * (m * dip)`, if `m` is a uniform positive scale plus a
    /// translation; `None` for rotation, skew, mirroring or unequal scales.
    pub(crate) fn new(m: &DWRITE_MATRIX, ppd: f32) -> Option<Mapping> {
        let uniform = m.m12.abs() < EPSILON
            && m.m21.abs() < EPSILON
            && m.m11 > 0.0
            && (m.m11 - m.m22).abs() < EPSILON * m.m11.max(1.0);
        (uniform && ppd > 0.0 && ppd.is_finite()).then_some(Mapping { scale: ppd * m.m11, dx: ppd * m.dx, dy: ppd * m.dy })
    }

    fn apply(&self, x: f32, y: f32) -> (i32, i32) {
        (round_i32(self.scale * x + self.dx), round_i32(self.scale * y + self.dy))
    }
}

/// A run laid out in device pixels.
pub(crate) struct RunGeometry {
    pub(crate) glyphs: Vec<Placed>,
    pub(crate) style: GlyphStyle,
    /// The baseline origin in device pixels (DirectWrite's `blackBoxRect`
    /// for a run with no ink is the empty rectangle there).
    pub(crate) origin: (i32, i32),
}

/// Lay out `run` drawn at `baseline` (DIPs) with `measuring` mode.
///
/// # Safety
/// `run` must be a run DirectWrite handed to the caller: `glyphIndices` and,
/// when non-null, `glyphAdvances` / `glyphOffsets` hold `glyphCount` entries
/// for the call's duration.
pub(crate) unsafe fn lay_out(run: &DWRITE_GLYPH_RUN, baseline: (f32, f32), map: &Mapping, measuring: i32) -> Option<RunGeometry> {
    let face: &IDWriteFontFace = run.fontFace.as_ref()?;
    let n = run.glyphCount as usize;
    if run.glyphIndices.is_null() || run.fontEmSize.is_nan() || run.fontEmSize <= 0.0 {
        return None;
    }
    // SAFETY: per the contract above.
    let (indices, advances, offsets) = unsafe {
        (
            core::slice::from_raw_parts(run.glyphIndices, n),
            (!run.glyphAdvances.is_null()).then(|| core::slice::from_raw_parts(run.glyphAdvances, n)),
            (!run.glyphOffsets.is_null()).then(|| core::slice::from_raw_parts(run.glyphOffsets, n)),
        )
    };
    let sideways = run.isSideways.as_bool();
    let rtl = run.bidiLevel & 1 == 1;
    let em = run.fontEmSize;

    let mut fm = DWRITE_FONT_METRICS::default();
    // SAFETY: a getter on a live face.
    unsafe { face.GetMetrics(&raw mut fm) };
    if fm.designUnitsPerEm == 0 {
        return None;
    }
    let per_unit = em / f32::from(fm.designUnitsPerEm);
    // Design metrics: the sideways anchor always, and the default advances
    // in the natural mode.
    let design = glyph_metrics(face, indices, sideways, None)?;
    // A glyph's own advance is needed even with `glyphAdvances` given: an RTL
    // glyph is placed by it.
    let defaults = if measuring == DWRITE_MEASURING_MODE_NATURAL.0 {
        None
    } else {
        // SAFETY: as above; GDI-compatible metrics at this run's pixel size.
        Some(glyph_metrics(face, indices, sideways, Some((em, map.scale, measuring == DWRITE_MEASURING_MODE_GDI_NATURAL.0)))?)
    };
    let default_advance = |i: usize| {
        let m = defaults.as_ref().map_or(&design[i], |d| &d[i]);
        #[allow(clippy::cast_precision_loss)] // design units are far below 2^24
        let units = if sideways { m.advanceHeight } else { m.advanceWidth } as f32;
        units * per_unit
    };

    let mut glyphs = Vec::with_capacity(n);
    let mut pen = 0.0f32;
    for (i, &gi) in indices.iter().enumerate() {
        let adv = advances.map_or_else(|| default_advance(i), |a| a[i]);
        let (along, up) = offsets.map_or((0.0, 0.0), |o| (o[i].advanceOffset, o[i].ascenderOffset));
        let mut x = if rtl {
            let x = pen - default_advance(i) - along;
            pen -= adv;
            x
        } else {
            let x = pen + along;
            pen += adv;
            x
        };
        let mut y = -up;
        if sideways {
            // Put the vertical origin, turned a quarter counter-clockwise, on
            // the pen: the horizontal origin then sits at (+vOY, +aw/2).
            #[allow(clippy::cast_precision_loss)]
            let (voy, half_aw) = (design[i].verticalOriginY as f32, design[i].advanceWidth as f32 / 2.0);
            x += voy * per_unit;
            y += half_aw * per_unit;
        }
        let (px, py) = map.apply(baseline.0 + x, baseline.1 + y);
        glyphs.push(Placed { gi, x: px, y: py });
    }

    // SAFETY: a getter on a live face.
    let sims = unsafe { face.GetSimulations() };
    #[allow(clippy::cast_possible_truncation)] // a pixel em size, far below i64::MAX / 64
    let size_26_6 = (em * map.scale * 64.0).round() as i64;
    let style = GlyphStyle {
        size_26_6,
        sideways,
        bold: sims.0 & DWRITE_FONT_SIMULATIONS_BOLD.0 != 0,
        oblique: sims.0 & DWRITE_FONT_SIMULATIONS_OBLIQUE.0 != 0,
    };
    Some(RunGeometry { glyphs, style, origin: map.apply(baseline.0, baseline.1) })
}

/// Design metrics of `indices`, or — with `gdi = (emSize, pixelsPerDip,
/// useGdiNatural)` — the GDI-compatible ones DirectWrite uses for the GDI
/// measuring modes.
fn glyph_metrics(face: &IDWriteFontFace, indices: &[u16], sideways: bool, gdi: Option<(f32, f32, bool)>) -> Option<Vec<DWRITE_GLYPH_METRICS>> {
    let mut out = vec![DWRITE_GLYPH_METRICS::default(); indices.len()];
    let count = u32::try_from(indices.len()).ok()?;
    // SAFETY: `indices` and `out` both hold `count` entries.
    unsafe {
        match gdi {
            None => face.GetDesignGlyphMetrics(indices.as_ptr(), count, out.as_mut_ptr(), sideways).ok()?,
            Some((em, ppd, natural)) => face
                .GetGdiCompatibleGlyphMetrics(em, ppd, None, natural, indices.as_ptr(), count, out.as_mut_ptr(), sideways)
                .ok()?,
        }
    }
    Some(out)
}
