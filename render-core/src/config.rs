//! Rendering profile: the subset of MacType.ini settings this offline core
//! reproduces.

/// Anti-alias mode (`AntiAliasMode`): greyscale or LCD subpixel with an order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Aa {
    /// Greyscale (AntiAliasMode 0).
    Grey,
    /// LCD subpixel, RGB order (AntiAliasMode 2).
    LcdRgb,
    /// LCD subpixel, BGR order (AntiAliasMode 3).
    LcdBgr,
    /// LightLCD (light-hinted, LCD rendered), RGB order (AntiAliasMode 4).
    LightLcdRgb,
    /// LightLCD, BGR order (AntiAliasMode 5).
    LightLcdBgr,
}

impl Aa {
    /// The MacType `AntiAliasMode` integer.
    pub fn mode(self) -> i32 {
        match self {
            Aa::Grey => 0,
            Aa::LcdRgb => 2,
            Aa::LcdBgr => 3,
            Aa::LightLcdRgb => 4,
            Aa::LightLcdBgr => 5,
        }
    }
    pub fn is_lcd(self) -> bool { !matches!(self, Aa::Grey) }
}

/// The `[DirectWrite]` section: what the injected core hands to DirectWrite /
/// Direct2D as `IDWriteRenderingParams` for text it cannot rasterise itself
/// (Direct2D device contexts drawing to DXGI surfaces, where no GDI DC can be
/// borrowed). Same keys and defaults as upstream `settings.cpp`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DwParams {
    /// `GammaValue`; when absent or 0 (the greyscale profiles ship 0), derived
    /// from the general gamma: `g*g > 1.3 ? g*g/2 : 0.7`.
    pub gamma: f32,
    /// `Contrast` (enhanced contrast, also used as the greyscale contrast).
    pub contrast: f32,
    /// `ClearTypeLevel` 0..1.
    pub cleartype_level: f32,
    /// `RenderingMode` 0..6, passed to `DWRITE_RENDERING_MODE` as is (as
    /// upstream's Direct2D params do; 6 = OUTLINE there).
    pub rendering_mode: i32,
}

impl DwParams {
    /// Upstream's defaults for a profile with no `[DirectWrite]` section.
    pub fn derived_from(general_gamma: f32) -> DwParams {
        let g2 = general_gamma * general_gamma;
        DwParams { gamma: if g2 > 1.3 { g2 / 2.0 } else { 0.7 }, contrast: 1.0, cleartype_level: 1.0, rendering_mode: 5 }
    }
}

/// Which ini section a line belongs to while parsing.
#[derive(PartialEq)]
enum Section {
    General,
    DirectWrite,
    Experimental,
    Other,
}

/// A rendering profile.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub gamma: f32,
    pub weight: f32,     // RenderWeight (coverage curve)
    pub contrast: f32,
    pub gamma_mode: i32, // GammaMode: <0 linear, 1 sRGB, 2 avg, else plain gamma
    pub aa: Aa,
    /// HintingMode: 0 native/bytecode, 1 none, 2 autohint.
    pub hinting: i32,
    /// FreeType LCD filter: 0 none, 1 default, 2 light.
    pub lcd_filter: i32,
    /// Outline embolden strength in 26.6 units (NormalWeight; 0 = none).
    pub embolden: i32,
    /// `[DirectWrite]` overrides for the paths DirectWrite/Direct2D render.
    pub dw: DwParams,
    /// `[Experimental] ClipBoxFix` (default on, as upstream): pad the glyph
    /// metrics `GetGlyphOutline` reports so apps that clip to them (Java2D)
    /// do not cut off the heavier rendered glyphs.
    pub clipbox_fix: bool,
}

