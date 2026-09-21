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
}

impl Profile {
    /// Clean Greyscale (shipped default): greyscale, no hinting bias, gamma 1.25.
    pub fn clean_greyscale() -> Profile {
        Profile { gamma: 1.25, weight: 1.0, contrast: 1.0, gamma_mode: 0,
                  aa: Aa::Grey, hinting: 0, lcd_filter: 0, embolden: 0 }
    }
    /// Clean Sharp: LCD subpixel, no hinting, gamma 1.2, no LCD filter.
    pub fn clean_sharp() -> Profile {
        Profile { gamma: 1.20, weight: 1.0, contrast: 1.0, gamma_mode: 0,
                  aa: Aa::LcdRgb, hinting: 1, lcd_filter: 0, embolden: 0 }
    }
    /// Accurate: LightLCD, autohint, gamma 1.3, LIGHT filter.
    pub fn accurate() -> Profile {
        Profile { gamma: 1.30, weight: 1.0, contrast: 1.0, gamma_mode: 0,
                  aa: Aa::LightLcdRgb, hinting: 2, lcd_filter: 2, embolden: 0 }
    }
    /// Clean Dark Greyscale: greyscale tuned for dark backgrounds
    /// (gamma 1.1, contrast 0.9, slightly heavier weight).
    pub fn clean_dark_greyscale() -> Profile {
        Profile { gamma: 1.10, weight: 1.05, contrast: 0.9, gamma_mode: 0,
                  aa: Aa::Grey, hinting: 0, lcd_filter: 0, embolden: 0 }
    }
    /// Clean Sharp Dark: LCD subpixel tuned for dark backgrounds.
    pub fn clean_sharp_dark() -> Profile {
        Profile { gamma: 1.10, weight: 1.05, contrast: 0.9, gamma_mode: 0,
                  aa: Aa::LcdRgb, hinting: 0, lcd_filter: 0, embolden: 0 }
    }

    /// Parse a MacType profile `.ini` into a `Profile`. Reads the first value of
    /// each key (the `[General]`/`[FreeType]` section, not the `[DirectWrite]`
    /// overrides). Unknown/missing keys keep sensible defaults.
    pub fn from_ini(path: &str) -> Option<Profile> {
        let text = std::fs::read_to_string(path).ok()?;
        Some(Profile::from_ini_str(&text))
    }

    /// Parse profile settings from the text of a MacType `.ini`.
    pub fn from_ini_str(text: &str) -> Profile {
        let mut p = Profile::clean_greyscale();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with(';') || line.starts_with('[') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            if !seen.insert(k) { continue; } // first occurrence wins
            match k {
                "HintingMode" => if let Ok(n) = v.parse() { p.hinting = n; },
                "AntiAliasMode" => if let Ok(n) = v.parse::<i32>() {
                    p.aa = match n { 0 => Aa::Grey, 2 => Aa::LcdRgb, 3 => Aa::LcdBgr,
                                     4 => Aa::LightLcdRgb, 5 => Aa::LightLcdBgr, _ => p.aa };
                },
                "GammaValue" => if let Ok(f) = v.parse() { p.gamma = f; },
                "Contrast" => if let Ok(f) = v.parse() { p.contrast = f; },
                "RenderWeight" => if let Ok(f) = v.parse() { p.weight = f; },
                "LcdFilter" => if let Ok(n) = v.parse() { p.lcd_filter = n; },
                "NormalWeight" => if let Ok(n) = v.parse() { p.embolden = n; },
                _ => {}
            }
        }
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
