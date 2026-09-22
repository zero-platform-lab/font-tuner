//! "Custom" profile: a dialog with sliders/combos for the rendering knobs the
//! core reads, a live preview drawn by render-core (offline, no injection), and
//! `Apply`, which writes `%APPDATA%\Font-tuner\Custom.ini` and points
//! `font-tuner.ini`'s `AlternativeFile=` at it by absolute path. The injected
//! core resolves that path as is (`Path::join` keeps an absolute right-hand
//! side), so nothing in `RenderCore64.dll` changes for this feature.
//!
//! The preview font comes from GDI the same way the injected core gets fonts:
//! `GetFontData` on a font selected into a memory DC, handed to FreeType as an
//! in-memory face. The font picker is the stock `ChooseFont` dialog.

use core::ffi::c_void;
use std::cell::RefCell;
use std::path::{Path, PathBuf};

use render_core::{tables_for, Aa, Canvas, Ft, Ink, Profile};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateFontIndirectW, DeleteDC, DeleteObject, EndPaint, FrameRect, GetFontData,
    GetStockObject, GetTextMetricsW, InvalidateRect, SelectObject, SetDIBitsToDevice, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, GRAY_BRUSH, HBRUSH, HFONT, HGDIOBJ, LOGFONTW, PAINTSTRUCT, TEXTMETRICW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WindowsProgramming::WritePrivateProfileStringW;
use windows::Win32::UI::Controls::Dialogs::{ChooseFontW, CommDlgExtendedError, CF_INITTOLOGFONTSTRUCT, CF_NOVERTFONTS, CF_SCREENFONTS, CHOOSEFONTW};
use windows::Win32::UI::Controls::{TBM_SETPOS, TBM_SETRANGEMAX, TBM_SETRANGEMIN, TBS_HORZ, TBS_NOTICKS};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW, LoadIconW, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, SendMessageW, SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow,
    BS_PUSHBUTTON, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CS_HREDRAW, CS_VREDRAW, ES_READONLY,
    HMENU, HWND_BROADCAST, IDC_ARROW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SWP_NOMOVE, SWP_NOZORDER, SW_SHOW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_HSCROLL, WM_PAINT, WM_SETFONT,
    WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_EX_APPWINDOW, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE,
};

use crate::{lang, struct_size, wide};

/// File name of the custom profile (under `%APPDATA%\Font-tuner\`).
pub(crate) const FILE_NAME: &str = "Custom.ini";

/// `%APPDATA%\Font-tuner\Custom.ini`, or None when APPDATA is unset.
pub(crate) fn ini_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(Path::new(&appdata).join("Font-tuner").join(FILE_NAME))
}

/// Point `[General] AlternativeFile=` at the custom ini by absolute path.
/// `ini` is the install dir's font-tuner.ini.
pub(crate) fn select(ini: &Path) -> bool {
    let Some(custom) = ini_path() else { return false };
    let (sec, key) = (wide("General"), wide("AlternativeFile"));
    let (val, file) = (wide(&custom.to_string_lossy()), wide(&ini.to_string_lossy()));
    // SAFETY: every string is NUL-terminated and outlives the call.
    unsafe { WritePrivateProfileStringW(PCWSTR(sec.as_ptr()), PCWSTR(key.as_ptr()), PCWSTR(val.as_ptr()), PCWSTR(file.as_ptr())).is_ok() }
}

/// The custom ini's text for `p`: the keys `Profile::from_ini_str` reads, in
/// upstream's spelling, so the file also loads in any ini-based tool.
fn ini_text(p: &Profile) -> String {
    format!(
        "; Font-tuner custom profile (written by the tray's Custom dialog)\r\n\
         [General]\r\n\
         Name=Custom\r\n\
         HintingMode={}\r\n\
         AntiAliasMode={}\r\n\
         LcdFilter={}\r\n\
         GammaMode={}\r\n\
         GammaValue={:.2}\r\n\
         Contrast={:.2}\r\n\
         RenderWeight={:.2}\r\n\
         NormalWeight={}\r\n",
        p.hinting, p.aa.mode(), p.lcd_filter, p.gamma_mode, p.gamma, p.contrast, p.weight, p.embolden
    )
}

