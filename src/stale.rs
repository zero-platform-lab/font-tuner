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

use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS};
use windows::Win32::Foundation::{CloseHandle, ERROR_BAD_LENGTH, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

/// Exe names of processes holding the core at `dll_path` whose `GetMsgProc`
/// RVA is not `expected`. A process that has the module but whose image we
/// cannot read counts as stale (better to refuse than to guess). Processes
/// whose module list we cannot even snapshot (other users, higher integrity
/// — which the hook does not reach either) are skipped. A copy of the DLL
/// loaded from another
/// directory (the `loader` test harness) is not a problem: Windows resolves
/// the hook DLL by path and maps the installed one as a separate image.
pub fn holders_of_stale_core(dll_path: &Path, expected: usize) -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: no arguments.
    let me = unsafe { GetCurrentProcessId() };
    for (pid, exe) in processes() {
        if pid == me {
            continue;
        }
        let Some(base) = module_base(pid, dll_path) else { continue };
        // The module is there: from here on anything we cannot read is stale —
        // unless the process simply exited between the snapshot and the read.
        let p = Remote::open(pid);
        let rva = p.as_ref().and_then(|p| export_rva(p, base, "GetMsgProc"));
        if rva != Some(expected) && !p.as_ref().is_some_and(Remote::exited) {
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

/// `sizeof(T)` as the `dwSize` field Toolhelp structures want.
fn struct_size<T>() -> u32 {
    u32::try_from(std::mem::size_of::<T>()).expect("Win32 structs are far smaller than 4 GiB")
}

fn processes() -> Vec<(u32, String)> {
    let mut v = Vec::new();
    // SAFETY: a Toolhelp walk over our own entry struct; the snapshot handle
    // is closed before returning.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return v };
        let mut e = PROCESSENTRY32W { dwSize: struct_size::<PROCESSENTRY32W>(), ..Default::default() };
        let mut ok = Process32FirstW(snap, &raw mut e).is_ok();
        while ok {
            v.push((e.th32ProcessID, wide_to_string(&e.szExeFile)));
            ok = Process32NextW(snap, &raw mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    v
}

/// Base address of the module loaded from `dll_path` in `pid`, if any.
fn module_base(pid: u32, dll_path: &Path) -> Option<usize> {
    let want = dll_path.to_string_lossy();
    // SAFETY: as in `processes`, with the retry Microsoft Learn asks for.
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
        let mut e = MODULEENTRY32W { dwSize: struct_size::<MODULEENTRY32W>(), ..Default::default() };
        let mut found = None;
        let mut ok = Module32FirstW(snap, &raw mut e).is_ok();
        while ok && found.is_none() {
            if wide_to_string(&e.szExePath).eq_ignore_ascii_case(&want) {
                found = Some(e.modBaseAddr.addr());
            }
            ok = Module32NextW(snap, &raw mut e).is_ok();
        }
        let _ = CloseHandle(snap);
        found
    }
}

struct Remote(HANDLE);

impl Remote {
    fn open(pid: u32) -> Option<Remote> {
        // SAFETY: the handle is closed in `drop`.
        unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok().map(Remote) }
    }
    fn exited(&self) -> bool {
        // SAFETY: a zero-timeout wait on our own process handle.
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let mut got = 0usize;
        // SAFETY: `buf` is `len` bytes; the remote address is only read, and
        // a bad one fails the call rather than faulting us.
        unsafe {
            ReadProcessMemory(self.0, std::ptr::with_exposed_provenance(addr), buf.as_mut_ptr().cast(), len, Some(&raw mut got)).ok()?;
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
        // SAFETY: closing the handle `open` received, once.
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

/// RVA of the named export read from the PE *file* at `path` (no mapping,
/// so the tray can inspect a core before it loads — and self-hooks — it).
/// Same walk as `export_rva`, translating RVAs through the section table.
pub fn file_export_rva(path: &Path, name: &str) -> Option<usize> {
    let d = std::fs::read(path).ok()?;
    let u32_at = |o: usize| d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let u16_at = |o: usize| d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let pe = u32_at(0x3c)?;
    if u32_at(pe)? != 0x4550 {
        return None;
    }
    let nsec = u16_at(pe + 6)?;
    let optsz = u16_at(pe + 20)?;
    let sec0 = pe + 24 + optsz;
    let to_off = |rva: usize| -> Option<usize> {
        (0..nsec).find_map(|i| {
            let s = sec0 + 40 * i;
            let (vs, va, raw) = (u32_at(s + 8)?, u32_at(s + 12)?, u32_at(s + 20)?);
            (va <= rva && rva < va + vs).then(|| raw + rva - va)
        })
    };
    let exp = u32_at(pe + 24 + 112)?;
    if exp == 0 {
        return None;
    }
    let dir = to_off(exp)?;
    let n_names = u32_at(dir + 24)?;
    let funcs = to_off(u32_at(dir + 28)?)?;
    let names = to_off(u32_at(dir + 32)?)?;
    let ords = to_off(u32_at(dir + 36)?)?;
    for i in 0..n_names.min(4096) {
        let n = to_off(u32_at(names + 4 * i)?)?;
        let s = d.get(n..n + name.len() + 1)?;
        if &s[..name.len()] == name.as_bytes() && s[name.len()] == 0 {
            let ord = u16_at(ords + 2 * i)?;
            return u32_at(funcs + 4 * ord);
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

    /// The file walk agrees with check-export-rva.ps1 on the built core.
    #[test]
    fn file_export_rva_of_built_core() {
        let dll = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("render-inject/target/release/RenderCore64.dll");
        if !dll.exists() {
            return;
        }
        assert_eq!(super::file_export_rva(&dll, "GetMsgProc"), Some(0x1000));
        assert_eq!(super::file_export_rva(&dll, "NoSuchExport"), None);
    }
}
