//! render-core: an experimental, offline Rust port of the MacType glyph
//! *rendering* core (vendor/mactype/ft.cpp) — the gamma/contrast/LCD tuning and
//! linear-space blend that sit on top of FreeType.
//!
//! Scope: it turns "a character + a profile" into pixels, exactly as MacType
//! would (the LUT + blend math is verified bit-for-bit against the C++ code; see
//! `verify/`). It does **not** hook or inject anything — the system-wide part of
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
