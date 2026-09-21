# Font-tuner specification

A small, self-contained font-rendering tuner for Windows (tray + injected render core; upstream: https://github.com/snowie2000/mactype).
It reproduces what the upstream closed-source tray does in "tray mode", ships the
render core DLL built from source, bundles a set of rendering profiles, and
adds a tray-menu system-font switcher. Windows 11, 64-bit only.

---

## 1. Architecture

### 1.1 Injection flow

```
font-tuner.exe ──(SetWindowsHookExW WH_GETMESSAGE, global)──▶ every 64-bit GUI process
                                                          maps RenderCore64.dll
                                                          (hook proc lives there)
                          core DllMain hooks the font APIs (GDI / DirectWrite / Direct2D)
                          │
                          └─ on child-process spawn, core injects RenderBootstrap64.dll
                             (GdippInjectDLL), which LoadLibraryW's the core in the child
```

* **font-tuner.exe** — the tray process. Installs one global `WH_GETMESSAGE` hook
  whose procedure is exported by `RenderCore64.dll`. When any 64-bit GUI
  process pulls a message, Windows maps the core DLL into it and the hook fires,
  so the core's `DllMain` runs and patches the font-rendering APIs.
* **RenderCore64.dll** — the render engine (Rust port of the upstream core). Does the
  actual glyph rendering via the bundled FreeType fork. On attach it marks
  itself non-unloadable for the life of the process
  (`GetModuleHandleEx` + `GET_MODULE_HANDLE_EX_FLAG_PIN`): removing the hook
  (tray off / exit / upgrade / uninstall) stops injection into *new* processes
  but never unmaps the DLL from running ones, so there is no "code executes
  after unmap" crash path. Profile switch, on/off and upgrade all take effect
  for processes started afterwards.
* **Fixed hook-procedure RVA** — `SetWindowsHookEx` stores only
  `GetMsgProc - hmod`; in a target process Windows resolves the DLL by path,
  finds the (self-pinned) image that process already holds, and calls
  `that_base + RVA`. After an upgrade the running processes still hold the
  *previous* build, so the RVA must be identical across builds or every one
  of them executes random bytes on its next message and dies at once (this
  happened once: a refactor moved `GetMsgProc` from `0x1100` to `0x8300`).
  The core's `build.rs` pins `GetMsgProc` to RVA `0x1000` (first byte of
  `.text`) with the linker's `/ORDER`; `build-msi.ps1` refuses to package a
  core where it moved, and the tray refuses to hook one (see 1.2).
* **RenderBootstrap64.dll** — a tiny Rust replacement (`bootstrap/`, crate
  `render-bootstrap`, output name `RenderBootstrap64`) for the closed-source bootstrap.
  The core injects it into freshly spawned child processes; its only job is to
  `LoadLibraryW` the core from a background thread. It uses `CreateThread` from
  `DllMain` (never `LoadLibrary` under loader lock) to stay deadlock-free.

### 1.2 Hooking & concurrency

* **Mechanism** — GDI `ExtTextOutW` is hooked with an inline detour via
  **retour** (pure Rust; the iced-x86 disassembler). DirectWrite/Direct2D entry
  points are patched directly in their COM vtables. No MinHook/Detours.
* **Thread-safe patching** — retour does not stop other threads while it
  rewrites the target's first bytes, so `install_hook` freezes every other
  thread in the process (`CreateToolhelp32Snapshot` + `SuspendThread`) around
  the patch and resumes them after — the window MinHook closes internally.
* **Attach-once** — a WH_GETMESSAGE map plus another load can make the DLL two
  module instances in one process; a per-process named mutex
  (`Local\FontTuner.Attached.<pid>`) ensures only the first attach hooks, so a
  second attach cannot detour over our own jump and corrupt the trampoline.
* **One-time vtable patches** (CreateAlphaTexture, every Direct2D creation and
  text slot) are serialised by a mutex and re-checked under it; otherwise two
  racing threads both capture the "original" from an already-patched slot and
  the detour calls itself → infinite recursion. Direct2D slots are keyed by
  (vtable, slot) in one map, since each render-target class has its own vtable.
* **Direct2D reach** — `render-inject/src/d2d.rs` walks upstream's creation
  chain: `D2D1CreateFactory` → `CreateHwnd/DC/WicBitmapRenderTarget` and
  `ID2D1Factory1..7::CreateDevice`; `D2D1CreateDevice` →
  `ID2D1Device..6::CreateDeviceContext`; `D2D1CreateDeviceContext`. On every
  target it patches `DrawGlyphRun` (29), the description overload (82),
  `SetTextAntialiasMode` (34) and `SetTextRenderingParams` (36). Where the
  target lends a GDI DC the run is drawn by render-core; where it does not
  (DXGI surfaces: swap chains, composition) the OS draws with the profile's
  `[DirectWrite]` `IDWriteRenderingParams`, the antialias mode derived from
  `AntiAliasMode`, and the 1/65535 transform nudge upstream applies when
  `HintingMode=1`. Slot numbers were checked against the `windows` crate's
  vtable definitions.
* **Re-entrancy** is guarded per-thread (`thread_local`), so one thread
  rendering never forces another thread's draw down the untuned GDI path.
