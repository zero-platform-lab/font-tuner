//! The state a draw needs, and how a detour gets back to the real function.

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::DirectWrite::IDWriteFontFace;

use render_core::{Ft, Profile, Tables};

/// Everything a draw needs, behind one lock.
///
/// FreeType, the blend tables, the profile and the "last font" cache used to be
/// four separate `static mut`s that every call site promised to touch only while
/// holding RENDER_LOCK. Nothing enforced that promise, and a reload swapping
/// tables and profile separately could be observed half-applied. Keeping them in
/// one struct behind one mutex makes the promise unbypassable: there is no way to
/// reach the face or the profile without the guard.
pub(crate) struct RenderState {
    pub(crate) ft: Ft,
    pub(crate) tables: Tables,
    pub(crate) profile: Profile,
    /// DirectWrite faces recently drawn, and the key each is open under in
    /// `ft` (`fonts::reface`). Least recently used first.
    pub(crate) dw_faces: Vec<DwFace>,
}

/// A DirectWrite face and its key in `Ft`. The clone keeps the object alive,
/// so its address cannot be recycled for another font while it is listed.
pub(crate) struct DwFace {
    pub(crate) face: IDWriteFontFace,
    pub(crate) key: u64,
    /// Keyed by the object rather than a file path (a font supplied from
    /// memory): closed in `Ft` when it leaves the list.
    pub(crate) memory: bool,
}

// SAFETY: `Ft` owns raw FreeType handles, which are not thread-safe on their
// own. The only access is through RENDER's mutex, so at most one thread ever
// touches the library or the face at a time — the condition FreeType requires.
unsafe impl Send for RenderState {}

/// `None` until `on_attach` initialises FreeType; a draw that arrives first
/// simply falls back to the OS rasteriser.
pub(crate) static RENDER: Mutex<Option<RenderState>> = Mutex::new(None);

/// A detour's trampoline back to the real function.
///
/// Every `ORIG_*` is published before the corresponding patch goes live, so a
/// detour that is running always finds it. If it somehow did not, the process
/// has already jumped into our code with no way back, and this aborts rather
/// than calling a null pointer.
#[inline]
pub(crate) fn orig<F: Copy>(cell: &OnceLock<F>) -> F {
    *cell.get().expect("detour ran before its trampoline was published")
}

/// Log the first substituted draw once, so the log shows the pipeline ran
/// without a line per glyph run. Shared by the GDI and DirectWrite paths.
pub(crate) static CAPTURED: AtomicBool = AtomicBool::new(false);

/// `f32::round() as i32`, the one float→int cast this crate makes: pixel
/// positions and em sizes from DirectWrite, clamped to a sane range first.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn round_i32(v: f32) -> i32 {
    v.round().clamp(-1.0e6, 1.0e6) as i32
}
