#![allow(non_snake_case)]
//! RenderBootstrap64.dll: the file the core loads into every child
//! process it spawns (expfunc.cpp, GdippInjectDLL). The real one is closed
//! source; all we need it to do is pull RenderCore64.dll from the same
//! folder, whose DllMain then hooks the process on its own.
//!
//! Scope: this reaches ordinary child processes (e.g. Chrome's
//! `crashpad-handler` / utility processes). It does NOT reach the sandboxed
//! renderer and GPU processes of Chrome/Edge — those are created with
//! MITIGATION_FORCE_MS_SIGNED_BINS, so the loader refuses this unsigned DLL
//! outright. Reaching them would require a Microsoft-signed binary and is out
//! of scope here.

use std::ffi::c_void;
use windows::Win32::Foundation::{CloseHandle, HMODULE};
use windows::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameW, LoadLibraryW,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::System::Threading::{CreateThread, THREAD_CREATION_FLAGS};
use windows::core::PCWSTR;

const CORE: &str = "RenderCore64.dll";

#[unsafe(no_mangle)]
extern "system" fn DllMain(hinst: HMODULE, reason: u32, _reserved: *const c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // Load the core from a worker thread, not inline: LoadLibrary here
        // would run under the loader lock, and the core's own DllMain
        // installs hooks and may load further DLLs. Nesting that under the
        // lock can deadlock the child we are meant to instrument. A thread
        // created in DllMain does not start until the loader lock is
        // released, so the core loads safely afterwards.
        // SAFETY: `hinst` is our own module handle, passed through as the
        // thread parameter; nothing here loads a library under the loader lock.
        unsafe {
            let _ = DisableThreadLibraryCalls(hinst);
            if let Ok(h) = CreateThread(None, 0, Some(load_thread), Some(hinst.0.cast_const()), THREAD_CREATION_FLAGS(0), None) {
                let _ = CloseHandle(h);
            }
        }
    }
    // Never fail the load: a missing core must not take the child down.
    1
}

/// Runs after DllMain returns (loader lock released).
///
/// Must stay panic-free: the crate builds with `panic = "abort"`, so a panic
/// here would abort — and thus kill — the host child process, breaking the
/// "never take the child down" invariant. Keep every call non-panicking.
unsafe extern "system" fn load_thread(param: *mut c_void) -> u32 {
    // `param` is the HMODULE DllMain passed to CreateThread.
    load_core(HMODULE(param));
    0
}

fn load_core(hinst: HMODULE) {
    // Grow the buffer until the full module path fits, so an install path
    // longer than MAX_PATH still loads the core instead of silently no-op'ing.
    let mut buf = vec![0u16; 260];
    let n = loop {
        // SAFETY: `hinst` is our module handle and `buf` outlives the call.
        let n = unsafe { GetModuleFileNameW(Some(hinst), &mut buf) } as usize;
        if n == 0 {
            return;
        }
        if n < buf.len() {
            break n;
        }
        // n == buf.len(): path was truncated. Grow, up to the Windows maximum.
        if buf.len() >= 0x8000 {
            return;
        }
        buf.resize(buf.len() * 2, 0);
    };
    // Replace our own file name with the core's, same directory.
    let Some(slash) = buf[..n].iter().rposition(|&c| c == u16::from(b'\\')) else {
        return;
    };
    let mut path: Vec<u16> = buf[..=slash].to_vec();
    path.extend(CORE.encode_utf16());
    path.push(0);
    // SAFETY: `path` is NUL-terminated and outlives the call; this runs
    // after DllMain returned, so not under the loader lock.
    let _ = unsafe { LoadLibraryW(PCWSTR(path.as_ptr())) };
}
