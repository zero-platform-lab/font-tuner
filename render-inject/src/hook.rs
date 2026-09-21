//! Installing the hooks, and doing it without tripping over other threads.
//!
//! Two mechanisms: an inline detour on an exported function (`install_hook`,
//! via retour), and a direct write into a COM vtable slot (`patch_slot`).
//! Both rewrite code or function pointers that other threads may be running,
//! which is what the thread freezing and the patch lock here are for.

use core::ffi::c_void;

use retour::RawDetour;
use std::sync::Mutex;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread,
    THREAD_SUSPEND_RESUME,
};

use crate::log;

/// Serialises the one-time vtable patches (CreateAlphaTexture, D2D factory
/// render-target creation). Without it two threads racing the first call both
/// pass the `is_null()`/`is_none()` check and both patch the slot; the loser
/// then captures the "original" from a slot already holding our detour, so the
/// detour calls itself — infinite recursion, host crash. (The D2D DrawGlyphRun
/// path already guards with SLOT_ORIG's mutex; these paths did not.)
pub(crate) static VTABLE_PATCH_LOCK: Mutex<()> = Mutex::new(());

/// Installed inline detours, kept alive for the life of the process (the DLL
/// pins itself, so they are never disabled). retour patches the target's first
/// bytes non-atomically and does NOT stop other threads while doing so, so a
/// thread executing inside those bytes at that instant would fault. We suspend
/// every other thread in this process around the patch — the same window
/// MinHook closes internally — then leak the detour so it stays enabled.
static DETOURS: Mutex<Vec<RawDetour>> = Mutex::new(Vec::new());

/// Suspend all threads in this process except the caller, for the duration of
/// the returned guard. Resumed (in reverse) on drop. Best-effort: threads that
/// cannot be opened/suspended are skipped.
struct FrozenThreads(Vec<isize>);
impl FrozenThreads {
    unsafe fn all_but_current() -> FrozenThreads {
        let pid = GetCurrentProcessId();
        let me = GetCurrentThreadId();
        let mut handles = Vec::new();
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
            let mut e = THREADENTRY32 { dwSize: core::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
            if Thread32First(snap, &mut e).is_ok() {
                loop {
                    if e.th32OwnerProcessID == pid && e.th32ThreadID != me {
                        if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, e.th32ThreadID) {
                            // SuspendThread returns (DWORD)-1 on failure.
                            if SuspendThread(h) != u32::MAX {
                                handles.push(h.0 as isize);
                            } else {
                                let _ = CloseHandle(h);
                            }
                        }
                    }
                    if Thread32Next(snap, &mut e).is_err() { break; }
                }
            }
            let _ = CloseHandle(snap);
        }
        FrozenThreads(handles)
    }
}
impl Drop for FrozenThreads {
    fn drop(&mut self) {
        for &h in self.0.iter().rev() {
            unsafe {
                let hh = HANDLE(h as *mut c_void);
                ResumeThread(hh);
                let _ = CloseHandle(hh);
            }
        }
    }
}

/// Create + enable an inline detour on `target`, with other threads frozen
/// around the byte patch. `publish` receives the trampoline (the way back to
/// the original) *before* the detour goes live, so a thread that hits the
/// detour the instant it is enabled already finds its `ORIG_*` set.
pub(crate) unsafe fn install_hook(target: *const (), detour: *const (), publish: impl FnOnce(*const ())) -> bool {
    let d = match RawDetour::new(target, detour) {
        Ok(d) => d,
        Err(e) => { log(&format!("detour new failed: {e:?}")); return false; }
    };
    publish(d.trampoline() as *const () as *const ());
    let ok = {
        let _frozen = FrozenThreads::all_but_current();
        d.enable().is_ok()
    };
    if !ok { log("detour enable failed"); return false; }
    if let Ok(mut v) = DETOURS.lock() { v.push(d); }
    true
}

/// Overwrite one vtable slot with `newv`. `publish` receives the previous
/// value before the write, for the same reason as in `install_hook`.
pub(crate) unsafe fn patch_slot(slot: *mut usize, newv: usize, publish: impl FnOnce(usize)) -> bool {
    let mut oldp = PAGE_PROTECTION_FLAGS(0);
    if VirtualProtect(slot as *const c_void, 8, PAGE_EXECUTE_READWRITE, &mut oldp).is_err() { return false; }
    publish(*slot);
    *slot = newv;
    let _ = VirtualProtect(slot as *const c_void, 8, oldp, &mut oldp);
    true
}
