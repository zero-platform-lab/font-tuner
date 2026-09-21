//! Inject a DLL into a target process by exe name, via the classic
//! CreateRemoteThread(LoadLibraryW) method.
//!
//!   injector <target-exe-name> <absolute-dll-path>
//!
//! Both must be 64-bit. This only loads the DLL into the target; what the DLL
//! then does is up to the DLL (render-inject currently just logs).

use core::ffi::c_void;
use std::iter::once;

use windows::core::{s, w};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, OpenProcess, WaitForSingleObject, INFINITE, LPTHREAD_START_ROUTINE,
    PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ,
    PROCESS_VM_WRITE,
};

fn find_pid(name: &str) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W {
            dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let exe = String::from_utf16_lossy(&e.szExeFile[..n]);
                if exe.eq_ignore_ascii_case(name) {
                    found = Some(e.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

fn inject(pid: u32, dll_path: &str) -> windows::core::Result<()> {
    unsafe {
        let hproc: HANDLE = OpenProcess(
            PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION
                | PROCESS_VM_WRITE | PROCESS_VM_READ,
            false,
            pid,
        )?;

        let wide: Vec<u16> = dll_path.encode_utf16().chain(once(0)).collect();
        let bytes = wide.len() * 2;
        let remote = VirtualAllocEx(hproc, None, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            let _ = CloseHandle(hproc);
            return Err(windows::core::Error::from_thread());
        }
        WriteProcessMemory(hproc, remote, wide.as_ptr() as *const c_void, bytes, None)?;

        let k32 = GetModuleHandleW(w!("kernel32.dll"))?;
        let load = GetProcAddress(k32, s!("LoadLibraryW"));
        let start: LPTHREAD_START_ROUTINE = std::mem::transmute(load);

        let hthread = CreateRemoteThread(hproc, None, 0, start, Some(remote), 0, None)?;
        WaitForSingleObject(hthread, INFINITE);

        let _ = VirtualFreeEx(hproc, remote, 0, MEM_RELEASE);
        let _ = CloseHandle(hthread);
        let _ = CloseHandle(hproc);
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: injector <target-exe-name> <absolute-dll-path>");
        std::process::exit(2);
    }
    let (target, dll) = (&args[1], &args[2]);
    match find_pid(target) {
        Some(pid) => {
            println!("target {target} pid={pid}");
            match inject(pid, dll) {
                Ok(()) => println!("injected {dll}"),
                Err(e) => {
                    eprintln!("inject failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => {
            eprintln!("process {target} not found");
            std::process::exit(1);
        }
    }
}
