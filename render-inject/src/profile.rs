//! Which rendering profile is active, and where it comes from.
//!
//! The tray owns the choice: it writes `[General] AlternativeFile=` into the
//! install directory's `font-tuner.ini`. This module turns that into a
//! `Profile`, at attach and again when the tray broadcasts "reload profile".

use core::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::OnceLock;

use render_core::{tables_for, Profile};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

use crate::{log, RENDER};

/// Registered window message the tray broadcasts for "reload profile"
/// (0 until on_attach registers it). Handled in GetMsgProc on the receiving
/// process's own UI thread: no extra thread, no polling.
pub(crate) static RELOAD_MSG: AtomicU32 = AtomicU32::new(0);
pub(crate) const RELOAD_MSG_NAME: PCWSTR = w!("FontTuner.ReloadProfile");

/// This DLL's module handle, stored as an exposed address because `HINSTANCE`
/// is not `Sync`. Set in DllMain before any other thread of ours starts.
pub(crate) static SELF_HINST: OnceLock<usize> = OnceLock::new();

/// Directory this DLL was loaded from (the install dir: font-tuner.ini + ini\).
fn self_dir() -> Option<PathBuf> {
    let hinst = HINSTANCE(core::ptr::with_exposed_provenance_mut::<c_void>(*SELF_HINST.get()?));
    let mut buf = [0u16; 260];
    // SAFETY: `hinst` is our own module handle (set in DllMain, never
    // unloaded because the DLL pins itself) and `buf` outlives the call.
    let n = unsafe { GetModuleFileNameW(Some(hinst.into()), &mut buf) } as usize;
    if n == 0 {
        return None;
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..n])).parent().map(std::path::Path::to_path_buf)
}

/// The `AlternativeFile=` value from a font-tuner.ini's text, as written by the
/// tray. Pure so it can be tested without a filesystem or a Win32 process.
///
/// Deliberately lenient in the same way `GetPrivateProfileString` is: the key
/// match ignores case and surrounding blanks, `;`/`#` comment lines are skipped,
/// and a value containing `=` (a path can) is kept whole. An empty value means
/// "not set", so the caller falls back to the built-in profile.
fn parse_alternative_file(text: &str) -> Option<&str> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if !key.trim().eq_ignore_ascii_case("AlternativeFile") {
            continue;
        }
        let value = value.trim();
        return (!value.is_empty()).then_some(value);
    }
    None
}

/// Is `exe` (a file name, no directory) listed in the `[UnloadDll]` section of
/// a font-tuner.ini's text? That section is upstream's list of programs the
/// tuning must stay out of: ones with no GUI, ones that dislike an injected
/// DLL, and the tray itself. Pure so it can be tested without a filesystem.
///
/// Lenient like the rest of the ini handling: section and entry names match
/// case-insensitively, blanks are trimmed, `;`/`#` comment lines are skipped,
/// and anything that is not a bare file name (a `key=value` line) is ignored.
fn is_excluded(text: &str, exe: &str) -> bool {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(';') || line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = name.trim().eq_ignore_ascii_case("UnloadDll");
            continue;
        }
        if in_section && !line.contains('=') && line.eq_ignore_ascii_case(exe) {
            return true;
        }
    }
    false
}

/// Should this process be left alone? True when its exe is listed in
/// `[UnloadDll]` of the install dir's font-tuner.ini. Read once at attach;
/// editing the list takes effect in processes started afterwards.
pub(crate) fn is_process_excluded(exe_path: &str) -> bool {
    let Some(dir) = self_dir() else { return false };
    let Ok(text) = std::fs::read_to_string(dir.join("font-tuner.ini")) else { return false };
    let exe = exe_path.rsplit(['\\', '/']).next().unwrap_or(exe_path);
    is_excluded(&text, exe)
}

/// The profile the tray selected: `[General] AlternativeFile=ini\<name>.ini`
/// in the install dir's font-tuner.ini, resolved relative to that dir. Read at
/// attach, and again when the tray broadcasts "reload profile".
fn profile_path() -> Option<String> {
    let dir = self_dir()?;
    let text = std::fs::read_to_string(dir.join("font-tuner.ini")).ok()?;
    let rel = parse_alternative_file(&text)?;
    Some(dir.join(rel).to_string_lossy().into_owned())
}

