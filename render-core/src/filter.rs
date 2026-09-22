//! Gamma / contrast / coverage lookup tables and the linear-space alpha blend.
//!
//! The math is MacType's `CAlphaBlend::init` / `CAlphaBlendColorOne::doAB`
//! (upstream MacType ft.cpp, commit 05052e8): encode both colours to linear
//! light, interpolate by the coverage alpha, decode back. Upstream does it in
//! fixed-point integers (a 2000s speed trick, no faster on modern CPUs); this
//! port computes the same formula in `f32`, rounding to the nearest byte where
//! upstream truncates. The formula is the specification, so this is correct by
//! construction; `verify/` cross-checks it against the C++ fixed-point and it
//! agrees to within one 8-bit level (this side being the more accurate).
//! A draw is two table lookups, a lerp and a short binary search — the tables
//! absorb the `powf`.

/// Gamma-encode LUT (byte → linear light) and the coverage → alpha curve.
pub struct Tables {
    encode: [f32; 256], // byte -> linear light in [0, 1]  (gamma encode)
    curve: [f32; 256],  // coverage -> alpha in [0, 1]      (contrast/weight)
}

/// sRGB electro-optical transfer: gamma-encoded byte value `x` in [0,1] to
/// linear light.
fn srgb_to_linear(x: f32) -> f32 {
    if x <= 10.0 / 255.0 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

impl Tables {
    /// Build the tables for a profile.
    ///
    /// * `gamma` – `GammaValue` (used when `mode` selects plain-power gamma)
    /// * `weight` – `RenderWeight` (coverage S-curve)
    /// * `contrast` – `Contrast`
    /// * `mode` – `GammaMode`: `<0` linear, `1` sRGB, `2` sRGB/linear avg,
    ///   else plain-power `gamma`.
    pub fn build(gamma: f32, weight: f32, contrast: f32, mode: i32) -> Tables {
        let mut encode = [0.0f32; 256];
        let mut curve = [0.0f32; 256];
        for byte in 0..=255u8 {
            let x = f32::from(byte) / 255.0;
            encode[usize::from(byte)] = match mode {
                m if m < 0 => x,                              // linear
                1 => srgb_to_linear(x),                       // sRGB
                2 => f32::midpoint(srgb_to_linear(x), x),     // sRGB / linear average
                _ => x.powf(gamma),                           // plain-power gamma
            };

            // Contrast/weight S-curve, symmetric about the midpoint.
            let t = x.powf(1.0 / weight);
            let a = if t < 0.5 {
                (t * 2.0).powf(contrast) / 2.0
            } else {
                1.0 - ((1.0 - t) * 2.0).powf(contrast) / 2.0
            };
            curve[usize::from(byte)] = a.clamp(0.0, 1.0);
        }
        Tables { encode, curve }
    }

    /// Decode a linear-light value to the gamma-encoded byte whose encoded
    /// value is nearest `linear` — the most accurate inverse of `encode`.
    /// Binary-searches the (monotonic increasing) table, so it inverts every
    /// `GammaMode`, including the sRGB/linear average that has no closed form.
    /// (Upstream truncates here instead; that is the ≤1-level difference
    /// `verify/` allows, and this side is the more accurate.)
    fn decode(&self, linear: f32) -> u8 {
        let hi = self.encode.partition_point(|&e| e < linear);
        if hi == 0 {
            return 0;
        }
        if hi >= 256 {
            return 255;
        }
        let nearest = if linear - self.encode[hi - 1] <= self.encode[hi] - linear { hi - 1 } else { hi };
        u8::try_from(nearest).unwrap_or(u8::MAX)
    }

    /// One-channel blend of foreground `fg` over background `bg` at coverage
    /// `cov` (0..=255), done in gamma-linear space. Port of
    /// `CAlphaBlendColorOne::doAB`.
    #[inline]
    pub fn blend(&self, bg: u8, fg: u8, cov: u8) -> u8 {
        let a = self.curve[usize::from(cov)];
        if a <= 0.0 {
            return bg;
        }
        let linear = self.encode[usize::from(bg)] * (1.0 - a) + self.encode[usize::from(fg)] * a;
        self.decode(linear)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints_and_monotone() {
        let t = Tables::build(1.25, 1.0, 1.0, 0);
        assert_eq!(t.blend(255, 0, 0), 255);
        assert_eq!(t.blend(255, 0, 255), 0);
        let mut prev = 256i32;
        for c in 0..=255u8 {
            let v = i32::from(t.blend(255, 0, c));
            assert!(v <= prev);
            prev = v;
        }
    }

    /// Black-on-white greyscale blend at gamma 1.25. The reference values come
    /// from the C++ oracle (verify/), which computes the same formula in
    /// fixed-point; the float port matches it within one level, which is what
    /// this asserts (endpoints exact).
    #[test]
    fn greyscale_regression_g125() {
        let t = Tables::build(1.25, 1.0, 1.0, 0);
        for &(cov, expect) in &[(0u8, 255u8), (32, 229), (64, 202), (128, 146), (192, 83), (255, 0)] {
            let got = i32::from(t.blend(255, 0, cov));
            assert!((got - i32::from(expect)).abs() <= 1, "cov={cov}: got {got}, oracle {expect}");
        }
    }

    /// Exercise every GammaMode branch of `build` (mode<0 linear, mode==1 sRGB,
    /// mode==2 sRGB/linear avg, else plain gamma), including both sides of the
    /// sRGB `x <= 10/255` toe. Endpoints must hold for all modes: full opaque
    /// black over white is 0, zero coverage keeps bg.
    #[test]
    fn all_gamma_modes_build_and_blend() {
        for &mode in &[-1i32, 0, 1, 2, 5] {
            let t = Tables::build(1.20, 1.0, 1.0, mode);
            assert_eq!(t.blend(255, 0, 255), 0, "mode={mode} full coverage");
            assert_eq!(t.blend(255, 0, 0), 255, "mode={mode} zero coverage keeps bg");
            // monotone non-increasing across coverage for black-on-white
            let mut prev = 256i32;
            for c in 0..=255u8 {
                let v = i32::from(t.blend(255, 0, c));
                assert!(v <= prev, "mode={mode} non-monotone at cov={c}");
                prev = v;
            }
        }
    }

    /// Non-identity weight and contrast drive the `curve` S-curve (both
    /// `t < 0.5` and the upper half), covering `build`'s coverage branch and
    /// the `blend` early-out at zero coverage.
    #[test]
    fn weight_and_contrast_curve() {
        let t = Tables::build(1.25, 1.6, 0.7, 0);
        assert_eq!(t.blend(200, 10, 0), 200, "zero coverage returns bg unchanged");
        assert_eq!(t.blend(255, 0, 255), 0);
        // mid coverage lands strictly between the endpoints
        let mid = t.blend(255, 0, 128);
        assert!(mid > 0, "mid={mid}");
        assert!(mid < 255, "mid={mid}");
    }
}