// ---- controls -------------------------------------------------------------

/// Slider rows: (min, max) in hundredths for the float knobs, whole units for
/// NormalWeight. The bounds keep the values in the range where the coverage
/// curve and gamma encoding still produce readable text.
const GAMMA_RANGE: (i32, i32) = (50, 300); // 0.50 .. 3.00
const CONTRAST_RANGE: (i32, i32) = (50, 250); // 0.50 .. 2.50
const WEIGHT_RANGE: (i32, i32) = (50, 250); // 0.50 .. 2.50
const EMBOLDEN_RANGE: (i32, i32) = (-16, 48); // 26.6 units; 64 = one pixel

/// Combo rows: the ini value each entry stands for.
const HINTING: [i32; 3] = [0, 1, 2];
const AA: [Aa; 5] = [Aa::Grey, Aa::LcdRgb, Aa::LcdBgr, Aa::LightLcdRgb, Aa::LightLcdBgr];
const LCD_FILTER: [i32; 4] = [0, 1, 2, 3];
const GAMMA_MODE: [i32; 4] = [0, 1, 2, -1];

const ID_HINTING: usize = 1001;
const ID_AA: usize = 1002;
const ID_LCD_FILTER: usize = 1003;
const ID_GAMMA_MODE: usize = 1004;
const ID_GAMMA: usize = 1005;
const ID_CONTRAST: usize = 1006;
const ID_WEIGHT: usize = 1007;
const ID_EMBOLDEN: usize = 1008;
const ID_FONT: usize = 1101;
const ID_COPY: usize = 1102;
const ID_APPLY: usize = 1103;
const ID_CLOSE: usize = 1104;

/// Trackbar message the `windows` crate does not export (commctrl.h: WM_USER + 0).
const TBM_GETPOS: u32 = 0x0400;
/// WM_COMMAND notification codes.
const CBN_SELCHANGE: usize = 1;
const BN_CLICKED: usize = 0;
/// The `'ttcf'` table tag: present only for TrueType collections.
const TTCF: u32 = u32::from_le_bytes(*b"ttcf");

/// Layout at 96 dpi; scaled by the window's dpi at creation.
const ROW_H: i32 = 30;
const LABEL_W: i32 = 150;
const CTL_X: i32 = 170;
const CTL_W: i32 = 250;
const VAL_X: i32 = 430;
const VAL_W: i32 = 60;
const MARGIN: i32 = 12;
const PREVIEW_W: i32 = 500;
const PREVIEW_H: i32 = 64;
const CLIENT_W: i32 = MARGIN * 2 + PREVIEW_W;

struct Dlg {
    hwnd: HWND,
    dpi: u32,
    font: HFONT,
    hinting: HWND,
    aa: HWND,
    lcd_filter: HWND,
    gamma_mode: HWND,
    gamma: (HWND, HWND),
    contrast: (HWND, HWND),
    weight: (HWND, HWND),
    embolden: (HWND, HWND),
    font_name: HWND,
    /// Top-left of the light and dark preview panels (client coordinates).
    preview: [(i32, i32); 2],
    profile: Profile,
    ft: Option<Ft>,
    lf: LOGFONTW,
    /// Em size in pixels for `lf`, taken from the DC's text metrics in
    /// `load_font`. `lfHeight` is the em size when negative but the *cell*
    /// height (em + internal leading) when positive, so its absolute value is
    /// not the em size. The injected core derives it the same way
    /// (`render-inject/src/gdi.rs::em_px`).
    em_px: i32,
    /// Install dir's font-tuner.ini and the profile the tray had selected,
    /// used by "copy current".
    ini: PathBuf,
    current: Option<PathBuf>,
    s: lang::Strings,
}

thread_local! {
    static DLG: RefCell<Option<Dlg>> = const { RefCell::new(None) };
}

