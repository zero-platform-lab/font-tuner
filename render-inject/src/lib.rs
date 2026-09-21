#![allow(non_snake_case)] // crate/DLL name RenderCore64 mirrors MacType64.Core
//! render-inject — SKELETON ONLY. See DESIGN.md.
//!
//! This is the future replacement for MacType64.Core.dll: a DLL injected into
//! each process that will hook text drawing and render it via `render-core`.
//! Right now it installs no hooks and injects nothing — `DllMain` is a no-op.
//! Its only job today is to prove that `render-core` packages into a cdylib DLL
//! (the linking path: render-core rlib + FreeType fork lib + C shim).

use core::ffi::c_void;
use windows::core::BOOL;
use windows::Win32::Foundation::HINSTANCE;

/// No-op entry point. Real hook installation (off the loader lock, via a
/// spawned thread) belongs in a later stage — see DESIGN.md.
#[no_mangle]
pub extern "system" fn DllMain(_hinst: HINSTANCE, _reason: u32, _reserved: *mut c_void) -> BOOL {
    BOOL(1) // TRUE
}

/// Exported self-check: proves `render-core` is linked and callable from the
/// DLL. Returns 1 on success. (Used only by tests/tools, not by any hook.)
#[no_mangle]
pub extern "C" fn render_core_selfcheck() -> i32 {
    let t = render_core::Tables::build(1.25, 1.0, 1.0, 0);
    let ok = t.blend(255, 0, 0) == 255 && t.blend(255, 0, 255) == 0;
    ok as i32
}
