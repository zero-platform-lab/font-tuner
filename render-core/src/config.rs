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
        Some(p)
    }
}
