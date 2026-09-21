//! font-tuner: a small tray loader for the RenderCore64 text-rendering DLL.
//!
//! It does what the upstream closed-source tray does in "tray mode": install a
//! global WH_GETMESSAGE hook whose procedure lives in RenderCore64.dll, so that
//! every 64-bit GUI process maps the DLL and its DllMain hooks the font APIs.
//! 32-bit processes are out of scope.

#![windows_subsystem = "windows"]

mod lang;
mod stale;
mod sysfont;

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::*;
use windows::Win32::System::WindowsProgramming::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{w, PCSTR, PCWSTR};

const WM_TRAY: u32 = WM_APP + 1;
const ID_ENABLED: usize = 1;
const ID_EXIT: usize = 2;
const ID_RELOAD: usize = 3;
const ID_VERSION: usize = 4;
const ID_PROFILE_BASE: usize = 100;
const ID_SYSFONT_DEFAULT: usize = 200;
const ID_SYSFONT_BASE: usize = 201;
const DLL_NAME: &str = "RenderCore64.dll";
/// Icon resources embedded via app.rc. Black-metallic reads on a light
/// taskbar, silver on a dark one.
const IDI_TRAY_LIGHT: usize = 1; // black metallic, for light taskbar
const IDI_TRAY_DARK: usize = 2; // silver, for dark taskbar

/// Whether the taskbar uses the light theme. Missing value ⇒ dark (the
/// Windows 11 default), so we fall back to the silver icon.
fn taskbar_is_light() -> bool {
    let sub = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = wide("SystemUsesLightTheme");
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut _ as *mut _),
            Some(&mut size),
        )
    };
    r == ERROR_SUCCESS && data != 0
}

/// Load the small (tray-sized) themed icon from our own resources.
fn load_tray_icon(light: bool) -> HICON {
    let id = if light { IDI_TRAY_LIGHT } else { IDI_TRAY_DARK };
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let cx = GetSystemMetrics(SM_CXSMICON);
        let cy = GetSystemMetrics(SM_CYSMICON);
        match LoadImageW(
            Some(HINSTANCE(hinst.0)),
            PCWSTR(id as *const u16),
            IMAGE_ICON,
            cx,
            cy,
            LR_DEFAULTCOLOR,
        ) {
            Ok(h) => HICON(h.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn msgbox(text: &str) {
    let t = wide(text);
    let c = wide("Font-tuner");
    unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | MB_ICONERROR);
    }
}

/// Informational popup (not an error), used for the version item.
fn infobox(text: &str) {
    let t = wide(text);
    let c = wide("Font-tuner");
    unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | MB_ICONINFORMATION);
    }
}

/// The Font-tuner install folder: next to this exe, else the default location.
fn install_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let here = exe.parent()?.to_path_buf();
    if here.join(DLL_NAME).exists() {
        return Some(here);
    }
    let pf = std::env::var_os("ProgramFiles")?;
    let d = Path::new(&pf).join("Font-tuner");
    d.join(DLL_NAME).exists().then_some(d)
}

/// Where `GetMsgProc` must sit inside `RenderCore64.dll`: the first byte of
/// `.text`. The core's `build.rs` pins it there with the linker's `/ORDER`.
///
/// Windows applies `proc - hmod` to whatever image of the DLL a target process
/// already holds. The core self-pins, so after an upgrade the running
/// processes still hold the *previous* build; if the RVA differed, the hook
/// would land on random bytes in them and they would all crash on their next
/// message. So the tray refuses to hook a core whose export has moved.
const HOOK_PROC_RVA: usize = 0x1000;

/// A global WH_GETMESSAGE hook backed by RenderCore64's exported `GetMsgProc`.
struct Hook {
    hhook: HHOOK,
}

enum HookError {
    /// Load, export lookup or `SetWindowsHookExW` failed.
    Install,
    /// `GetMsgProc` is not at `HOOK_PROC_RVA`; hooking would crash every
    /// process that still holds an older core.
    Layout,
    /// These running processes hold a core whose `GetMsgProc` is elsewhere;
    /// hooking would crash them on their next message.
    Stale(Vec<String>),
}

