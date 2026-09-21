# Rust reimplementation of the MacType pipeline

A from-scratch Rust reimplementation of what MacType does: intercept text
drawing in every process and render it with FreeType + custom gamma/LCD tuning.
This **is** the shipped core: the `font-tuner` MSI installs `RenderCore64.dll`
(this crate), not the C++ MacType core.

FreeType itself is reused unchanged (the fork's `freetype64.lib`); everything
else — the tuning maths, the hooking, and the FreeType glue — is Rust. The only
native code linked into the shipped DLL is that FreeType static lib.

## Crates

| crate | kind | role |
|---|---|---|
| `render-core` | lib | the rendering engine: gamma/contrast/LCD LUTs + blend (ported from ft.cpp, **bit-exact verified**), direct FreeType FFI (no C shim), `Profile` (incl. `from_ini`), glyph/string compositing |
| `render-inject` | cdylib `RenderCore64.dll` | injected into each process; hooks GDI + DirectWrite/Direct2D text and renders with render-core; exports `GetMsgProc` for auto-injection; self-pins so it is never unmapped from a running process |
| `loader` | bin | install a WH_GETMESSAGE hook backed by RenderCore64.dll, scoped to one process — the test harness for trying the core in a single app |

## Pipeline (as built)

```
the tray's global (or loader's single-process) WH_GETMESSAGE hook maps RenderCore64.dll
  → DllMain self-pins (GetModuleHandleEx FLAG_PIN) and spawns a thread
    (off the loader lock) that, once per process (named-mutex guard):
      loads the active profile (install-dir font-tuner.ini AlternativeFile, else default)
      hooks gdi32!ExtTextOutW               (retour inline detour, other threads frozen)
      patches IDWriteBitmapRenderTarget::DrawGlyphRun in the shared vtable
      hooks IDWriteFactory::CreateGlyphRunAnalysis / D2D1CreateFactory
  → each text draw:
      resolve the font from the DC / glyph run (GetFontData 'ttcf' for TTCs,
        or IDWriteFontFace file bytes + index)
      render the run with render-core (grey/LCD per profile) over the DC's pixels
      blit back, skipping the OS rasteriser
  → never unloaded from a running process (pinned); DllMain DETACH is a no-op
    reached only at process teardown
```

Rendering is serialised by a mutex (one shared FreeType face, re-faced per draw).
Hooking uses **retour** (pure Rust; iced-x86 disassembler), not MinHook/Detours;
because retour does not stop threads while patching, `install_hook` freezes the
other threads around the byte patch. The one-time vtable patches are serialised
by a mutex so two threads cannot both capture the "original" and recurse.

## What works

- **render-core**: greyscale + LCD, gamma modes, weight/embolden — verified
  **bit-for-bit** against the C++ original (`render-core/verify/`).
- **GDI** text replaced under injection (string + `ETO_GLYPH_INDEX`), with the
  DC's font/colour/baseline, over the existing content.
- **DirectWrite** (`IDWriteBitmapRenderTarget::DrawGlyphRun`) replaced under
  injection via the shared-vtable patch.
- **Auto-injection** via a WH_GETMESSAGE hook (the upstream mechanism); the
  `loader` bin scopes it to one process for testing, the tray installs it
  globally. The DLL self-pins, so it is never unmapped from a running process.
- **Profiles** driven by font-tuner's own `.ini` files (`Profile::from_ini`),
  read once at attach from the install dir's `font-tuner.ini` (`AlternativeFile`).
  A profile switch takes effect for processes started afterwards; the tray's
  "Reload profile" broadcasts a registered message that makes already-injected
  processes re-read the ini on their own UI thread (no watcher thread).
- **GDI fidelity**: `ETO_OPAQUE` / `ETO_CLIPPED` / `lpDx` honoured in
  `render-inject`.
- **Performance**: the font file is extracted + re-faced only when the font
  actually changes (cached), not per draw.
- **Tests**: `render-core` has `cargo test` units for the blend (a regression
  guard against the C++ oracle values) and `Profile::from_ini`, plus the
  bit-exact golden harness in `verify/`.

## Port scope = MacType's full hook coverage

This is a **port**: the target is everything the C++ MacType intercepts
(`vendor/mactype/hooklist.h`, `directwrite.cpp`), not a narrowed subset. Text
paths MacType hooks, and where we stand:

| path | MacType hooks | ours |
|---|---|---|
| GDI `ExtTextOutW` | yes | **done** |
| GDI `ExtTextOutA` / `TextOutW/A` / `GetGlyphOutline*` | yes | todo (most apps hit ExtTextOutW) |
| DirectWrite `IDWriteBitmapRenderTarget::DrawGlyphRun` (vtbl 3) | yes | **done** |
| DirectWrite `CreateGlyphRunAnalysis` → `CreateAlphaTexture` (Chromium/Skia, VS Code) | yes | **done** |
| Direct2D `ID2D1RenderTarget::DrawGlyphRun` (vtbl 29) | yes | **done** (via `D2D1CreateFactory` → RT creation → per-vtable patch) |
| Direct2D `DrawGlyphRun1` (vtbl 82) / `ID2D1DeviceContext` | yes | todo |
| factory/device hooks to reach the above (`D2D1CreateFactory`, `D2D1CreateDevice(Context)`, `DWriteCreateFactory`, `GetGdiInterop`) | yes | partial (`D2D1CreateFactory` hooked; DWrite paths patch the shared vtable directly) |

Do not treat any path MacType covers as out of scope: the remaining rows are
not-yet-ported, not deliberately dropped.

## Other remaining

- DPI transforms and non-natural DirectWrite measuring modes.
- Coloured LCD is only exercised for black/greyscale text in verification.
- `static mut` state (set once at init) should move to proper sync types; the
  one-time patches that race are already mutex-guarded.

## Build & try (single process)

Requires `build/lib/freetype64.lib` first (run `build-core.ps1` once). Then, per
crate: `cargo build --release`. Quick demos:

```powershell
# offline render gallery (no hooking)
cargo run --release --manifest-path render-core/Cargo.toml

# inject into ONE running app (here charmap) for 8 seconds, then unhook
loader\target\release\loader.exe <abs path>\RenderCore64.dll charmap.exe 8
```

Test with the tray stopped: a running tray injects the *installed* core into
every process, so two builds would fight over the same hooks.

The injected DLL logs to `%TEMP%\render-inject.log` and saves one capture PNG.
The per-stage probe/window crates used to develop each path were removed once
the work landed in `render-inject`; see git history if you need them.
