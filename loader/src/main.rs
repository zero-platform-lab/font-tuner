//! Install a WH_GETMESSAGE hook whose procedure lives in RenderCore64.dll,
//! **targeted at one process** (a thread-specific hook, not global). Windows
//! maps the DLL into that process when it pumps messages; its DllMain runs and
//! hooks that process's text drawing — the auto-injection mechanism
//! MacType/font-tuner use, scoped to a single app for safety.
//!
//!   loader <absolute-RenderCore64.dll-path> <target-exe-name> [seconds]
//!
//! The hook is removed (and the DLL cleans up its hooks) when this exits.

use std::iter::once;
use std::time::Duration;

use windows::core::{s, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HINSTANCE, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, Thread32First, Thread32Next, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    HOOKPROC, MSG, WH_GETMESSAGE, WM_QUIT,
};

/// `sizeof(T)` as the `dwSize` field Toolhelp structures want.
fn struct_size<T>() -> u32 {
    u32::try_from(size_of::<T>()).expect("Win32 structs are far smaller than 4 GiB")
}

/// A Toolhelp snapshot handle, closed on drop.
struct Snapshot(HANDLE);

impl Snapshot {
    fn new(flags: windows::Win32::System::Diagnostics::ToolHelp::CREATE_TOOLHELP_SNAPSHOT_FLAGS) -> Option<Snapshot> {
        // SAFETY: a plain Win32 call; the handle is closed in `drop`.
        unsafe { CreateToolhelp32Snapshot(flags, 0) }.ok().map(Snapshot)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        // SAFETY: closing the handle `new` opened, once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn exe_name(entry: &PROCESSENTRY32W) -> String {
    let n = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
    String::from_utf16_lossy(&entry.szExeFile[..n])
}

fn find_pid(name: &str) -> Option<u32> {
    let snap = Snapshot::new(TH32CS_SNAPPROCESS)?;
    let mut e = PROCESSENTRY32W { dwSize: struct_size::<PROCESSENTRY32W>(), ..Default::default() };
    // SAFETY: `e` has its dwSize set and outlives the calls.
    let mut ok = unsafe { Process32FirstW(snap.0, &raw mut e) }.is_ok();
    while ok {
        if exe_name(&e).eq_ignore_ascii_case(name) {
            return Some(e.th32ProcessID);
        }
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snap.0, &raw mut e) }.is_ok();
    }
    None
}

fn first_thread(pid: u32) -> Option<u32> {
    let snap = Snapshot::new(TH32CS_SNAPTHREAD)?;
    let mut e = THREADENTRY32 { dwSize: struct_size::<THREADENTRY32>(), ..Default::default() };
    // SAFETY: `e` has its dwSize set and outlives the calls.
    let mut ok = unsafe { Thread32First(snap.0, &raw mut e) }.is_ok();
    while ok {
        if e.th32OwnerProcessID == pid {
            return Some(e.th32ThreadID);
        }
        // SAFETY: as above.
        ok = unsafe { Thread32Next(snap.0, &raw mut e) }.is_ok();
    }
    None
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: loader <RenderCore64.dll-path> <target-exe-name> [seconds]");
        std::process::exit(2);
    }
    let (dll, target) = (&args[1], &args[2]);
    let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(15);

    let pid = find_pid(target).unwrap_or_else(|| fail(&format!("{target} not found")));
    let tid = first_thread(pid).unwrap_or_else(|| fail(&format!("no thread for pid {pid}")));

    let wide: Vec<u16> = dll.encode_utf16().chain(once(0)).collect();
    // SAFETY: `wide` is NUL-terminated; the export name is a static literal;
    // GetMsgProc has HOOKPROC's exact signature (it is written for
    // SetWindowsHookEx), which the transmute types.
    let (hmod, hookproc) = unsafe {
        let hmod = LoadLibraryW(PCWSTR(wide.as_ptr())).unwrap_or_else(|e| fail(&format!("LoadLibrary failed: {e}")));
        let proc = GetProcAddress(hmod, s!("GetMsgProc")).unwrap_or_else(|| fail("GetMsgProc not found"));
        (hmod, std::mem::transmute::<unsafe extern "system" fn() -> isize, HOOKPROC>(proc))
    };

    // SAFETY: a thread-scoped hook on a thread we just enumerated; unhooked
    // below after the message loop ends.
    let hook = unsafe { SetWindowsHookExW(WH_GETMESSAGE, hookproc, Some(HINSTANCE(hmod.0)), tid) }
        .unwrap_or_else(|e| fail(&format!("SetWindowsHookEx failed: {e}")));
    println!("WH_GETMESSAGE hook on {target} (pid={pid}, tid={tid}); active {secs}s");

    // SAFETY: no arguments.
    let me = unsafe { GetCurrentThreadId() };
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        // SAFETY: posting to our own main thread, which is pumping messages.
        let _ = unsafe { PostThreadMessageW(me, WM_QUIT, WPARAM(0), LPARAM(0)) };
    });

    let mut msg = MSG::default();
    // SAFETY: a standard message loop over our own MSG.
    unsafe {
        while GetMessageW(&raw mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
        let _ = UnhookWindowsHookEx(hook);
    }
    println!("hook removed");
}