impl Hook {
    fn install(dir: &Path) -> Result<Hook, HookError> {
        let dll = dir.join(DLL_NAME);
        let path = wide(dll.to_str().ok_or(HookError::Install)?);
        unsafe {
            let hmod = LoadLibraryW(PCWSTR(path.as_ptr())).map_err(|_| HookError::Install)?;
            let proc_ = GetProcAddress(hmod, PCSTR(b"GetMsgProc\0".as_ptr())).ok_or(HookError::Install)?;
            if proc_ as usize - hmod.0 as usize != HOOK_PROC_RVA {
                return Err(HookError::Layout);
            }
            let stale = stale::holders_of_stale_core(&dll, HOOK_PROC_RVA);
            if !stale.is_empty() {
                return Err(HookError::Stale(stale));
            }
            let hookproc: HOOKPROC = Some(std::mem::transmute(proc_));
            let hhook = SetWindowsHookExW(WH_GETMESSAGE, hookproc, Some(HINSTANCE(hmod.0)), 0)
                .map_err(|_| HookError::Install)?;
            Ok(Hook { hhook })
        }
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.hhook);
        }
    }
}

struct Profiles {
    ini: PathBuf,
    names: Vec<String>,
}

impl Profiles {
    fn load(dir: &Path) -> Profiles {
        let mut names: Vec<String> = std::fs::read_dir(dir.join("ini"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().into_string().ok()?;
                n.to_ascii_lowercase().ends_with(".ini").then_some(n)
            })
            .collect();
        // Preferred menu order: greyscale pair first, then Accurate, then the
        // LCD "Clean" series. Anything else falls in afterwards, alphabetically.
        const ORDER: &[&str] = &[
            "clean greyscale.ini",
            "clean dark greyscale.ini",
            "accurate.ini",
            "clean sharp.ini",
            "clean sharp dark.ini",
        ];
        names.sort_by_key(|n| {
            let lower = n.to_ascii_lowercase();
            let rank = ORDER.iter().position(|o| *o == lower).unwrap_or(ORDER.len());
            (rank, lower)
        });
        Profiles { ini: dir.join("font-tuner.ini"), names }
    }

    fn ini_path(&self) -> Vec<u16> {
        wide(self.ini.to_str().unwrap_or(""))
    }

    /// File name part of `[General] AlternativeFile=`.
    fn current(&self) -> String {
        let mut buf = [0u16; 260];
        let (sec, key, def, file) = (wide("General"), wide("AlternativeFile"), wide(""), self.ini_path());
        let n = unsafe {
            GetPrivateProfileStringW(
                PCWSTR(sec.as_ptr()),
                PCWSTR(key.as_ptr()),
                PCWSTR(def.as_ptr()),
                Some(&mut buf),
                PCWSTR(file.as_ptr()),
            )
        };
        let v = String::from_utf16_lossy(&buf[..n as usize]);
        v.rsplit(['\\', '/']).next().unwrap_or("").to_string()
    }

    fn select(&self, name: &str) -> bool {
        let val = wide(&format!("ini\\{name}"));
        let (sec, key, file) = (wide("General"), wide("AlternativeFile"), self.ini_path());
        unsafe {
            WritePrivateProfileStringW(
                PCWSTR(sec.as_ptr()),
                PCWSTR(key.as_ptr()),
                PCWSTR(val.as_ptr()),
                PCWSTR(file.as_ptr()),
            )
            .is_ok()
        }
    }
}

struct App {
    dir: PathBuf,
    hwnd: HWND,
    hook: Option<Hook>,
    profiles: Profiles,
    s: lang::Strings,
    taskbar_created: u32,
    hicon: HICON,
    icon_light: bool,
}

impl App {
    fn enabled(&self) -> bool {
        self.hook.is_some()
    }