fn with_dlg<R>(f: impl FnOnce(&mut Dlg) -> R) -> Option<R> {
    DLG.with(|d| d.borrow_mut().as_mut().map(f))
}

/// Open the dialog (or raise it if already open). `ini` is the install dir's
/// font-tuner.ini; `current` the selected profile's path, if any.
pub(crate) fn open(ini: PathBuf, current: Option<PathBuf>, s: &lang::Strings) {
    if let Some(hwnd) = with_dlg(|d| d.hwnd) {
        // SAFETY: our own live window.
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
        return;
    }
    let profile = ini_path()
        .filter(|p| p.exists())
        .or_else(|| current.clone())
        .and_then(|p| Profile::from_ini(&p.to_string_lossy()))
        .unwrap_or_else(Profile::clean_greyscale);

    // SAFETY: class/window creation with NUL-terminated strings that live to
    // the end of this function; every handle is ours.
    unsafe {
        let class = wide("font-tuner-custom");
        let hinst = HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0);
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hIcon: LoadIconW(Some(hinst), PCWSTR(core::ptr::without_provenance(1))).unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(core::ptr::without_provenance_mut(16)), // COLOR_BTNFACE + 1
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&raw const wc); // fails harmlessly once registered
        let title = wide(s.custom_title);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_APPWINDOW,
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
            80,
            80,
            10,
            10,
            None,
            None,
            Some(hinst),
            None,
        ) else {
            return;
        };
        let dpi = GetDpiForWindow(hwnd);
        let lf = message_font(dpi);
        let font = CreateFontIndirectW(&raw const lf);
        let mut d = Dlg {
            hwnd,
            dpi,
            font,
            hinting: HWND::default(),
            aa: HWND::default(),
            lcd_filter: HWND::default(),
            gamma_mode: HWND::default(),
            gamma: (HWND::default(), HWND::default()),
            contrast: (HWND::default(), HWND::default()),
            weight: (HWND::default(), HWND::default()),
            embolden: (HWND::default(), HWND::default()),
            font_name: HWND::default(),
            preview: [(0, 0); 2],
            profile,
            ft: Ft::new().ok(),
            lf,
            em_px: 12,
            ini,
            current,
            s: *s,
        };
        d.build(hinst);
        d.load_font();
        d.set_controls();
        DLG.with(|slot| *slot.borrow_mut() = Some(d));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// The system message font at `dpi` (the shell's UI font; also the default
/// preview font).
fn message_font(dpi: u32) -> LOGFONTW {
    let mut ncm = NONCLIENTMETRICSW { cbSize: struct_size::<NONCLIENTMETRICSW>(), ..Default::default() };
    // SAFETY: `ncm` is a NONCLIENTMETRICSW with cbSize set; the size matches.
    let ok = unsafe {
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, Some((&raw mut ncm).cast::<c_void>()), 0, dpi)
    };
    if ok.is_ok() {
        ncm.lfMessageFont
    } else {
        let mut lf = LOGFONTW { lfHeight: -12, ..Default::default() };
        for (dst, src) in lf.lfFaceName.iter_mut().zip("Yu Gothic UI".encode_utf16()) {
            *dst = src;
        }
        lf
    }
}

impl Dlg {
    fn px(&self, v: i32) -> i32 {
        i32::try_from(u64::from(self.dpi) * u64::from(v.unsigned_abs()) / 96).unwrap_or(v) * v.signum()
    }

