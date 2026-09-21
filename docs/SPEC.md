# Font-tuner specification

A small, self-contained tray loader for [MacType](https://github.com/snowie2000/mactype).
It reproduces what the closed-source *MacTray* does in "tray mode", ships the
MacType core DLL built from source, bundles a set of rendering profiles, and
adds a tray-menu system-font switcher. Windows 11, 64-bit only.

---

## 1. Architecture

### 1.1 Injection flow

```
font-tuner.exe ──(SetWindowsHookExW WH_GETMESSAGE, global)──▶ every 64-bit GUI process
                                                          maps MacType64.Core.dll
                                                          (hook proc lives there)
                          core DllMain hooks the font APIs (GDI / DirectWrite)
                          │
                          └─ on child-process spawn, core injects MTBootStrap64.dll
                             (GdippInjectDLL), which LoadLibraryW's the core in the child
```

* **font-tuner.exe** — the tray process. Installs one global `WH_GETMESSAGE` hook
  whose procedure is exported by `MacType64.Core.dll`. When any 64-bit GUI
  process pulls a message, Windows maps the core DLL into it and the hook fires,
  so the core's `DllMain` runs and patches the font-rendering APIs.
* **MacType64.Core.dll** — the MacType engine (built from `vendor/`). Does the
  actual glyph rendering via the bundled FreeType fork.
* **MTBootStrap64.dll** — a tiny Rust replacement (`bootstrap/`, crate
  `mtbootstrap`, output name `MTBootStrap64`) for the closed-source bootstrap.
  The core injects it into freshly spawned child processes; its only job is to
  `LoadLibraryW` the core from a background thread. It uses `CreateThread` from
  `DllMain` (never `LoadLibrary` under loader lock) to stay deadlock-free.

### 1.2 What it cannot reach

* **Chrome/Edge renderer & GPU processes** — blocked by
  `MITIGATION_FORCE_MS_SIGNED_BINS` (Microsoft-signed binaries only). The
  unsigned core/bootstrap DLLs are rejected at `LoadLibrary`. Ordinary child
  processes (crashpad-handler, utility) are reached. This is a Windows security
  boundary, not a bug; the only workarounds are Microsoft signing (not
  obtainable for arbitrary DLLs) or disabling the mitigation per browser
  (`RendererCodeIntegrityEnabled=0` policy — weakens the sandbox, not enabled by
  this project).
* **Processes without a message pump** (console apps, services) — `WH_GETMESSAGE`
  never fires there.
* **32-bit processes** — out of scope; this is a 64-bit-only build.
* **Higher-integrity processes** — UIPI blocks hook messages from a
  lower-integrity Font-tuner.

---

## 2. Rendering profiles

Profiles live in `profiles/ini/*.ini`. The active one is chosen by
`MacType.ini` `[General] AlternativeFile=ini\<name>.ini`; it applies to
newly created processes. The tray "profile" submenu writes this key when you
pick an entry (via `WritePrivateProfileString`).

### 2.1 The five profiles

| Profile | Hinting | Anti-alias | Character |
|---|---|---|---|
| **Clean Greyscale** *(default)* | 0 (none) | 0 greyscale | Neutral, soft, no colour fringing. The shipped default. |
| **Clean Dark Greyscale** | 0 (none) | 0 greyscale | Greyscale tuned for dark backgrounds (lower gamma 1.1, contrast 0.9, slightly heavier weight). |
| **Accurate** | 2 (TrueType bytecode) | 4 LightLCD | Strongest grid-fitting via the font's own hint instructions. Crispest at small/UI sizes; shapes are most pixel-aligned. |
| **Clean Sharp** | 1 (FreeType light) | 2 LCD | Subpixel (colour) LCD, moderate hinting. Sharp with high horizontal detail. (Formerly "Clean".) |
| **Clean Sharp Dark** | 0 (none) | 2 LCD | LCD subpixel tuned for dark backgrounds. (Formerly "Clean Dark".) |

Hinting modes: **0** = none (outline as-is, softest, most faithful shape);
**1** = FreeType light auto-hint (vertical stems grid-fit, balanced);
**2** = TrueType bytecode (font's own hints, strongest, crispest at small sizes,
can distort shape slightly). All profiles use DirectWrite `RenderingMode=2`
(GDI_CLASSIC) so GDI and DirectWrite text match.

### 2.2 Menu order

The tray lists profiles in a fixed preferred order (`ORDER` in `src/main.rs`),
not alphabetically: greyscale pair → Accurate → the LCD "Clean Sharp" series.
Any profile not in the list falls in afterwards, alphabetically, so adding an
`.ini` still shows it without a code change.

```
1. Clean Greyscale
2. Clean Dark Greyscale
3. Accurate
4. Clean Sharp
5. Clean Sharp Dark
```

---

## 3. System-font switcher

Tray submenu "システムフォント / System font" (`src/sysfont.rs`). Swaps the
shell UI fonts (caption, small-caption, menu, status, message, icon-title) via
`SystemParametersInfo` (`SPI_SET{NONCLIENTMETRICS,ICONTITLELOGFONT}`), which is a
persistent per-user setting broadcast with `WM_SETTINGCHANGE`.

* Fixed candidate list: `BIZ UDPゴシック`, `BIZ UDゴシック`, `Noto Sans JP`,
  `メイリオ`.
* On the first change the original font set is saved to
  `%LOCALAPPDATA%\font-tuner\sysfont-backup.bin`. "既定に戻す / Default (restore)"
  restores it and deletes the backup.
* Hardening: on restore the `cbSize` from the (user-writable) backup file is
  **not** trusted — it is forced to the real struct size so
  `SystemParametersInfo` can never read past the buffer.

Fully applies to newly drawn UI immediately; the shell picks it up completely
after a sign-out/in.

---

## 4. Tray icon

Two icons are embedded in the exe via `app.rc` / `build.rs` (`embed-resource`):

* `assets/tray-dark.ico` — **silver** metallic gear + "A", for a **dark** taskbar.
* `assets/tray-light.ico` — **black** metallic gear + "A", for a **light** taskbar.

At startup and on every `WM_SETTINGCHANGE`, Font-tuner reads
`HKCU\...\Themes\Personalize\SystemUsesLightTheme` and picks the matching icon,
swapping live when the theme changes. Missing value ⇒ dark (Windows 11 default).
Icon art is CC0 (public-domain gear) with a rendered letter "A".

---

## 5. Build

* **Toolflags** — `.cargo/config.toml` sets `+crt-static` for the MSVC target,
  removing the `VCRUNTIME140.dll` dependency (and its DLL-search-order hijack
  surface). Release profile: `opt-level="s"`, LTO, `panic="abort"`, stripped.
* **`build-core.ps1`** — builds `MacType64.Core.dll` from `vendor/` (IniParser,
  the snowie2000 FreeType fork, Detours) via MSBuild/vswhere.
* **`build-msi.ps1`** — builds the core, `cargo build --release --workspace`,
  stages exe + DLLs + `MacType.ini` + `ini\*.ini` into `build\pkg`, then
  `wix build` → `dist\font-tuner-<ver>-x64.msi`.

---

## 6. Installer (MSI, WiX v6)

* **Scope** perMachine, installs to `C:\Program Files\Font-tuner`.
* **Run at logon** — writes `HKLM\...\CurrentVersion\Run\Font-tuner`.
* **On install** — stops a running `MacTray.exe` and `font-tuner.exe`, then launches
  Font-tuner.
* **`MacType.ini`** is marked `NeverOverwrite` — a user's selected profile
  (the `AlternativeFile` value) survives upgrades.
* **Uninstall** — standard Add/Remove Programs entry, or
  `msiexec /x {ProductCode}`. Removes files, the Run registry value, and stops
  the process.
* **Signing** — the MSI and its payload are **unsigned**, so install shows a UAC
  "unknown publisher" prompt (and possibly SmartScreen). It is not blocked.
  Signing is intentionally omitted to keep the release unattributable; it would
  not help reach the browser renderer/GPU processes anyway (see §1.2).

---

## 7. Licensing

GPL-3.0-or-later. Bundles the MacType core and a FreeType fork from `vendor/`
under their respective licenses. Tray icon art is CC0.