    /// Turn the hook on or off. On failure returns the message to show the
    /// user; the caller shows it *after* releasing the `APP` borrow, because
    /// `MessageBoxW` runs a modal loop that re-enters `wndproc`, and a nested
    /// `with_app` would panic on the live `borrow_mut`.
    #[must_use]
    fn set_enabled(&mut self, on: bool) -> Option<String> {
        let mut err = None;
        if on {
            self.hook = match Hook::install(&self.dir) {
                Ok(h) => Some(h),
                Err(e) => {
                    err = Some(match e {
                        HookError::Install => self.s.err_hook.to_string(),
                        HookError::Layout => self.s.err_rva.to_string(),
                        HookError::Stale(names) => format!("{}

{}", self.s.err_stale, names.join(", ")),
                    });
                    None
                }
            };
        } else {
            self.hook = None;
        }
        self.update_icon(NIM_MODIFY);
        err
    }

    fn icon_data(&self) -> NOTIFYICONDATAW {
        let mut d = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.hicon,
            ..Default::default()
        };
        let tip = wide(if self.enabled() { self.s.tip_on } else { self.s.tip_off });
        let n = tip.len().min(d.szTip.len());
        d.szTip[..n].copy_from_slice(&tip[..n]);
        d
    }

    fn update_icon(&self, how: NOTIFY_ICON_MESSAGE) {
        unsafe {
            let _ = Shell_NotifyIconW(how, &self.icon_data());
        }
    }

    /// Re-pick the tray icon if the taskbar's light/dark theme changed.
    /// Called on WM_SETTINGCHANGE; a no-op when the theme is unchanged.
    fn refresh_theme_icon(&mut self) {
        let light = taskbar_is_light();
        if light == self.icon_light && !self.hicon.is_invalid() {
            return;
        }
        let old = self.hicon;
        self.hicon = load_tray_icon(light);
        self.icon_light = light;
        self.update_icon(NIM_MODIFY);
        unsafe {
            let _ = DestroyIcon(old);
        }
    }