    /// Create the controls. SAFETY: called once from `open` on the window just created.
    unsafe fn build(&mut self, hinst: HINSTANCE) {
        // SAFETY: every child call targets our live window.
        unsafe {
        let s = self.s;
        let rows: [(&str, usize); 8] = [
            (s.custom_hinting, ID_HINTING),
            (s.custom_aa, ID_AA),
            (s.custom_lcd_filter, ID_LCD_FILTER),
            (s.custom_gamma_mode, ID_GAMMA_MODE),
            (s.custom_gamma, ID_GAMMA),
            (s.custom_contrast, ID_CONTRAST),
            (s.custom_weight, ID_WEIGHT),
            (s.custom_embolden, ID_EMBOLDEN),
        ];
        let mut y = MARGIN;
        for (label, id) in rows {
            self.label(hinst, label, MARGIN, y + 4, LABEL_W);
            match id {
                ID_HINTING => self.hinting = self.combo(hinst, id, y, &[s.custom_hint_native, s.custom_hint_none, s.custom_hint_auto]),
                ID_AA => self.aa = self.combo(hinst, id, y, &[s.custom_aa_grey, "LCD (RGB)", "LCD (BGR)", "LightLCD (RGB)", "LightLCD (BGR)"]),
                ID_LCD_FILTER => self.lcd_filter = self.combo(hinst, id, y, &[s.custom_filter_none, s.custom_filter_default, s.custom_filter_light, s.custom_filter_legacy]),
                ID_GAMMA_MODE => self.gamma_mode = self.combo(hinst, id, y, &[s.custom_gm_power, "sRGB", s.custom_gm_avg, s.custom_gm_linear]),
                ID_GAMMA => self.gamma = self.slider(hinst, id, y, GAMMA_RANGE),
                ID_CONTRAST => self.contrast = self.slider(hinst, id, y, CONTRAST_RANGE),
                ID_WEIGHT => self.weight = self.slider(hinst, id, y, WEIGHT_RANGE),
                _ => self.embolden = self.slider(hinst, id, y, EMBOLDEN_RANGE),
            }
            y += ROW_H;
        }
        // Font row: current face name + picker button.
        self.label(hinst, s.custom_font, MARGIN, y + 4, LABEL_W);
        self.font_name = self.label(hinst, "", CTL_X, y + 4, CTL_W);
        self.button(hinst, s.custom_font_pick, ID_FONT, VAL_X, y, VAL_W + 10);
        y += ROW_H + 4;
        // Preview panels: light then dark, each PREVIEW_H tall at 1:1 pixels.
        self.preview[0] = (MARGIN, y);
        y += PREVIEW_H + 6;
        self.preview[1] = (MARGIN, y);
        y += PREVIEW_H + 10;
        // Buttons.
        let bw = 130;
        self.button(hinst, s.custom_copy, ID_COPY, MARGIN, y, bw + 30);
        self.button(hinst, s.custom_apply, ID_APPLY, CLIENT_W - MARGIN - bw * 2 - 8, y, bw);
        self.button(hinst, s.custom_close, ID_CLOSE, CLIENT_W - MARGIN - bw, y, bw);
        y += 30 + MARGIN;
        // Size the window to the client area.
        let mut r = RECT { left: 0, top: 0, right: self.px(CLIENT_W), bottom: self.px(y) };
        let _ = AdjustWindowRectExForDpi(&raw mut r, WS_CAPTION | WS_SYSMENU, false, WS_EX_APPWINDOW, self.dpi);
        let _ = SetWindowPos(self.hwnd, None, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER);
        }
    }

