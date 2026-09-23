//! The DC's logical-to-device mapping, and what the port does with it.
//!
//! `ExtTextOutW` takes logical coordinates: the map mode (`SetMapMode` +
//! window/viewport extents) and the world transform (`SetWorldTransform` under
//! `GM_ADVANCED`) both apply. render-core rasterises in device pixels, so a DC
//! with a mapping needs the whole run converted before it is drawn - and the
//! result blitted back device-for-device.
//!
//! Getting that wrong is not a harmless offset. Until 0.1.8 the port ignored
//! the mapping entirely and let the final `BitBlt` stretch its output: the
//! geometry came out right by accident, but the glyphs were a small rendering
//! blown up by nearest-neighbour. Measured on a 2x DC, every 2x2 pixel block
//! of our output was uniform (96 uniform, 0 mixed) where plain GDI produced
//! 105 mixed blocks from a genuine double-size rendering. That is worse than
//! not hooking at all.
//!
//! Upstream reaches the same conclusion (`override.cpp` 1200-1217, whose
//! `GetMapMode`/`GetWorldTransform` block just below it is commented out and
//! dead): read the combined world-to-device transform, redraw at the scaled
//! size when it is a positive axis-aligned scale, and hand anything else
//! (rotation, shear, mirroring) back to GDI.

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{LPtoDP, HDC};

/// A positive, axis-aligned logical-to-device mapping: the only kind the port
/// draws itself. `sx`/`sy` are the scale factors; the translation is not kept,
/// because callers convert points with [`Mapping::to_device`] instead of
/// applying the factors by hand.
#[derive(Clone, Copy)]
pub(crate) struct Mapping {
    pub(crate) sx: f64,
    pub(crate) sy: f64,
    hdc: HDC,
}

impl Mapping {
    /// True when device units are logical units, i.e. the common case where
    /// nothing has to be converted.
    pub(crate) fn is_identity(self) -> bool {
        (self.sx - 1.0).abs() < f64::EPSILON && (self.sy - 1.0).abs() < f64::EPSILON
    }

    /// Scale a logical length along x (advances, widths).
    pub(crate) fn len_x(self, v: i32) -> i32 {
        scale(v, self.sx)
    }

    /// Scale a logical length along y (ascent, height).
    pub(crate) fn len_y(self, v: i32) -> i32 {
        scale(v, self.sy)
    }

    /// Convert a logical point to device pixels, translation included. Uses
    /// `LPtoDP` itself so the DC's own rounding applies.
    pub(crate) fn to_device(self, x: i32, y: i32) -> (i32, i32) {
        let mut p = [POINT { x, y }];
        // SAFETY: `p` is ours and `hdc` is the app's DC, live for this call.
        if unsafe { LPtoDP(self.hdc, &mut p) }.as_bool() {
            (p[0].x, p[0].y)
        } else {
            (x, y)
        }
    }

    /// An explicit `lpDx` converted to device units. Stepping through the
    /// cumulative position (rather than scaling each entry on its own) keeps
    /// the rounding error from adding up along the run, as upstream's
    /// `TransformlpDx` does. With `pdy` the array is `(dx, dy)` pairs, so the
    /// two axes are stepped separately with their own factors.
    pub(crate) fn device_dx(self, dx: &[i32], pdy: bool) -> Vec<i32> {
        let mut out = Vec::with_capacity(dx.len());
        let (mut logical, mut device) = ([0i32; 2], [0i32; 2]);
        let factor = [self.sx, self.sy];
        for (i, &step) in dx.iter().enumerate() {
            let axis = usize::from(pdy && i % 2 == 1);
            logical[axis] += step;
            let at = scale(logical[axis], factor[axis]);
            out.push(at - device[axis]);
            device[axis] = at;
        }
        out
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn scale(v: i32, by: f64) -> i32 {
    // Bounded by the run sizes GDI accepts; `round` keeps halves off by at
    // most one pixel, which is what LPtoDP does to coordinates.
    (f64::from(v) * by).round() as i32
}

/// The DC's world-to-device mapping, or `None` when the port must not draw the
/// run itself (rotation, shear, mirroring, a degenerate mapping).
///
/// Derived from `LPtoDP` rather than gdi32's undocumented `GetTransform`:
/// probing three points recovers the same matrix (verified against
/// `GetTransform(hdc, GT_WORLD_TO_DEVICE)` on both a map-mode and a
/// world-transform DC) using only documented API.
pub(crate) fn mapping(hdc: HDC) -> Option<Mapping> {
    const UNIT: i32 = 1024; // large enough that the rounding per probe is noise
    let mut p = [POINT { x: 0, y: 0 }, POINT { x: UNIT, y: 0 }, POINT { x: 0, y: UNIT }];
    // SAFETY: `p` is ours; `hdc` is the app's DC, live for this call.
    if !unsafe { LPtoDP(hdc, &mut p) }.as_bool() {
        return None;
    }
    let unit = f64::from(UNIT);
    let (m11, m12) = (f64::from(p[1].x - p[0].x) / unit, f64::from(p[1].y - p[0].y) / unit);
    let (m21, m22) = (f64::from(p[2].x - p[0].x) / unit, f64::from(p[2].y - p[0].y) / unit);
    // Off-diagonal terms mean rotation or shear; a non-positive diagonal means
    // a mirror or a collapsed axis. GDI can draw those; render-core cannot.
    if m12.abs() > 1e-6 || m21.abs() > 1e-6 || m11 <= 0.0 || m22 <= 0.0 {
        return None;
    }
    Some(Mapping { sx: m11, sy: m22, hdc })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(sx: f64, sy: f64) -> Mapping {
        Mapping { sx, sy, hdc: HDC(std::ptr::null_mut()) }
    }

    /// With `ETO_PDY` the array interleaves x and y, so each axis steps on
    /// its own cumulative position and uses its own factor.
    #[test]
    fn device_dx_keeps_the_axes_apart_in_pdy_mode() {
        let m = fake(2.0, 3.0);
        assert_eq!(m.device_dx(&[10, 4, 10, 4], true), vec![20, 12, 20, 12]);
        // Without the flag the same array is four x advances.
        assert_eq!(m.device_dx(&[10, 4, 10, 4], false), vec![20, 8, 20, 8]);
    }

    #[test]
    fn identity_is_recognised_and_lengths_pass_through() {
        let m = fake(1.0, 1.0);
        assert!(m.is_identity());
        assert_eq!(m.len_x(17), 17);
        assert_eq!(m.len_y(13), 13);
    }

    #[test]
    fn lengths_scale_and_round() {
        let m = fake(2.0, 2.0);
        assert!(!m.is_identity());
        assert_eq!(m.len_x(17), 34);
        assert_eq!(m.len_y(13), 26);
        // 1.5x: halves round away from zero, as LPtoDP rounds coordinates.
        let m = fake(1.5, 1.5);
        assert_eq!(m.len_x(3), 5); // 4.5
        assert_eq!(m.len_x(10), 15);
    }

    /// Scaling each `lpDx` entry on its own would drift: 1.5 x 3px rounds to
    /// 5 every time, so four of them would span 20 instead of 18. Stepping
    /// through the cumulative positions (4.5, 9, 13.5, 18 -> 5, 9, 14, 18)
    /// keeps the end of the run where GDI puts it.
    #[test]
    fn device_dx_steps_through_cumulative_positions() {
        let m = fake(1.5, 1.5);
        let out = m.device_dx(&[3, 3, 3, 3], false);
        assert_eq!(out, vec![5, 4, 5, 4]);
        assert_eq!(out.iter().sum::<i32>(), 18, "= round(12 * 1.5), no drift");
    }
}