impl Profile {
    /// Clean Greyscale (shipped default): greyscale, no hinting bias, gamma 1.25.
    pub fn clean_greyscale() -> Profile {
        Profile { gamma: 1.25, weight: 1.0, contrast: 1.0, gamma_mode: 0,
                  aa: Aa::Grey, hinting: 0, lcd_filter: 0, embolden: 0, dw: DwParams::derived_from(1.25), clipbox_fix: true }
    }
    /// Clean Sharp: LCD subpixel, no hinting, gamma 1.2, no LCD filter.
    pub fn clean_sharp() -> Profile {
        Profile { gamma: 1.20, weight: 1.0, contrast: 1.0, gamma_mode: 0,
                  aa: Aa::LcdRgb, hinting: 1, lcd_filter: 0, embolden: 0, dw: DwParams::derived_from(1.20), clipbox_fix: true }
    }
    /// Accurate: LightLCD, autohint, gamma 1.3, LIGHT filter.
    pub fn accurate() -> Profile {
        Profile { gamma: 1.30, weight: 1.0, contrast: 1.0, gamma_mode: 0,
                  aa: Aa::LightLcdRgb, hinting: 2, lcd_filter: 2, embolden: 0, dw: DwParams::derived_from(1.30), clipbox_fix: true }
    }
    /// Clean Dark Greyscale: greyscale tuned for dark backgrounds
    /// (gamma 1.1, contrast 0.9, slightly heavier weight).
    pub fn clean_dark_greyscale() -> Profile {
        Profile { gamma: 1.10, weight: 1.05, contrast: 0.9, gamma_mode: 0,
                  aa: Aa::Grey, hinting: 0, lcd_filter: 0, embolden: 0, dw: DwParams::derived_from(1.10), clipbox_fix: true }
    }
    /// Clean Sharp Dark: LCD subpixel tuned for dark backgrounds.
    pub fn clean_sharp_dark() -> Profile {
        Profile { gamma: 1.10, weight: 1.05, contrast: 0.9, gamma_mode: 0,
                  aa: Aa::LcdRgb, hinting: 0, lcd_filter: 0, embolden: 0, dw: DwParams::derived_from(1.10), clipbox_fix: true }
    }

    /// Parse a MacType profile `.ini` into a `Profile`. Keys outside
    /// `[DirectWrite]` are read first-occurrence-wins (the `[General]` /
    /// `[FreeType]` section); `[DirectWrite]` fills `dw`. Unknown/missing keys
    /// keep sensible defaults.
    pub fn from_ini(path: &str) -> Option<Profile> {
        let text = std::fs::read_to_string(path).ok()?;
        Some(Profile::from_ini_str(&text))
    }

    /// Parse profile settings from the text of a MacType `.ini`.
    #[allow(clippy::too_many_lines)]
    pub fn from_ini_str(text: &str) -> Profile {
        let mut p = Profile::clean_greyscale();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut section = Section::General;
        // Explicit [DirectWrite] values; the rest is derived once the general
        // gamma is known (upstream reads the section after [General]).
        // Explicit [DirectWrite] gamma, contrast, cleartype level.
        let mut dw: [Option<f32>; 3] = [None; 3];
        let mut dw_mode: Option<i32> = None;
        let mut clipbox_fix: Option<bool> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                let name = line.trim_end_matches(']').trim_start_matches('[').trim();
                // Per-process sections (`[Experimental@idea64.exe]`) are not
                // applied: the core has no per-exe settings yet.
                section = if name.eq_ignore_ascii_case("DirectWrite") { Section::DirectWrite }
                    else if name.eq_ignore_ascii_case("Experimental") { Section::Experimental }
                    else if name.eq_ignore_ascii_case("General") || name.eq_ignore_ascii_case("FreeType") { Section::General }
                    else { Section::Other };
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match section {
                Section::DirectWrite => {
                    if k.eq_ignore_ascii_case("RenderingMode") {
                        dw_mode = dw_mode.or_else(|| v.parse().ok());
                    } else if let Some(i) = ["GammaValue", "Contrast", "ClearTypeLevel"].iter().position(|n| k == *n) {
                        if dw[i].is_none() { dw[i] = v.parse().ok(); }
                    }
                    continue;
                }
                Section::Experimental => {
                    // Profiles spell it ClipBoxFix / Clipboxfix / clipboxfix.
                    if k.eq_ignore_ascii_case("ClipBoxFix") && clipbox_fix.is_none() {
                        clipbox_fix = v.parse::<i32>().ok().map(|n| n != 0);
                    }
                    continue;
                }
                Section::Other => continue,
                Section::General => {}
            }
            if !seen.insert(k) { continue; } // first occurrence wins
            match k {
                "HintingMode" => if let Ok(n) = v.parse() { p.hinting = n; },
                "AntiAliasMode" => if let Ok(n) = v.parse::<i32>() {
                    p.aa = match n { 0 => Aa::Grey, 2 => Aa::LcdRgb, 3 => Aa::LcdBgr,
                                     4 => Aa::LightLcdRgb, 5 => Aa::LightLcdBgr, _ => p.aa };
                },
                "GammaValue" => if let Ok(f) = v.parse() { p.gamma = f; },
                "GammaMode" => if let Ok(n) = v.parse() { p.gamma_mode = n; },
                "Contrast" => if let Ok(f) = v.parse() { p.contrast = f; },
                "RenderWeight" => if let Ok(f) = v.parse() { p.weight = f; },
                "LcdFilter" => if let Ok(n) = v.parse() { p.lcd_filter = n; },
                "NormalWeight" => if let Ok(n) = v.parse() { p.embolden = n; },
                _ => {}
            }
        }
        let d = DwParams::derived_from(p.gamma);
        p.dw = DwParams {
            // A `[DirectWrite] GammaValue` of 0 (as the greyscale profiles ship)
            // means "don't override" — DirectWrite needs gamma > 0 — so fall
            // back to the derived gamma, not a literal 0 that fails
            // CreateCustomRenderingParams.
            gamma: dw[0].filter(|&g| g > 0.0).unwrap_or(d.gamma).clamp(0.0625, 20.0),
            contrast: dw[1].unwrap_or(d.contrast).clamp(0.0625, 10.0),
            cleartype_level: dw[2].unwrap_or(d.cleartype_level).clamp(0.0, 1.0),
            rendering_mode: dw_mode.unwrap_or(d.rendering_mode).clamp(0, 6),
        };
        p.clipbox_fix = clipbox_fix.unwrap_or(true);
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_ini_parses_accurate() {
        let ini = "\
; Accurate
[General]
HintingMode=2
AntiAliasMode=4
GammaValue=1.3
Contrast=1.0
RenderWeight=1.0
LcdFilter=2
[DirectWrite]
GammaValue=1.4
Contrast=0.0
";
        let p = Profile::from_ini_str(ini);
        assert_eq!(p.hinting, 2);
        assert_eq!(p.aa, Aa::LightLcdRgb);
        assert_eq!(p.lcd_filter, 2);
        assert!((p.gamma - 1.3).abs() < 1e-6, "first (General) GammaValue wins over DirectWrite");
        assert!((p.contrast - 1.0).abs() < 1e-6);
        assert!((p.dw.gamma - 1.4).abs() < 1e-6, "[DirectWrite] GammaValue lands in dw");
        // Contrast=0.0 is clamped to upstream's CONTRAST_MIN (settings.cpp).
        assert!((p.dw.contrast - 0.0625).abs() < 1e-6);
        assert!((p.dw.cleartype_level - 1.0).abs() < 1e-6, "absent key keeps the upstream default");
        assert_eq!(p.dw.rendering_mode, 5);
    }

