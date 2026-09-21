#![allow(non_snake_case)] // DLL name RenderCore64 / Win32 export names
//! render-inject — the DLL injected into each process. On load it hooks the
//! text-drawing entry points and renders with `render-core`, replacing Windows'
//! own text rendering inside whatever process this DLL was injected into:
//!   * GDI: gdi32!ExtTextOutW (`gdi.rs`) and the GetGlyphOutline metrics fix
//!     (`gdi_metrics.rs`).
//!   * DirectWrite: IDWriteBitmapRenderTarget::DrawGlyphRun and the
//!     CreateGlyphRunAnalysis → CreateAlphaTexture route (`dwrite.rs`).
//!   * Direct2D: every render target and device context (`d2d.rs`).
//!
//! This file holds what is shared: DllMain (pin + attach thread), the
//! attach-once logic, the WH_GETMESSAGE procedure and the log.

use core::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use render_core::{tables_for, Ft};
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HMODULE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
};
use windows::Win32::System::Threading::{CreateMutexW, CreateThread, GetCurrentProcessId, THREAD_CREATION_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, RegisterWindowMessageW, HC_ACTION, MSG, PM_REMOVE};

mod d2d;
mod dib;
mod dwrite;
mod gdi;
mod gdi_metrics;
mod hook;
mod profile;
mod state;

use profile::{load_profile, reload_profile, RELOAD_MSG, RELOAD_MSG_NAME, SELF_HINST};
use state::{RenderState, RENDER};

const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_PROCESS_DETACH: u32 = 0;

static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Append one line to `%TEMP%\render-inject.log`.
fn log(msg: &str) {
    if let Some(tmp) = std::env::var_os("TEMP") {
        let _g = LOG_LOCK.lock();
        let path = PathBuf::from(tmp).join("render-inject.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// Claim this process for the first DLL instance to attach. This image can be
/// mapped into one process more than once (Windows keys module identity by
/// the path it was loaded with, so the WH_GETMESSAGE map and another load can
/// become two instances with separate statics). A second attach would detour
/// ExtTextOutW over our own jump, and retour would build a trampoline from
/// that jump — corrupting the call chain and crashing the host. A named
/// kernel mutex is shared across instances, so the first attach owns it and
/// the rest bail. The handle is leaked on purpose: released only at process
/// exit, keeping the claim for the process's life.
fn claim_process(pid: u32) -> bool {
    let name: Vec<u16> = format!("Local\\FontTuner.Attached.{pid}\0").encode_utf16().collect();
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let h = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) };
    // SAFETY: GetLastError right after the call that set it.
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    match h {
        // Ours: the handle is deliberately never closed (see above).
        Ok(_) if !already => true,
        Ok(h) => {
            // SAFETY: closing the handle we just received.
            let _ = unsafe { CloseHandle(h) };
            false
        }
        Err(_) => false,
    }
}

/// Runs off the loader lock: init render-core and install the hooks.
unsafe extern "system" fn on_attach(_p: *mut c_void) -> u32 {
    // SAFETY: no arguments; process id of the calling process.
    let pid = unsafe { GetCurrentProcessId() };
    if !claim_process(pid) {
        return 0;
    }
    let mut buf = [0u16; 260];
    // SAFETY: `None` = this process's exe; `buf` outlives the call.
    let n = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    log(&format!("loaded into pid={pid} exe={}", String::from_utf16_lossy(&buf[..n])));

    let Ok(ft) = Ft::open(r"C:\Windows\Fonts\meiryo.ttc", 0) else {
        log("Ft::open failed");
        return 1;
    };
    // Use the active font-tuner profile if present, else the default.
    let (path, p) = load_profile();
    log(&format!("profile {}: {p:?}", path.as_deref().unwrap_or("(default)")));
    if let Ok(mut guard) = RENDER.lock() {
        *guard = Some(RenderState { ft, tables: tables_for(&p), profile: p, font_key: None, font_face: None });
    }
    dwrite::refresh_dw_rendering(&p);
    // SAFETY: a static NUL-terminated message name.
    RELOAD_MSG.store(unsafe { RegisterWindowMessageW(RELOAD_MSG_NAME) }, Ordering::Relaxed);

    gdi::setup_gdi_hook();
    gdi_metrics::setup_gdi_metrics_hooks();
    dwrite::setup_dwrite_hook();
    d2d::setup_d2d_hook();
    0
}

#[no_mangle]
pub extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        let _ = SELF_HINST.set(hinst.0.expose_provenance());
        // Pin ourselves for the life of the process. This DLL is mapped
        // into every GUI process by the tray's WH_GETMESSAGE hook; when
        // that hook goes away (tray off / exit / MSI upgrade / uninstall)
        // Windows FreeLibrary's us in every one of them at once. If any
        // code of ours can still run afterwards — the on_attach thread
        // still starting up, a thread inside a detour rendering glyphs —
        // that process executes unmapped memory and dies, and so does
        // every other process on the machine. Pinning makes FreeLibrary a
        // no-op, so the hooks simply stay installed until the process
        // exits. Contract: turning font-tuner off, switching profile and
        // upgrading all take effect for processes started afterwards;
        // running processes keep what they have.
        let mut me = HMODULE::default();
        // SAFETY: the address of a function in this very image identifies
        // it to GetModuleHandleEx; CreateThread only takes our fn pointer.
        // Nothing here loads a library under the loader lock.
        unsafe {
            let _ = GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                PCWSTR((DllMain as *const ()).cast::<u16>()),
                &raw mut me,
            );
            let _ = CreateThread(None, 0, Some(on_attach), None, THREAD_CREATION_FLAGS(0), None);
        }
    } else if reason == DLL_PROCESS_DETACH {
        // Pinned above, so this only ever runs at process termination. The
        // process is being torn down and will not draw again; touching
        // vtables or the detours (thread suspension) here is pointless and can
        // itself fault. Do nothing, as the C++ core does on termination.
    }
    BOOL(1)
}

/// WH_GETMESSAGE hook procedure. Its only purpose is to make Windows map this
/// DLL into every GUI process that pumps messages (which runs DllMain, which
/// installs our text hooks) — the same auto-injection mechanism the C++ core uses.
///
/// Its RVA is fixed at 0x1000, the first byte of `.text` (linker `/ORDER`, see
/// `build.rs`; the tray and `build-msi.ps1` both verify it). The
/// tray passes `hmod + rva` to SetWindowsHookEx; in a process that still holds
/// an *older* build of this DLL (self-pinned, same path) Windows reuses that
/// image and calls old_base + rva. If the RVA had moved, that lands on random
/// bytes and every such process crashes at once — which is what happened
/// when the 980-line lib.rs was split into modules and `GetMsgProc` drifted
/// from 0x1100 to 0x8300.
#[no_mangle]
pub extern "system" fn GetMsgProc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Tray's "reload profile" broadcast: act once per delivered message
    // (PM_REMOVE only, so a PeekMessage(PM_NOREMOVE) doesn't double up).
    let removed = u32::try_from(code) == Ok(HC_ACTION) && u32::try_from(wparam.0) == Ok(PM_REMOVE.0);
    if removed {
        // SAFETY: for WH_GETMESSAGE, lparam is a pointer to the MSG being
        // retrieved, valid for the duration of the hook call (or null).
        let message = unsafe { (lparam.0 as *const MSG).as_ref() }.map(|m| m.message);
        let id = RELOAD_MSG.load(Ordering::Relaxed);
        if id != 0 && message == Some(id) {
            reload_profile();
        }
    }
    // SAFETY: forwarding the hook chain call with the arguments we received.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