* **Hook-install guards in the tray** (`Hook::install`, `src/stale.rs`) —
  before even `LoadLibraryW` (loading the core would pin it in the tray and
  hook the tray itself) the tray checks (a) from the file on disk that the
  core has `GetMsgProc` at RVA `0x1000`, and (b) that no running process holds the
  core *from the same path* with `GetMsgProc` anywhere else (Toolhelp module
  walk + `ReadProcessMemory` of that image's export table; a process that has
  the module but whose image cannot be read counts as stale, unless it has
  exited meanwhile). A copy loaded
  from another directory (the `loader` harness) is not a problem: Windows
  resolves the hook DLL by path and maps the installed one as a separate
  image, and the attach-once mutex keeps the second one inert. Either failure
  shows an error and leaves the hook off; for (b) the message lists the
  programs and tells the user to sign out and back in (or reboot) — that is
  the only way the stale images go away, because the core is self-pinned.
  Processes the tray cannot open (other users, higher integrity) are not
  reached by its hook either, so skipping them is safe.

### 1.3 What it cannot reach

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
`font-tuner.ini` `[General] AlternativeFile=ini\<name>.ini`; it applies to
newly created processes. The tray "profile" submenu writes this key when you
pick an entry (via `WritePrivateProfileString`).

The injected core reads the key **once at attach**, so a switch shows up in
processes started afterwards. To update already-running processes, the tray's
**"Reload profile"** item broadcasts a registered window message
(`FontTuner.ReloadProfile`); each injected core sees it in its `GetMsgProc`
(already on that process's UI thread) and re-reads `font-tuner.ini` under the
render lock — no watcher thread, nothing that can run after the DLL is gone.
The tray's **"Version"** item opens an About box with the version, licence
(GPL-3.0-only), source URL, and the required FreeType credit.

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

The `[DirectWrite]` section (`GammaValue`, `Contrast`, `ClearTypeLevel`,
`RenderingMode`) is what Direct2D is told to use for text we cannot rasterise
ourselves (1.2). Defaults follow upstream: gamma derived from the general one
(`g² > 1.3 ? g²/2 : 0.7`), contrast 1.0, ClearType level 1.0, mode 5.

`[Experimental] ClipBoxFix` (default 1) pads the metrics `GetGlyphOutline`
reports for a metrics-only query — origin up by `floor(1.5·DPI/96)` px, black
box grown the same, both capped to the font's ascent/height — so apps that
clip glyphs to those metrics (Java2D) do not cut off the heavier rendered
glyphs. Per-process sections such as `[Experimental@idea64.exe]` are read
by upstream only; the core has no per-process settings.

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
* **`build-core.ps1`** — builds only the one native dependency the shipped DLL
  needs: the snowie2000 FreeType fork (`freetype64.lib`) via MSBuild/vswhere.
  The C++ MacType core, Detours and IniParser are no longer built — the render
  core is Rust (`RenderCore64.dll`) and hooking uses `retour`.
* **`build-msi.ps1`** — runs `build-core.ps1`, `cargo build --release`
  (workspace + `render-inject`), checks with `check-export-rva.ps1` that the
  core exports `GetMsgProc` at RVA `0x1000` (aborts otherwise — see 1.1),
  stages exe + DLLs + `font-tuner.ini` + `ini\*.ini` into `build\pkg`, then
  `wix build` →
  `dist\font-tuner-<ver>-x64.msi`.

---

## 6. Installer (MSI, WiX v6)

* **Scope** perMachine, installs to `C:\Program Files\Font-tuner`; adds a Start
  menu shortcut so the tray can be relaunched after "Exit".
* **Run at logon** — writes `HKLM\...\CurrentVersion\Run\Font-tuner`.
* **On install** — stops a running `font-tuner.exe`, then launches
  Font-tuner.
* **Restart Manager disabled** (`MSIRESTARTMANAGERCONTROL=Disable`,
  `REBOOT=ReallySuppress`): `RenderCore64.dll` is mapped into every GUI process,
  so the Restart Manager would otherwise close them all (it has killed the
  user's shell). Only the tray is closed.
* **In-use core swap** — because the core is mapped (and self-pinned) in every
  running process, its file is never free to overwrite. A deferred custom
  action (`RenameOldCore`, scheduled right after `InstallInitialize`, before
  `RemoveExistingProducts`) renames the in-use `RenderCore64.dll` aside so
  `InstallFiles` can place the new one immediately; the freshly launched tray
  then hooks with the new core. Renamed-aside copies are queued for deletion at
  next reboot (`MoveFileEx DELAY_UNTIL_REBOOT`). No reboot is needed for the
  upgrade to take effect on newly started processes.
  The renamed-aside image stays mapped in every running process under its
  original path, which is why `GetMsgProc` must keep the same RVA in the new
  core (1.1); if the tray finds a running process whose core disagrees, it
  does not hook and asks for a sign-out (1.2).
* **`font-tuner.ini`** is marked `NeverOverwrite` — a user's selected profile
  (the `AlternativeFile` value) survives upgrades.
* **Uninstall** — standard Add/Remove Programs entry, or
  `msiexec /x {ProductCode}`. Removes files, the Run registry value, and stops
  the process.
* **Signing** — the MSI and its payload are **unsigned**, so install shows a UAC
  "unknown publisher" prompt (and possibly SmartScreen). It is not blocked.
  Signing is intentionally omitted to keep the release unattributable; it would
  not help reach the browser renderer/GPU processes anyway (see §1.3).

---

## 7. Licensing

GPL-3.0-only. Bundles a FreeType fork from `vendor/`
under their respective licenses. Tray icon art is CC0.
