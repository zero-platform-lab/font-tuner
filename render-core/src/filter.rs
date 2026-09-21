//! Gamma / contrast / coverage lookup tables and the linear-space alpha blend.
//!
//! Direct port of MacType's `CAlphaBlend::init` and `CAlphaBlendColorOne::doAB`
//! (vendor/mactype/ft.cpp). Verified bit-for-bit against the C++ original — see
//! `verify/` — across all 256 coverage values and several profiles.

/// Fixed-point scale (`ft.cpp` `CAlphaBlend::BASE = 0x4000`).
pub const BASE: i32 = 0x4000;

/// Gamma-encode LUT, its inverse, and the coverage (contrast/weight) LUT.
pub struct Tables {
    tbl1: [i32; 257], // byte -> linear light * BASE   (gamma encode)
    tbl2: Vec<i32>,   // (linear*BASE^2 >> 16) -> byte  (gamma decode)
    tunetbl: [i32; 256], // coverage -> alpha in [0, BASE] (contrast/weight curve)
}

/// Exact port of `CAlphaBlend::rconv1`: inverse of `tbl1` by binary probe.
fn rconv1(tbl1: &[i32; 257], n: i32) -> u8 {
    let mut pos: i32 = 0x80;
    let mut i: i32 = pos >> 1;
    while i > 0 {
        if n >= tbl1[pos as usize] { pos += i } else { pos -= i }
        i >>= 1;
    }
    if n >= tbl1[pos as usize] { pos += 1 }
    (pos - 1) as u8
}

impl Tables {
    /// Build the tables for a profile.
    ///
    /// * `gamma`    – `GammaValue` (used when `mode` selects plain-power gamma)
    /// * `weight`   – `RenderWeight` (coverage S-curve)
    /// * `contrast` – `Contrast`
    /// * `mode`     – `GammaMode`: `<0` linear, `1` sRGB, `2` sRGB/linear avg,
    ///                else plain-power `gamma`.
    pub fn build(gamma: f32, weight: f32, contrast: f32, mode: i32) -> Tables {
        let mut alphatbl = [0i32; 256];
        for i in 0..256 {
            let temp = ((1.0f32 / 255.0) * i as f32).powf(1.0 / weight);
            let a = if temp < 0.5 {
                (temp * 2.0).powf(contrast) / 2.0
            } else {
                1.0 - ((1.0 - temp) * 2.0).powf(contrast) / 2.0
            };
            alphatbl[i] = (a * BASE as f32) as i32;
        }

        let mut tbl1 = [0i32; 257];
        for i in 0..256 {
            let x = i as f32 / 255.0;
            let t = if mode < 0 {
                x
            } else if mode == 1 {
                if i <= 10 { i as f32 / (12.92 * 255.0) } else { ((x + 0.055) / 1.055).powf(2.4) }
            } else if mode == 2 {
                let s = if i <= 10 { i as f32 / (12.92 * 255.0) } else { ((x + 0.055) / 1.055).powf(2.4) };
                (s + x) / 2.0
            } else {
                x.powf(gamma)
            };
            tbl1[i] = (t * BASE as f32) as i32;
        }
        tbl1[256] = BASE;

        let size = 256 * 16 + 1;
        let step = BASE / (size as i32 - 1); // = 4
        let tbl2: Vec<i32> = (0..size).map(|i| rconv1(&tbl1, i as i32 * step) as i32).collect();

        let mut tunetbl = [0i32; 256];
        for i in 0..256 {
            // identity tune curve (default TextTuning) => tunetbl == clamp(alphatbl)
            tunetbl[i] = alphatbl[i].clamp(0, BASE);
        }

        Tables { tbl1, tbl2, tunetbl }
    }

    #[inline]
    fn conv1(&self, b: u8) -> i32 { self.tbl1[b as usize] }

    #[inline]
    fn conv2(&self, n: i32) -> i32 { self.tbl2[(n >> 16) as usize] }

    /// One-channel blend of foreground `fg` over background `bg` at coverage
    /// `cov` (0..=255), done in gamma-linear space. Port of
    /// `CAlphaBlendColorOne::doAB`.
    #[inline]
    pub fn blend(&self, bg: u8, fg: u8, cov: u8) -> u8 {
        let a = self.tunetbl[cov as usize];
        if a == 0 { return bg; }
        self.conv2(self.conv1(bg) * (BASE - a) + self.conv1(fg) * a) as u8
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
            let v = t.blend(255, 0, c) as i32;
            assert!(v <= prev);
            prev = v;
        }
    }

    /// Black-on-white greyscale blend at gamma 1.25 — values captured from the
    /// C++ oracle (verify/), so this guards the LUT + blend against drift
    /// without needing the C++ harness.
    #[test]
    fn greyscale_regression_g125() {
        let t = Tables::build(1.25, 1.0, 1.0, 0);
        for &(cov, expect) in &[(0u8, 255u8), (32, 229), (64, 202), (128, 146), (192, 83), (255, 0)] {
            assert_eq!(t.blend(255, 0, cov), expect, "cov={cov}");
        }
    }

    /// Exercise every GammaMode branch of `build`'s tbl1 construction
    /// (mode<0 linear, mode==1 sRGB, mode==2 sRGB/linear avg, else plain gamma),
    /// including both sides of the sRGB `i <= 10` toe. Endpoints must hold for
    /// all modes: full opaque black over white is 0, zero coverage keeps bg.
    #[test]
    fn all_gamma_modes_build_and_blend() {
        for &mode in &[-1i32, 0, 1, 2, 5] {
            let t = Tables::build(1.20, 1.0, 1.0, mode);
            assert_eq!(t.blend(255, 0, 255), 0, "mode={mode} full coverage");
            assert_eq!(t.blend(255, 0, 0), 255, "mode={mode} zero coverage keeps bg");
            // monotone non-increasing across coverage for black-on-white
            let mut prev = 256i32;
            for c in 0..=255u8 {
                let v = t.blend(255, 0, c) as i32;
                assert!(v <= prev, "mode={mode} non-monotone at cov={c}");
                prev = v;
            }
        }
    }

    /// Non-identity weight and contrast drive the `alphatbl` S-curve
    /// (both `temp < 0.5` and the upper half), covering `build`'s coverage
    /// branch and the `blend` `a == 0` early-out at zero coverage.
    #[test]
    fn weight_and_contrast_curve() {
        let t = Tables::build(1.25, 1.6, 0.7, 0);
        assert_eq!(t.blend(200, 10, 0), 200, "a==0 returns bg unchanged");
        assert_eq!(t.blend(255, 0, 255), 0);
        // mid coverage lands strictly between the endpoints
        let mid = t.blend(255, 0, 128);
        assert!(mid > 0, "mid={mid}");
        assert!(mid < 255, "mid={mid}");
    }
}
