//! UI strings. Japanese when the OS UI language is Japanese, English otherwise.

use windows::Win32::Globalization::GetUserDefaultUILanguage;

#[derive(Clone, Copy)]
pub struct Strings {
    pub enabled: &'static str,
    pub profile: &'static str,
    pub reload: &'static str,
    pub version: &'static str,
    pub sysfont: &'static str,
    pub sysfont_default: &'static str,
    pub exit: &'static str,
    pub tip_on: &'static str,
    pub tip_off: &'static str,
    pub err_no_dll: &'static str,
    pub err_hook: &'static str,
    pub err_rva: &'static str,
    pub err_stale: &'static str,
    pub err_already: &'static str,
}

const EN: Strings = Strings {
    enabled: "Enabled",
    profile: "Profile",
    reload: "Reload profile (running apps)",
    version: "Version",
    sysfont: "System font",
    sysfont_default: "Default (restore)",
    exit: "Exit",
    tip_on: "Font-tuner - on",
    tip_off: "Font-tuner - off",
    err_no_dll: "RenderCore64.dll not found. Put font-tuner.exe in the Font-tuner folder.",
    err_hook: "Failed to install the hook.",
    err_rva: "RenderCore64.dll is not laid out as expected (GetMsgProc is not at RVA 0x1000). Hook not installed: it would crash every process that still holds an older core.",
    err_stale: "These programs still hold an older RenderCore64.dll; hooking now would crash them. Sign out and back in (or reboot), then enable Font-tuner:",
    err_already: "Font-tuner is already running.",
};

const JA: Strings = Strings {
    enabled: "有効",
    profile: "プロファイル",
    reload: "プロファイルを再読み込み (起動中のアプリに反映)",
    version: "バージョン",
    sysfont: "システムフォント",
    sysfont_default: "既定に戻す",
    exit: "終了",
    tip_on: "Font-tuner - 有効",
    tip_off: "Font-tuner - 無効",
    err_no_dll: "RenderCore64.dll が見つからない。font-tuner.exe を Font-tuner のフォルダに置く。",
    err_hook: "フックを張れなかった。",
    err_rva: "RenderCore64.dll の配置が想定と違う (GetMsgProc が RVA 0x1000 にない)。古いコアを保持したままのプロセスが全部落ちるため、フックを張らない。",
    err_stale: "古い RenderCore64.dll を保持したままのプログラムがある。今フックを張るとそれらが落ちる。サインアウトして入り直す (または再起動) してから有効にする:",
    err_already: "Font-tuner はすでに起動している。",
};

pub fn current() -> Strings {
    // Primary language id 0x11 = Japanese.
    let lang = unsafe { GetUserDefaultUILanguage() };
    if lang & 0x3ff == 0x11 { JA } else { EN }
}