    /// A child control. SAFETY: `hinst` is our module; `class` a system class.
    #[allow(clippy::too_many_arguments)]
    unsafe fn child(&self, hinst: HINSTANCE, class: PCWSTR, text: &str, style: u32, id: usize, left: i32, top: i32, width: i32, height: i32) -> HWND {
        let t = wide(text);
        // SAFETY: `t` is NUL-terminated and outlives the call; the parent is
        // our live window; a control id travels in the HMENU slot by Win32
        // convention.
        unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(t.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
            self.px(left),
            self.px(top),
            self.px(width),
            self.px(height),
            Some(self.hwnd),
            Some(HMENU(core::ptr::without_provenance_mut(id))),
            Some(hinst),
            None,
        )
        .unwrap_or_default();
        SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
        hwnd
        }
    }

    unsafe fn label(&self, hinst: HINSTANCE, text: &str, x: i32, y: i32, w: i32) -> HWND {
        // SAFETY: as `child`. Style 0 = SS_LEFT.
        unsafe { self.child(hinst, w!("STATIC"), text, 0, 0, x, y, w, 20) }
    }

    unsafe fn button(&self, hinst: HINSTANCE, text: &str, id: usize, x: i32, y: i32, w: i32) -> HWND {
        // SAFETY: as `child`.
        unsafe { self.child(hinst, w!("BUTTON"), text, BS_PUSHBUTTON.unsigned_abs() | WS_TABSTOP.0, id, x, y, w, 26) }
    }

    unsafe fn combo(&self, hinst: HINSTANCE, id: usize, y: i32, items: &[&str]) -> HWND {
        // SAFETY: as `child`; each item string is NUL-terminated for its call.
        unsafe {
            let h = self.child(hinst, w!("COMBOBOX"), "", CBS_DROPDOWNLIST.unsigned_abs() | WS_TABSTOP.0, id, CTL_X, y, CTL_W, 200);
            for it in items {
                let t = wide(it);
                SendMessageW(h, CB_ADDSTRING, None, Some(LPARAM(t.as_ptr() as isize)));
            }
            h
        }
    }

    /// A trackbar plus a read-only edit showing its value.
    unsafe fn slider(&self, hinst: HINSTANCE, id: usize, y: i32, range: (i32, i32)) -> (HWND, HWND) {
        // SAFETY: as `child`.
        unsafe {
            let tb = self.child(hinst, w!("msctls_trackbar32"), "", TBS_HORZ | TBS_NOTICKS | WS_TABSTOP.0, id, CTL_X, y, CTL_W, 26);
            SendMessageW(tb, TBM_SETRANGEMIN, Some(WPARAM(0)), Some(LPARAM(isize::try_from(range.0).unwrap_or(0))));
            SendMessageW(tb, TBM_SETRANGEMAX, Some(WPARAM(0)), Some(LPARAM(isize::try_from(range.1).unwrap_or(0))));
            let ed = self.child(hinst, w!("EDIT"), "", ES_READONLY.unsigned_abs() | WS_BORDER.0, 0, VAL_X, y + 2, VAL_W, 22);
            (tb, ed)
        }
    }

    /// Push `self.profile` into the controls.
    fn set_controls(&self) {
        let p = &self.profile;
        let sel = |h: HWND, i: usize| {
            // SAFETY: `h` is a live combo of ours.
            unsafe { SendMessageW(h, CB_SETCURSEL, Some(WPARAM(i)), None) };
        };
        sel(self.hinting, HINTING.iter().position(|&v| v == p.hinting).unwrap_or(0));
        sel(self.aa, AA.iter().position(|&v| v == p.aa).unwrap_or(0));
        sel(self.lcd_filter, LCD_FILTER.iter().position(|&v| v == p.lcd_filter).unwrap_or(0));
        sel(self.gamma_mode, GAMMA_MODE.iter().position(|&v| v == p.gamma_mode).unwrap_or(0));
        let pos = |(tb, _): (HWND, HWND), v: i32, range: (i32, i32)| {
            // SAFETY: `tb` is a live trackbar of ours.
            unsafe { SendMessageW(tb, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(isize::try_from(v.clamp(range.0, range.1)).unwrap_or(0)))) };
        };
        pos(self.gamma, hundredths(p.gamma), GAMMA_RANGE);
        pos(self.contrast, hundredths(p.contrast), CONTRAST_RANGE);
        pos(self.weight, hundredths(p.weight), WEIGHT_RANGE);
        pos(self.embolden, p.embolden, EMBOLDEN_RANGE);
        self.show_values();
        self.repaint_preview();
    }

    /// Read the controls back into `self.profile` (called on every change).
    fn read_controls(&mut self) {
        // SAFETY: every handle is a live control of ours.
        unsafe {
            let cur = |h: HWND| usize::try_from(SendMessageW(h, CB_GETCURSEL, None, None).0).unwrap_or(0);
            let pos = |(tb, _): (HWND, HWND)| i32::try_from(SendMessageW(tb, TBM_GETPOS, None, None).0).unwrap_or(0);
            self.profile.hinting = HINTING[cur(self.hinting).min(HINTING.len() - 1)];
            self.profile.aa = AA[cur(self.aa).min(AA.len() - 1)];
            self.profile.lcd_filter = LCD_FILTER[cur(self.lcd_filter).min(LCD_FILTER.len() - 1)];
            self.profile.gamma_mode = GAMMA_MODE[cur(self.gamma_mode).min(GAMMA_MODE.len() - 1)];
            self.profile.gamma = f32_from_hundredths(pos(self.gamma));
            self.profile.contrast = f32_from_hundredths(pos(self.contrast));
            self.profile.weight = f32_from_hundredths(pos(self.weight));
            self.profile.embolden = pos(self.embolden);
        }
        self.show_values();
        self.repaint_preview();
    }

    fn show_values(&self) {
        let set = |(_, ed): (HWND, HWND), text: String| {
            let t = wide(&text);
            // SAFETY: `ed` is a live edit of ours; `t` is NUL-terminated.
            unsafe {
                let _ = SetWindowTextW(ed, PCWSTR(t.as_ptr()));
            }
        };
        set(self.gamma, format!("{:.2}", self.profile.gamma));
        set(self.contrast, format!("{:.2}", self.profile.contrast));
        set(self.weight, format!("{:.2}", self.profile.weight));
        set(self.embolden, format!("{}", self.profile.embolden));
        let name_len = self.lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(0);
        let face = String::from_utf16_lossy(&self.lf.lfFaceName[..name_len]);
        let t = wide(&format!("{face}  {}px", self.em_px));
        // SAFETY: as above.
        unsafe {
            let _ = SetWindowTextW(self.font_name, PCWSTR(t.as_ptr()));
        }
    }

    fn repaint_preview(&self) {
        let (x, y) = self.preview[0];
        let r = RECT {
            left: self.px(x),
            top: self.px(y),
            right: self.px(x) + self.px(PREVIEW_W),
            bottom: self.px(self.preview[1].1) + self.px(PREVIEW_H),
        };
        // SAFETY: our own window; `r` is a RECT.
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), Some(&raw const r), false);
        }
    }

    /// Load `self.lf` into FreeType via GDI's font bytes (as the injected core
    /// does per DC). On failure the face is left unset and the preview is empty.
    fn load_font(&mut self) {
        let Some(ft) = self.ft.as_ref() else { return };
        // SAFETY: GDI objects created and released here; `lf` is ours.
        unsafe {
            let hfont = CreateFontIndirectW(&raw const self.lf);
            let hdc = CreateCompatibleDC(None);
            let old = SelectObject(hdc, HGDIOBJ(hfont.0));
            // Em size, as the injected core computes it for a DC.
            let mut tm = TEXTMETRICW::default();
            if GetTextMetricsW(hdc, &raw mut tm).as_bool() {
                let em = tm.tmHeight - tm.tmInternalLeading;
                self.em_px = if em > 0 { em } else { 12 };
            }
            let (table, size) = match GetFontData(hdc, TTCF, 0, None, 0) {
                0 | u32::MAX => (0, GetFontData(hdc, 0, 0, None, 0)),
                n => (TTCF, n),
            };
            if size != 0 && size != u32::MAX {
                let mut buf = vec![0u8; size as usize];
                GetFontData(hdc, table, 0, Some(buf.as_mut_ptr().cast::<c_void>()), size);
                let name_len = self.lf.lfFaceName.iter().position(|&c| c == 0).unwrap_or(0);
                let face = String::from_utf16_lossy(&self.lf.lfFaceName[..name_len]);
                let _ = ft.reface_memory(&buf, &face);
            }
            SelectObject(hdc, old);
            let _ = DeleteDC(hdc);
            let _ = DeleteObject(HGDIOBJ(hfont.0));
        }
    }

    fn choose_font(&mut self) {
        let mut lf = self.lf;
        let mut cf = CHOOSEFONTW {
            lStructSize: struct_size::<CHOOSEFONTW>(),
            hwndOwner: self.hwnd,
            lpLogFont: &raw mut lf,
            Flags: CF_SCREENFONTS | CF_INITTOLOGFONTSTRUCT | CF_NOVERTFONTS,
            ..Default::default()
        };
        // SAFETY: `cf` points at `lf`, which outlives the modal call.
        if unsafe { ChooseFontW(&raw mut cf) }.as_bool() {
            self.lf = lf;
            self.load_font();
            self.show_values();
            self.repaint_preview();
        } else {
            // SAFETY: no arguments; reports why the last common dialog failed
            // (0 = the user cancelled).
            let err = unsafe { CommDlgExtendedError() };
            if err.0 != 0 {
                crate::msgbox(&format!("ChooseFont: CommDlgExtendedError {:#x}", err.0));
            }
        }
    }

    /// Draw both preview panels: black text on white, light text on dark.
    fn paint(&self) {
        let mut ps = PAINTSTRUCT::default();
        // SAFETY: BeginPaint/EndPaint pair on our window inside WM_PAINT; the
        // DIB bits and header outlive the SetDIBitsToDevice call.
        unsafe {
            let hdc = BeginPaint(self.hwnd, &raw mut ps);
            let (width, height) = (self.px(PREVIEW_W), self.px(PREVIEW_H));
            let panels: [([u8; 3], Ink); 2] = [([255, 255, 255], Ink::default()), ([30, 30, 30], Ink { fg: [222, 222, 222] })];
            for (i, (bg, ink)) in panels.into_iter().enumerate() {
                let canvas = self.render(bg, ink, width, height);
                let mut bgra = vec![0u8; canvas.w * canvas.h * 4];
                canvas.blit_to_bgra_topdown(&mut bgra);
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: struct_size::<BITMAPINFOHEADER>(),
                        biWidth: width,
                        biHeight: -height,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let (left, top) = (self.px(self.preview[i].0), self.px(self.preview[i].1));
                SetDIBitsToDevice(hdc, left, top, width.unsigned_abs(), height.unsigned_abs(), 0, 0, 0, height.unsigned_abs(), bgra.as_ptr().cast::<c_void>(), &raw const bmi, DIB_RGB_COLORS);
                let frame = RECT { left: left - 1, top: top - 1, right: left + width + 1, bottom: top + height + 1 };
                FrameRect(hdc, &raw const frame, HBRUSH(GetStockObject(GRAY_BRUSH).0));
            }
            let _ = EndPaint(self.hwnd, &raw const ps);
        }
    }

    /// One panel: the sample at the chosen size and again 1.5× larger.
    fn render(&self, bg: [u8; 3], ink: Ink, width: i32, height: i32) -> Canvas {
        let size = (usize::try_from(width).unwrap_or(1), usize::try_from(height).unwrap_or(1));
        let mut canvas = Canvas::filled(size.0, size.1, bg);
        let Some(ft) = self.ft.as_ref() else { return canvas };
        let p = &self.profile;
        let tables = tables_for(p);
        let px = self.em_px.max(6);
        let big = px * 3 / 2;
        let pad = self.px(6);
        render_core::render::draw_text_onto(&mut canvas, ft, &tables, p, ink, self.s.custom_sample, px, (pad, pad + px), None);
        // The larger line only when it fits (descender ≈ a quarter of the size).
        let base2 = pad + px + self.px(6) + big;
        if base2 + big / 4 <= height {
            render_core::render::draw_text_onto(&mut canvas, ft, &tables, p, ink, self.s.custom_sample, big, (pad, base2), None);
        }
        canvas
    }

    /// Write Custom.ini, select it, and ask running processes to reload.
    fn apply(&self) -> Result<(), String> {
        let path = ini_path().ok_or_else(|| self.s.custom_err_appdata.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, ini_text(&self.profile)).map_err(|e| format!("{}: {e}", path.display()))?;
        if !select(&self.ini) {
            return Err(format!("{}: {}", self.ini.display(), self.s.custom_err_select));
        }
        // SAFETY: a static message name; broadcasting a registered message.
        // Then invalidate every window so the change shows without waiting for
        // apps to repaint on their own.
        unsafe {
            let id = RegisterWindowMessageW(w!("FontTuner.ReloadProfile"));
            let _ = PostMessageW(Some(HWND_BROADCAST), id, WPARAM(0), LPARAM(0));
            let _ = InvalidateRect(None, None, true);
        }
        Ok(())
    }

    /// Reset the controls from the profile the tray currently has selected.
    fn copy_current(&mut self) {
        if let Some(p) = self.current.as_ref().and_then(|p| Profile::from_ini(&p.to_string_lossy())) {
            self.profile = p;
            self.set_controls();
        }
    }
}