    #[test]
    fn from_ini_parses_gamma_mode() {
        let p = Profile::from_ini_str("[General]
GammaMode=1
NormalWeight=8
");
        assert_eq!(p.gamma_mode, 1);
        assert_eq!(p.embolden, 8);
        // Shipped profiles say GammaMode=0 (plain power gamma), the default.
        assert_eq!(Profile::from_ini_str("[General]
GammaMode=0
").gamma_mode, 0);
        assert_eq!(Profile::from_ini_str("[General]
").gamma_mode, 0);
    }

    #[test]
    fn clipbox_fix_from_experimental_section_any_case() {
        assert!(Profile::from_ini_str("").clipbox_fix, "upstream default is on");
        assert!(!Profile::from_ini_str("[Experimental]
clipboxfix=0
").clipbox_fix);
        assert!(Profile::from_ini_str("[Experimental]
ClipBoxFix=1
").clipbox_fix);
        // a per-process section must not override the plain one
        assert!(!Profile::from_ini_str("[Experimental]
Clipboxfix=0
[Experimental@idea64.exe]
clipboxfix=1
").clipbox_fix);
        // and a key outside [Experimental] is ignored
        assert!(Profile::from_ini_str("ClipBoxFix=0
").clipbox_fix);
    }

    #[test]
    fn dw_params_derive_from_general_gamma_when_section_absent() {
        // 1.25^2 = 1.5625 > 1.3 -> /2
        let p = Profile::from_ini_str("GammaValue=1.25
");
        assert!((p.dw.gamma - 1.5625 / 2.0).abs() < 1e-6);
        // 1.1^2 = 1.21 <= 1.3 -> 0.7
        let p = Profile::from_ini_str("GammaValue=1.1
");
        assert!((p.dw.gamma - 0.7).abs() < 1e-6);
        // a [DirectWrite] GammaValue must not leak into the general gamma
        let p = Profile::from_ini_str("[DirectWrite]
GammaValue=0.9
RenderingMode=2
[General]
GammaValue=1.3
");
        assert!((p.gamma - 1.3).abs() < 1e-6);
        assert!((p.dw.gamma - 0.9).abs() < 1e-6);
        assert_eq!(p.dw.rendering_mode, 2);

        // [DirectWrite] GammaValue=0 (as the greyscale profiles ship) means
        // "don't override": derive from the general gamma, never a literal 0
        // (DirectWrite needs gamma > 0).
        let p = Profile::from_ini_str("[General]
GammaValue=1.25
[DirectWrite]
GammaValue=0.0
");
        assert!((p.dw.gamma - 1.5625 / 2.0).abs() < 1e-6, "gamma 0 -> derived, got {}", p.dw.gamma);
    }

    #[test]
    fn from_ini_greyscale_defaults() {
        let p = Profile::from_ini_str("AntiAliasMode=0\nHintingMode=0\nGammaValue=1.25\n");
        assert_eq!(p.aa, Aa::Grey);
        assert_eq!(p.hinting, 0);
        assert!((p.gamma - 1.25).abs() < 1e-6);
    }

    #[test]
    fn aa_mode_and_is_lcd_all_variants() {
        assert_eq!(Aa::Grey.mode(), 0);
        assert_eq!(Aa::LcdRgb.mode(), 2);
        assert_eq!(Aa::LcdBgr.mode(), 3);
        assert_eq!(Aa::LightLcdRgb.mode(), 4);
        assert_eq!(Aa::LightLcdBgr.mode(), 5);
        assert!(!Aa::Grey.is_lcd());
        for aa in [Aa::LcdRgb, Aa::LcdBgr, Aa::LightLcdRgb, Aa::LightLcdBgr] {
            assert!(aa.is_lcd());
        }
    }

    #[test]
    fn all_presets_construct() {
        assert_eq!(Profile::clean_greyscale().aa, Aa::Grey);
        assert_eq!(Profile::clean_sharp().aa, Aa::LcdRgb);
        assert_eq!(Profile::accurate().aa, Aa::LightLcdRgb);
        assert_eq!(Profile::clean_dark_greyscale().aa, Aa::Grey);
        assert_eq!(Profile::clean_sharp_dark().aa, Aa::LcdRgb);
        assert_eq!(Profile::accurate().lcd_filter, 2);
        assert!((Profile::clean_dark_greyscale().contrast - 0.9).abs() < 1e-6);
    }

    #[test]
    fn antialias_mode_bgr_and_light_bgr_and_invalid() {
        assert_eq!(Profile::from_ini_str("AntiAliasMode=3\n").aa, Aa::LcdBgr);
        assert_eq!(Profile::from_ini_str("AntiAliasMode=5\n").aa, Aa::LightLcdBgr);
        // unknown value keeps the default (Grey) via the `_ => p.aa` arm
        assert_eq!(Profile::from_ini_str("AntiAliasMode=9\n").aa, Aa::Grey);
    }

    #[test]
    fn contrast_weight_normalweight_and_comments() {
        let p = Profile::from_ini_str(
            "; a comment line\nContrast=0.8\nRenderWeight=1.4\nNormalWeight=32\n",
        );
        assert!((p.contrast - 0.8).abs() < 1e-6);
        assert!((p.weight - 1.4).abs() < 1e-6);
        assert_eq!(p.embolden, 32);
    }

    #[test]
    fn malformed_values_keep_defaults() {
        // parse failures hit the `if let Ok(..)` else path for every key
        let base = Profile::clean_greyscale();
        let p = Profile::from_ini_str(
            "HintingMode=x\nAntiAliasMode=y\nGammaValue=z\nContrast=q\n\
             RenderWeight=w\nLcdFilter=n\nNormalWeight=m\nNoEquectionHere\nUnknownKey=1\n",
        );
        assert_eq!(p.hinting, base.hinting);
        assert_eq!(p.aa, base.aa);
        assert!((p.gamma - base.gamma).abs() < 1e-6);
        assert!((p.contrast - base.contrast).abs() < 1e-6);
        assert!((p.weight - base.weight).abs() < 1e-6);
        assert_eq!(p.lcd_filter, base.lcd_filter);
        assert_eq!(p.embolden, base.embolden);
    }

    #[test]
    fn from_ini_reads_file_and_missing_path() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ft-profile-test-{}.ini", std::process::id()));
        std::fs::write(&path, "AntiAliasMode=2\nHintingMode=1\nGammaValue=1.2\n").unwrap();
        let p = Profile::from_ini(path.to_str().unwrap()).expect("file parses");
        assert_eq!(p.aa, Aa::LcdRgb);
        assert_eq!(p.hinting, 1);
        std::fs::remove_file(&path).ok();
        // missing file -> None (the `?` on read_to_string)
        assert!(Profile::from_ini(path.to_str().unwrap()).is_none());
    }
}
