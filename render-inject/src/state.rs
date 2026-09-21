//! The state a draw needs, and how a detour gets back to the real function.

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

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
    /// Identity of the face currently loaded into `ft`, so a draw only
    /// re-extracts and re-faces when the font actually changes.
    pub(crate) font_key: Option<String>,
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