fn hundredths(v: f32) -> i32 {
    // Bounded by the slider ranges, so the float→int cast cannot overflow.
    #[allow(clippy::cast_possible_truncation)]
    let n = (v * 100.0).round() as i32;
    n
}

fn f32_from_hundredths(n: i32) -> f32 {
    // Slider positions are three-digit integers; exactly representable.
    #[allow(clippy::cast_precision_loss)]
    let f = n as f32 / 100.0;
    f
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: a window procedure on our own window. `with_dlg` borrows are
    // released before anything modal (ChooseFont, MessageBox) re-enters it.
    unsafe {
        match msg {
            // While ChooseFont is up the state is checked out (see ID_FONT),
            // so let DefWindowProc validate the window instead of leaving the
            // paint pending forever.
            WM_PAINT => match with_dlg(|d| d.paint()) {
                Some(()) => LRESULT(0),
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            },
            WM_HSCROLL => {
                with_dlg(Dlg::read_controls);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                let code = (wparam.0 >> 16) & 0xFFFF;
                match id {
                    ID_HINTING | ID_AA | ID_LCD_FILTER | ID_GAMMA_MODE if code == CBN_SELCHANGE => {
                        with_dlg(Dlg::read_controls);
                    }
                    ID_FONT if code == BN_CLICKED => {
                        // ChooseFont is modal: take the state out so the
                        // re-entrant WM_PAINTs see no live borrow.
                        let d = DLG.with(|d| d.borrow_mut().take());
                        if let Some(mut d) = d {
                            d.choose_font();
                            DLG.with(|slot| *slot.borrow_mut() = Some(d));
                        }
                    }
                    ID_COPY if code == BN_CLICKED => {
                        with_dlg(Dlg::copy_current);
                    }
                    ID_APPLY if code == BN_CLICKED => {
                        if let Some(Err(e)) = with_dlg(|d| d.apply()) {
                            crate::msgbox(&e);
                        }
                    }
                    ID_CLOSE if code == BN_CLICKED => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(d) = DLG.with(|d| d.borrow_mut().take()) {
                    let _ = DeleteObject(HGDIOBJ(d.font.0));
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_round_trips_through_the_core_parser() {
        let mut p = Profile::accurate();
        p.gamma_mode = 2;
        p.embolden = 8;
        p.contrast = 1.35;
        let q = Profile::from_ini_str(&ini_text(&p));
        assert_eq!(q.hinting, p.hinting);
        assert_eq!(q.aa, p.aa);
        assert_eq!(q.lcd_filter, p.lcd_filter);
        assert_eq!(q.gamma_mode, 2);
        assert_eq!(q.embolden, 8);
        assert!((q.gamma - p.gamma).abs() < 1e-6);
        assert!((q.contrast - 1.35).abs() < 1e-6);
        assert!((q.weight - p.weight).abs() < 1e-6);
    }

    #[test]
    fn slider_units_round_trip() {
        for n in [GAMMA_RANGE.0, 100, 125, GAMMA_RANGE.1] {
            assert_eq!(hundredths(f32_from_hundredths(n)), n);
        }
    }
}
