//! Install a WH_GETMESSAGE hook whose procedure lives in RenderCore64.dll,
//! **targeted at one process** (a thread-specific hook, not global). Windows
//! maps the DLL into that process when it pumps messages; its DllMain runs and
//! hooks that process's text drawing — the auto-injection mechanism
//! MacType/font-tuner use, scoped to a single app for safety.
//!
//!   loader <absolute-RenderCore64.dll-path> <target-exe-name> [seconds]
//!
//! The hook is removed (and the DLL cleans up its hooks) when this exits.

use core::ffi::c_void;
use std::iter::once;

use windows::core::{s, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HINSTANCE, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, Thread32First, Thread32Next,
    PROCESSENTRY32W, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HOOKPROC, MSG, WH_GETMESSAGE, WM_QUIT,
};

fn find_pid(name: &str) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut pid = None;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                if String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case(name) {
                    pid = Some(e.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut e).is_err() { break; }
            }
        }
        let _ = CloseHandle(snap);
        pid
    }
}

fn first_thread(pid: u32) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).ok()?;
        let mut e = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut tid = None;
        if Thread32First(snap, &mut e).is_ok() {
            loop {
                if e.th32OwnerProcessID == pid {
                    tid = Some(e.th32ThreadID);
                    break;
                }
                if Thread32Next(snap, &mut e).is_err() { break; }
            }
        }
        let _ = CloseHandle(snap);
        tid
    }
}

use core::mem::size_of;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: loader <RenderCore64.dll-path> <target-exe-name> [seconds]");
        std::process::exit(2);
    }
    let (dll, target) = (&args[1], &args[2]);
    let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(15);

    let pid = find_pid(target).unwrap_or_else(|| { eprintln!("{target} not found"); std::process::exit(1) });
    let tid = first_thread(pid).unwrap_or_else(|| { eprintln!("no thread for pid {pid}"); std::process::exit(1) });

    unsafe {
        let wide: Vec<u16> = dll.encode_utf16().chain(once(0)).collect();
        let hmod = LoadLibraryW(PCWSTR(wide.as_ptr()))
            .unwrap_or_else(|e| { eprintln!("LoadLibrary failed: {e}"); std::process::exit(1) });
        let proc = GetProcAddress(hmod, s!("GetMsgProc"));
        if proc.is_none() { eprintln!("GetMsgProc not found"); std::process::exit(1); }
        let hookproc: HOOKPROC = std::mem::transmute(proc);

        let hook = SetWindowsHookExW(WH_GETMESSAGE, hookproc, Some(HINSTANCE(hmod.0)), tid)
            .unwrap_or_else(|e| { eprintln!("SetWindowsHookEx failed: {e}"); std::process::exit(1) });
        println!("WH_GETMESSAGE hook on {target} (pid={pid}, tid={tid}); active {secs}s");

        let me = GetCurrentThreadId();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            let _ = PostThreadMessageW(me, WM_QUIT, WPARAM(0), LPARAM(0));
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
        println!("hook removed");
        let _: *mut c_void = std::ptr::null_mut();
    }
}