/// The active profile (path it came from, or None for the built-in default).
pub(crate) fn load_profile() -> (Option<String>, Profile) {
    let path = profile_path();
    let p = path.as_deref().and_then(Profile::from_ini).unwrap_or_else(Profile::clean_greyscale);
    (path, p)
}

/// Re-read font-tuner.ini and swap the profile + tables in, under RENDER_LOCK
/// so no draw observes a half-updated pair. Called on this process's UI
/// thread from GetMsgProc when the tray broadcasts RELOAD_MSG. If the ini is
/// unreadable the built-in default applies, same as at attach.
pub(crate) fn reload_profile() {
    let (path, p) = load_profile();
    if let Ok(mut guard) = RENDER.lock() {
        if let Some(st) = guard.as_mut() {
            st.tables = tables_for(&p);
            st.profile = p;
        }
    }
    crate::dwrite::refresh_dw_rendering(&p);
    log(&format!("reloaded profile {}", path.as_deref().unwrap_or("(default)")));
}

#[cfg(test)]
mod tests {
    use super::*;

    // "Small" tests: pure, no filesystem, no threads, deterministic.

    #[test]
    fn unload_dll_matches_only_bare_names_in_its_own_section() {
        let ini = "[General]\nAlternativeFile=ini\\Clean Greyscale.ini\n\n[UnloadDll]\n; List of .exes\nfont-tuner.exe\n  MsMpEng.exe  \n\n[Exclude]\nnotepad.exe\n";
        assert!(is_excluded(ini, "font-tuner.exe"));
        assert!(is_excluded(ini, "MSMPENG.EXE"), "names match case-insensitively");
        assert!(!is_excluded(ini, "notepad.exe"), "only the [UnloadDll] section counts");
        assert!(!is_excluded(ini, "AlternativeFile"), "key=value lines are not entries");
        assert!(!is_excluded(ini, "explorer.exe"));
        assert!(!is_excluded("", "anything.exe"), "no section, no exclusion");
    }

    #[test]
    fn alternative_file_reads_the_tray_written_key() {
        let ini = "[General]\r\nAlternativeFile=ini\\Clean Greyscale.ini\r\n\r\n[Font-tuner]\r\nAutoRun=1\r\n";
        assert_eq!(parse_alternative_file(ini), Some("ini\\Clean Greyscale.ini"));
    }

    #[test]
    fn alternative_file_ignores_case_blanks_and_comments() {
        let ini = "; AlternativeFile=ini\\commented-out.ini\n\
                   # AlternativeFile=ini\\also-not-this.ini\n\
                   \x20 alternativefile  =   ini\\Accurate.ini   \n";
        assert_eq!(parse_alternative_file(ini), Some("ini\\Accurate.ini"));
    }

    #[test]
    fn alternative_file_keeps_a_value_containing_equals() {
        // A path may contain '=', so only the first '=' separates key from value.
        let ini = "AlternativeFile=ini\\odd=name.ini\n";
        assert_eq!(parse_alternative_file(ini), Some("ini\\odd=name.ini"));
    }

    #[test]
    fn alternative_file_absent_or_empty_means_use_the_default() {
        assert_eq!(parse_alternative_file(""), None);
        assert_eq!(parse_alternative_file("[General]\nRedrawDelay=5000\n"), None);
        assert_eq!(parse_alternative_file("AlternativeFile=\n"), None);
        assert_eq!(parse_alternative_file("AlternativeFile=   \n"), None);
    }

    #[test]
    fn alternative_file_takes_the_first_of_duplicates() {
        // GetPrivateProfileString returns the first; match that so a stray
        // second key cannot silently change the profile.
        let ini = "AlternativeFile=ini\\first.ini\nAlternativeFile=ini\\second.ini\n";
        assert_eq!(parse_alternative_file(ini), Some("ini\\first.ini"));
    }
}
