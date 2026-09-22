//! render-core: an experimental, offline Rust port of the MacType glyph
//! *rendering* core (upstream MacType ft.cpp, commit 05052e8) — the gamma/contrast/LCD tuning and
//! linear-space blend that sit on top of FreeType.
//!
//! Scope: it turns "a character + a profile" into pixels the way MacType does.
//! The LUT + blend math is the formula in docs/SPEC.md 2.3, computed in f32
//! instead of upstream's fixed-point integers (agreeing within one 8-bit
//! level, this side more accurate). It does **not** hook or inject
//! anything — the system-wide part of
//! MacType (GDI/DirectWrite interception, DLL injection) is out of scope here.
//!
//! FreeType itself is reused unchanged (the fork's `freetype64.lib`), so glyph
//! rasterisation is identical, not reimplemented.

pub mod config;
pub mod filter;
pub mod ft;
pub mod render;

pub use config::{Aa, Profile};
pub use filter::Tables;
pub use ft::Ft;
pub use render::{Canvas, Ink};

/// Build the blend tables for a profile.
pub fn tables_for(p: &Profile) -> Tables {
    Tables::build(p.gamma, p.weight, p.contrast, p.gamma_mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tables_for_each_preset_blends_endpoints() {
        for p in [Profile::clean_greyscale(), Profile::clean_sharp(), Profile::accurate(),
                  Profile::clean_dark_greyscale(), Profile::clean_sharp_dark()] {
            let t = tables_for(&p);
            assert_eq!(t.blend(255, 0, 255), 0);
            assert_eq!(t.blend(255, 0, 0), 255);
        }
    }
}
