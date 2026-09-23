//! Installing the hooks, and doing it without tripping over other threads.
//!
//! Two mechanisms: an inline detour on an exported function (`install_hook`,
//! via retour), and a direct write into a COM vtable slot (`patch_slot`).
//! Both rewrite code or function pointers that other threads may be running,
//! which is what the thread freezing and the patch lock here are for.

use core::ffi::c_void;
use std::sync::Mutex;

use retour::RawDetour;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
};

use crate::log;

/// Installed inline detours, kept alive for the life of the process (the DLL
/// pins itself, so they are never disabled). retour patches the target's first
/// bytes non-atomically and does NOT stop other threads while doing so, so a
/// thread executing inside those bytes at that instant would fault. We suspend
/// every other thread in this process around the patch — the same window
/// MinHook closes internally — then leak the detour so it stays enabled.
static DETOURS: Mutex<Vec<RawDetour>> = Mutex::new(Vec::new());

/// `sizeof(T)` as the `dwSize` field Toolhelp structures want.
pub(crate) fn struct_size<T>() -> u32 {
    u32::try_from(core::mem::size_of::<T>()).expect("Win32 structs are far smaller than 4 GiB")
}

/// Suspend all threads in this process except the caller, for the duration of
/// the returned guard. Resumed (in reverse) on drop. Best-effort: threads that
/// cannot be opened/suspended are skipped.
struct FrozenThreads(Vec<HANDLE>);

impl FrozenThreads {
    fn all_but_current() -> FrozenThreads {
        // SAFETY: plain Win32 calls; every handle we keep was opened here and
        // is closed in `drop`, and the snapshot handle is closed before return.
        unsafe {
            let pid = GetCurrentProcessId();
            let me = GetCurrentThreadId();
            let mut handles = Vec::new();
            if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
                let mut e = THREADENTRY32 { dwSize: struct_size::<THREADENTRY32>(), ..Default::default() };
                let mut ok = Thread32First(snap, &raw mut e).is_ok();
                while ok {
                    if e.th32OwnerProcessID == pid && e.th32ThreadID != me {
                        if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, e.th32ThreadID) {
                            // SuspendThread returns (DWORD)-1 on failure.
                            if SuspendThread(h) == u32::MAX {
                                let _ = CloseHandle(h);
                            } else {
                                handles.push(h);
                            }
                        }
                    }
                    ok = Thread32Next(snap, &raw mut e).is_ok();
                }
                let _ = CloseHandle(snap);
            }
            FrozenThreads(handles)
        }
    }
}

impl Drop for FrozenThreads {
    fn drop(&mut self) {
        for &h in self.0.iter().rev() {
            // SAFETY: `h` was opened with THREAD_SUSPEND_RESUME and suspended
            // exactly once in `all_but_current`; nobody else closes it.
            unsafe {
                ResumeThread(h);
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Create + enable an inline detour on `target`, with other threads frozen
/// around the byte patch. `publish` receives the trampoline (the way back to
/// the original) *before* the detour goes live, so a thread that hits the
/// detour the instant it is enabled already finds its `ORIG_*` set.
///
/// # Safety
/// `target` must be the entry of a function that stays mapped for the life
/// of the process, and `detour` a function with the identical ABI and
/// signature.
pub(crate) unsafe fn install_hook(target: *const (), detour: *const (), publish: impl FnOnce(*const ())) -> bool {
    // SAFETY: forwarded from the caller's contract above.
    let d = match unsafe { RawDetour::new(target, detour) } {
        Ok(d) => d,
        Err(e) => {
            log(&format!("detour new failed: {e:?}"));
            return false;
        }
    };
    publish(core::ptr::from_ref(d.trampoline()));
    let ok = {
        let _frozen = FrozenThreads::all_but_current();
        // SAFETY: every other thread is suspended, so none can be executing
        // the bytes retour overwrites.
        unsafe { d.enable() }.is_ok()
    };
    if !ok {
        log("detour enable failed");
        return false;
    }
    if let Ok(mut v) = DETOURS.lock() {
        v.push(d);
    }
    true
}

/// Overwrite one vtable slot with `newv`. `publish` receives the previous
/// value before the write, for the same reason as in `install_hook`.
///
/// # Safety
/// `slot` must point at a live, pointer-aligned vtable entry, and `newv` at a
/// function with the ABI and signature that entry's callers expect.
pub(crate) unsafe fn patch_slot(slot: *mut *const (), newv: *const (), publish: impl FnOnce(*const ())) -> bool {
    let bytes = core::mem::size_of::<*const ()>();
    let mut oldp = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: the caller guarantees `slot` is a live vtable entry; the page
    // is made writable for exactly this write and restored afterwards.
    unsafe {
        if VirtualProtect(slot.cast::<c_void>(), bytes, PAGE_EXECUTE_READWRITE, &raw mut oldp).is_err() {
            return false;
        }
        publish(*slot);
        *slot = newv;
        let _ = VirtualProtect(slot.cast::<c_void>(), bytes, oldp, &raw mut oldp);
    }
    true
}
