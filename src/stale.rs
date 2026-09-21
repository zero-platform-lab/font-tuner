//! Find running processes that still hold a `RenderCore64.dll` whose
//! `GetMsgProc` is not where the tray is about to point the hook.
//!
//! The core self-pins, so a process keeps whatever build it first loaded.
//! `SetWindowsHookEx` records only `proc - hmod`, and in such a process
//! Windows calls `old_base + that RVA` in the old image. If the two builds
//! disagree on the RVA, that is random bytes and the process dies on its next
//! message. The tray runs this check before hooking and refuses when anything
//! would be hit; the user signs out (or reboots) and the stale images are gone.

use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, ERROR_BAD_LENGTH, HANDLE};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

/// Exe names of processes holding the core at `dll_path` whose `GetMsgProc`
/// RVA is not `expected`. A process that has the module but whose image we
/// cannot read counts as stale (better to refuse than to guess). Processes we
/// cannot open at all (other users, higher integrity — which the hook does
/// not reach either) are skipped. A copy of the DLL loaded from another
/// directory (the `loader` test harness) is not a problem: Windows resolves
/// the hook DLL by path and maps the installed one as a separate image.
pub fn holders_of_stale_core(dll_path: &Path, expected: usize) -> Vec<String> {
    let mut out = Vec::new();
    let me = unsafe { GetCurrentProcessId() };
    for (pid, exe) in processes() {
        if pid == me {
            continue;
        }
        let Some(base) = module_base(pid, dll_path) else { continue };
        let Some(p) = Remote::open(pid) else { continue };
        if export_rva(&p, base, "GetMsgProc") != Some(expected) {
            out.push(exe);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn wide_to_string(w: &[u16]) -> String {
    let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..n])
}

fn processes() -> Vec<(u32, String)> {
    let mut v = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return v };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                v.push((e.th32ProcessID, wide_to_string(&e.szExeFile)));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    v
}

/// Base address of the module loaded from `dll_path` in `pid`, if any.
fn module_base(pid: u32, dll_path: &Path) -> Option<usize> {
    let want = dll_path.to_string_lossy();
    unsafe {
        // A module snapshot fails with ERROR_BAD_LENGTH while the target is
        // mid-load; Microsoft Learn says to retry until it succeeds. Give up
        // after a bounded number of tries rather than spin.
        let mut snap = None;
        for _ in 0..50 {
            match CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) {
                Ok(h) => {
                    snap = Some(h);
                    break;
                }
                Err(e) if e.code() == ERROR_BAD_LENGTH.to_hresult() => std::thread::sleep(std::time::Duration::from_millis(2)),
                Err(_) => break,
            }
        }
        let snap = snap?;
        let mut e = MODULEENTRY32W { dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        if Module32FirstW(snap, &mut e).is_ok() {
            loop {
                if wide_to_string(&e.szExePath).eq_ignore_ascii_case(&want) {
                    found = Some(e.modBaseAddr as usize);
                    break;
                }
                if Module32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

struct Remote(HANDLE);

impl Remote {
    fn open(pid: u32) -> Option<Remote> {
        unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok().map(Remote) }
    }
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let mut got = 0usize;
        unsafe {
            ReadProcessMemory(self.0, addr as *const _, buf.as_mut_ptr() as *mut _, len, Some(&mut got)).ok()?;
        }
        (got == len).then_some(buf)
    }
    fn u32(&self, addr: usize) -> Option<u32> {
        self.read(addr, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u16(&self, addr: usize) -> Option<u16> {
        self.read(addr, 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// RVA of the named export in the PE image mapped at `base` inside `p`.
/// Walks the export directory through `ReadProcessMemory`; RVAs are offsets
/// from `base` because the image is mapped, not read from disk.
fn export_rva(p: &Remote, base: usize, name: &str) -> Option<usize> {
    let e_lfanew = p.u32(base + 0x3c)? as usize;
    let pe = base + e_lfanew;
    if p.u32(pe)? != 0x4550 {
        return None;
    }
    // PE32+ optional header: data directory 0 (export) at +112 from the
    // optional header start (pe + 24).
    let exp = p.u32(pe + 24 + 112)? as usize;
    if exp == 0 {
        return None;
    }
    let dir = base + exp;
    let n_names = p.u32(dir + 24)? as usize;
    let funcs = base + p.u32(dir + 28)? as usize;
    let names = base + p.u32(dir + 32)? as usize;
    let ords = base + p.u32(dir + 36)? as usize;
    for i in 0..n_names.min(4096) {
        let nrva = p.u32(names + 4 * i)? as usize;
        let s = p.read(base + nrva, name.len() + 1)?;
        if &s[..name.len()] == name.as_bytes() && s[name.len()] == 0 {
            let ord = p.u16(ords + 2 * i)? as usize;
            return p.u32(funcs + 4 * ord).map(|v| v as usize);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    /// Prints what the check sees on this machine (`cargo test -- --nocapture`);
    /// with no core loaded anywhere the list is simply empty.
    #[test]
    fn scan_runs() {
        let dll = std::path::Path::new(r"C:\Program Files\Font-tuner\RenderCore64.dll");
        let v = super::holders_of_stale_core(dll, 0x1000);
        eprintln!("stale holders: {v:?}");
    }
}
