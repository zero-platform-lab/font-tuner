#![allow(non_snake_case)] // crate/DLL name RenderCore64 mirrors MacType64.Core
//! render-inject — the DLL that (eventually) hooks text drawing in each process
//! it is loaded into and renders with `render-core`. See DESIGN.md.
//!
//! Stage 4a: on load it spawns a thread that records which process it landed in
//! (a log line under %TEMP%\render-inject.log). This proves the injection path.
//! No hooking / writeback yet — that is the next stage.

use core::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;

use windows::core::BOOL;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Threading::{CreateThread, GetCurrentProcessId, THREAD_CREATION_FLAGS};

const DLL_PROCESS_ATTACH: u32 = 1;

/// Runs off the loader lock (spawned from DllMain), so file I/O is safe here.
unsafe extern "system" fn on_attach(_param: *mut c_void) -> u32 {
    let pid = GetCurrentProcessId();
    let mut buf = [0u16; 260];
    let n = GetModuleFileNameW(None, &mut buf);
    let exe = String::from_utf16_lossy(&buf[..n as usize]);
    if let Some(tmp) = std::env::var_os("TEMP") {
        let path = PathBuf::from(tmp).join("render-inject.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "loaded into pid={pid} exe={exe}");
        }
    }
    0
}

#[no_mangle]
pub extern "system" fn DllMain(_hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // Spawn a thread (runs after the loader lock is released) rather than
        // doing work directly in DllMain.
        unsafe {
            let _ = CreateThread(None, 0, Some(on_attach), None, THREAD_CREATION_FLAGS(0), None);
        }
    }
    BOOL(1) // TRUE
}

/// Exported self-check: proves `render-core` is linked and callable.
#[no_mangle]
pub extern "C" fn render_core_selfcheck() -> i32 {
    let t = render_core::Tables::build(1.25, 1.0, 1.0, 0);
    let ok = t.blend(255, 0, 0) == 255 && t.blend(255, 0, 255) == 0;
    ok as i32
}