    fn build_menu(&mut self) -> HMENU {
        // Re-read ini\ each time so files added or removed show up without a restart.
        self.profiles = Profiles::load(&self.dir);
        unsafe {
            let menu = CreatePopupMenu().unwrap_or_default();
            let sub = CreatePopupMenu().unwrap_or_default();
            let cur = self.profiles.current();
            for (i, n) in self.profiles.names.iter().enumerate() {
                let checked = if n.eq_ignore_ascii_case(&cur) { MF_CHECKED } else { MF_UNCHECKED };
                let t = wide(n.trim_end_matches(".ini"));
                let _ = AppendMenuW(sub, MF_STRING | checked, ID_PROFILE_BASE + i, PCWSTR(t.as_ptr()));
            }
            // System-font submenu: "restore default" then the fixed font list.
            let fsub = CreatePopupMenu().unwrap_or_default();
            let cur_face = sysfont::current_face().unwrap_or_default();
            let td = wide(self.s.sysfont_default);
            let _ = AppendMenuW(fsub, MF_STRING, ID_SYSFONT_DEFAULT, PCWSTR(td.as_ptr()));
            let _ = AppendMenuW(fsub, MF_SEPARATOR, 0, PCWSTR::null());
            for (i, f) in sysfont::FONTS.iter().enumerate() {
                let checked = if cur_face == *f { MF_CHECKED } else { MF_UNCHECKED };
                let t = wide(f);
                let _ = AppendMenuW(fsub, MF_STRING | checked, ID_SYSFONT_BASE + i, PCWSTR(t.as_ptr()));
            }

            let en = if self.enabled() { MF_CHECKED } else { MF_UNCHECKED };
            let (t1, t2, tf, t3) = (
                wide(self.s.enabled),
                wide(self.s.profile),
                wide(self.s.sysfont),
                wide(self.s.exit),
            );
            let _ = AppendMenuW(menu, MF_STRING | en, ID_ENABLED, PCWSTR(t1.as_ptr()));
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(t2.as_ptr()));
            let tr = wide(self.s.reload);
            let _ = AppendMenuW(menu, MF_STRING, ID_RELOAD, PCWSTR(tr.as_ptr()));
            let _ = AppendMenuW(menu, MF_POPUP, fsub.0 as usize, PCWSTR(tf.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let tv = wide(&format!("{} {}", self.s.version, env!("CARGO_PKG_VERSION")));
            let _ = AppendMenuW(menu, MF_STRING, ID_VERSION, PCWSTR(tv.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, PCWSTR(t3.as_ptr()));
            menu
        }
    }
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.borrow_mut().as_mut().map(f))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => {
                let ev = lparam.0 as u32;
                if ev == WM_RBUTTONUP || ev == WM_LBUTTONUP || ev == WM_CONTEXTMENU {
                    // Build the menu, then drop the borrow: TrackPopupMenu runs a
                    // modal loop that re-enters this procedure.
                    let Some(menu) = with_app(|a| a.build_menu()) else {
                        return LRESULT(0);
                    };
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let _ = SetForegroundWindow(hwnd);
                    let cmd = TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                        pt.x,
                        pt.y,
                        None,
                        hwnd,
                        None,
                    )
                    .0 as usize;
                    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                    let _ = DestroyMenu(menu);
                    handle_command(hwnd, cmd);
                }
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                // Fires on theme (light/dark) changes; swap the icon if needed.
                with_app(|a| a.refresh_theme_icon());
                LRESULT(0)
            }
            WM_DESTROY => {
                with_app(|a| {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &a.icon_data());
                    let _ = a.set_enabled(false);
                    let _ = DestroyIcon(a.hicon);
                });
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => {
                if msg != 0 && with_app(|a| a.taskbar_created) == Some(msg) {
                    // Explorer restarted; put the icon back.
                    with_app(|a| a.update_icon(NIM_ADD));
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }
}

fn handle_command(hwnd: HWND, cmd: usize) {
    match cmd {
        ID_ENABLED => {
            if let Some(Some(err)) = with_app(|a| a.set_enabled(!a.enabled())) {
                msgbox(&err);
            }
        }
        ID_EXIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        // Ask every injected process to re-read font-tuner.ini. The core's
        // GetMsgProc handles this message on each process's own UI thread.
        ID_RELOAD => unsafe {
            let id = RegisterWindowMessageW(w!("FontTuner.ReloadProfile"));
            let _ = PostMessageW(Some(HWND_BROADCAST), id, WPARAM(0), LPARAM(0));
        },
        ID_VERSION => infobox(&format!(
            "Font-tuner {}\n\
             License: GPL-3.0-only\n\
             Source: {}\n\n\
             Rendering core: RenderCore64 (Rust port), statically linked with\n\
             the FreeType library.\n\
             Portions of this software are copyright \u{00A9} The FreeType\n\
             Project (www.freetype.org). All rights reserved.",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_REPOSITORY"),
        )),
        ID_SYSFONT_DEFAULT => sysfont::restore(),
        c if (ID_SYSFONT_BASE..ID_SYSFONT_BASE + sysfont::FONTS.len()).contains(&c) => {
            sysfont::apply(sysfont::FONTS[c - ID_SYSFONT_BASE]);
        }
        c if c >= ID_PROFILE_BASE => {
            with_app(|a| {
                if let Some(n) = a.profiles.names.get(c - ID_PROFILE_BASE).cloned() {
                    a.profiles.select(&n);
                }
            });
        }
        _ => {}
    }
}

fn main() {
    let s = lang::current();
    let Some(dir) = install_dir() else {
        msgbox(s.err_no_dll);
        return;
    };
    unsafe {
        let name = wide("Local\\font-tuner");
        let _mutex = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            msgbox(s.err_already);
            return;
        }

        let class = wide("font-tuner");
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: HINSTANCE(hinst.0),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(class.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(HINSTANCE(hinst.0)),
            None,
        ) else {
            return;
        };

        let tb = wide("TaskbarCreated");
        let taskbar_created = RegisterWindowMessageW(PCWSTR(tb.as_ptr()));
        let profiles = Profiles::load(&dir);
        let icon_light = taskbar_is_light();
        let hicon = load_tray_icon(icon_light);
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                dir,
                hwnd,
                hook: None,
                profiles,
                s,
                taskbar_created,
                hicon,
                icon_light,
            });
        });
        let err = with_app(|a| {
            a.update_icon(NIM_ADD);
            a.set_enabled(true)
        });
        if let Some(Some(err)) = err {
            msgbox(&err);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        APP.with(|a| *a.borrow_mut() = None);
    }
}
