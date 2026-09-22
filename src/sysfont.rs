//! System-wide UI font swap.
//!
//! Replaces the shell's caption / small-caption / menu / status / message and
//! icon-title fonts (the same fonts you would change under Windows) via
//! SystemParametersInfo. The original set is saved to a backup file the first
//! time we change it, so [`restore`] brings it back exactly. Changes take
//! effect for newly drawn UI immediately; already-open windows and the shell
//! fully pick it up after a sign-out/in.

use std::path::PathBuf;

use windows::Win32::Graphics::Gdi::LOGFONTW;
use windows::Win32::UI::WindowsAndMessaging::{
    NONCLIENTMETRICSW, SPI_GETICONTITLELOGFONT, SPI_GETNONCLIENTMETRICS, SPI_SETICONTITLELOGFONT,
    SPI_SETNONCLIENTMETRICS, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

/// Fixed candidate list: installed, Japanese-capable UI sans fonts. The label
/// shown in the menu is the face name itself.
pub const FONTS: &[&str] = &[
    "BIZ UDPゴシック",
    "BIZ UDゴシック",
    "Noto Sans JP",
    "メイリオ",
];

fn backup_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let dir = PathBuf::from(base).join("font-tuner");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("sysfont-backup.bin"))
}

fn set_face(lf: &mut LOGFONTW, face: &str) {
    lf.lfFaceName = [0u16; 32];
    for (dst, c) in lf.lfFaceName.iter_mut().zip(face.encode_utf16()).take(31) {
        *dst = c;
    }
}

fn face_of(lf: &LOGFONTW) -> String {
    let f = &lf.lfFaceName;
    let n = f.iter().position(|&c| c == 0).unwrap_or(f.len());
    String::from_utf16_lossy(&f[..n])
}

/// `sizeof(T)` as the `cbSize` / `uiParam` value the Win32 struct APIs want.
fn struct_size<T>() -> u32 {
    u32::try_from(std::mem::size_of::<T>()).expect("Win32 structs are far smaller than 4 GiB")
}

/// The raw bytes of a plain-data Win32 struct, for the backup file.
fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `#[repr(C)]` Win32 struct of plain data, so reading
    // its bytes for its whole size is valid for the borrow's lifetime.
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(v).cast::<u8>(), std::mem::size_of::<T>()) }
}

/// A plain-data Win32 struct read back from its bytes.
fn from_bytes<T: Copy>(b: &[u8]) -> Option<T> {
    let bytes = b.get(..std::mem::size_of::<T>())?;
    let mut v = std::mem::MaybeUninit::<T>::uninit();
    // SAFETY: `bytes` is exactly `size_of::<T>()` long and `T` is plain data
    // (every bit pattern is a valid value), so the copy fully initialises `v`.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), v.as_mut_ptr().cast::<u8>(), bytes.len());
        Some(v.assume_init())
    }
}

fn get_ncm() -> Option<NONCLIENTMETRICSW> {
    let mut ncm = NONCLIENTMETRICSW { cbSize: struct_size::<NONCLIENTMETRICSW>(), ..Default::default() };
    // SAFETY: `ncm` carries its own size and outlives the call.
    unsafe {
        SystemParametersInfoW(SPI_GETNONCLIENTMETRICS, ncm.cbSize, Some((&raw mut ncm).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).ok()?;
    }
    Some(ncm)
}

fn get_icon() -> Option<LOGFONTW> {
    let mut lf = LOGFONTW::default();
    // SAFETY: the size passed is exactly `lf`'s, and `lf` outlives the call.
    unsafe {
        SystemParametersInfoW(SPI_GETICONTITLELOGFONT, struct_size::<LOGFONTW>(), Some((&raw mut lf).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).ok()?;
    }
    Some(lf)
}

fn push_ncm(ncm: &NONCLIENTMETRICSW) {
    // SAFETY: `ncm` carries its own size; SPI_SET only reads through the
    // pointer, so handing a shared borrow as `*mut` is sound.
    let _ = unsafe {
        SystemParametersInfoW(SPI_SETNONCLIENTMETRICS, ncm.cbSize, Some(std::ptr::from_ref(ncm).cast_mut().cast()), SPIF_UPDATEINIFILE | SPIF_SENDCHANGE)
    };
}

fn push_icon(lf: &LOGFONTW) {
    // SAFETY: as in `push_ncm`, with `lf`'s exact size.
    let _ = unsafe {
        SystemParametersInfoW(SPI_SETICONTITLELOGFONT, struct_size::<LOGFONTW>(), Some(std::ptr::from_ref(lf).cast_mut().cast()), SPIF_UPDATEINIFILE | SPIF_SENDCHANGE)
    };
}

/// The face name the shell caption currently uses (for the menu check mark).
pub fn current_face() -> Option<String> {
    get_ncm().map(|ncm| face_of(&ncm.lfCaptionFont))
}

/// Set every shell UI font to `face`. Saves the original on the first change.
pub fn apply(face: &str) {
    let (Some(mut ncm), Some(mut icon)) = (get_ncm(), get_icon()) else {
        return;
    };
    if let Some(p) = backup_path()
        && !p.exists()
    {
        let mut bytes = as_bytes(&ncm).to_vec();
        bytes.extend_from_slice(as_bytes(&icon));
        let _ = std::fs::write(&p, &bytes);
    }
    set_face(&mut ncm.lfCaptionFont, face);
    set_face(&mut ncm.lfSmCaptionFont, face);
    set_face(&mut ncm.lfMenuFont, face);
    set_face(&mut ncm.lfStatusFont, face);
    set_face(&mut ncm.lfMessageFont, face);
    set_face(&mut icon, face);
    push_ncm(&ncm);
    push_icon(&icon);
}

/// Restore the fonts saved before the first change, then forget the backup.
/// No-op if we never changed anything.
pub fn restore() {
    let Some(p) = backup_path() else { return };
    let Ok(bytes) = std::fs::read(&p) else { return };
    let split = std::mem::size_of::<NONCLIENTMETRICSW>();
    let (Some(mut ncm), Some(icon)) = (
        from_bytes::<NONCLIENTMETRICSW>(&bytes),
        bytes.get(split..).and_then(from_bytes::<LOGFONTW>),
    ) else {
        return;
    };
    // Do not trust cbSize from the (user-writable) backup file: force it to the
    // real struct size so SystemParametersInfo never reads past our buffer.
    ncm.cbSize = struct_size::<NONCLIENTMETRICSW>();
    push_ncm(&ncm);
    push_icon(&icon);
    let _ = std::fs::remove_file(&p);
}
