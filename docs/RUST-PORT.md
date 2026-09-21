# Rust reimplementation of the MacType pipeline

An experimental, from-scratch Rust reimplementation of what MacType does:
intercept text drawing in every process and render it with FreeType + custom
gamma/LCD tuning. Lives on the `develop` branch and its `feature/*` branches;
**not** part of the shipped `font-tuner` MSI (which uses the C++ MacType core).

FreeType itself is reused unchanged (the fork's `freetype64.lib`); only
MacType's *tuning + hooking* layers are rewritten in Rust.

## Crates

| crate | kind | role |
|---|---|---|
| `render-core` | lib | the rendering engine: gamma/contrast/LCD LUTs + blend (ported from ft.cpp, **bit-exact verified**), FreeType shim, `Profile` (incl. `from_ini`), glyph/string compositing |
| `render-inject` | cdylib `RenderCore64.dll` | injected into each process; hooks GDI + DirectWrite text and renders with render-core; exports `GetMsgProc` for auto-injection; cleans up on unload |
| `injector` | bin | inject a DLL into a process by name (CreateRemoteThread+LoadLibraryW) |
| `loader` | bin | install a WH_GETMESSAGE hook backed by RenderCore64.dll, scoped to one process |
| `hook-probe` | bin | single-process GDI interception + writeback (development stages) |
| `dwrite-probe` | bin | single-process DirectWrite interception + render |
| `text-window` / `dwrite-window` | bin | GDI / DirectWrite test targets |

## Pipeline (as built)

```
injector, or loader's WH_GETMESSAGE hook, gets RenderCore64.dll into a process
  → DllMain spawns a thread (off the loader lock) that:
      loads the active profile (%LOCALAPPDATA%\font-tuner\profile.ini, else default)
      hooks gdi32!ExtTextOutW               (MinHook)
      patches IDWriteBitmapRenderTarget::DrawGlyphRun in the shared vtable
  → each text draw:
      resolve the font from the DC / glyph run (GetFontData 'ttcf' for TTCs,
        or IDWriteFontFace file bytes + index)
      render the run with render-core (grey/LCD per profile) over the DC's pixels
      blit back, skipping the OS rasteriser
  → on unload: disable the GDI hooks, restore the DirectWrite vtable slot
```

Rendering is serialised by a mutex (one shared FreeType face, re-faced per draw).

## What works

- **render-core**: greyscale + LCD, gamma modes, weight/embolden — verified
  **bit-for-bit** against the C++ original (`render-core/verify/`).
- **GDI** text replaced under injection (string + `ETO_GLYPH_INDEX`), with the
  DC's font/colour/baseline, over the existing content.
- **DirectWrite** (`IDWriteBitmapRenderTarget::DrawGlyphRun`) replaced under
  injection via the shared-vtable patch.
- **Auto-injection** via a WH_GETMESSAGE hook (MacType's mechanism), scoped to a
  target process for safety; safe DLL unload.
- **Profiles** driven by font-tuner's own `.ini` files (`Profile::from_ini`).

## Not done / known limits

- Direct2D / GPU DirectWrite (`ID2D1RenderTarget::DrawGlyphRun`, glyph-run
  analysis) — only the GDI-interop bitmap path is hooked.
- `ETO_OPAQUE` / `ETO_CLIPPED` / `lpDx` are handled in `hook-probe` but not yet
  carried into `render-inject`; DPI transforms and non-natural measuring modes.
- Coloured LCD is only exercised for black/greyscale text in verification.
- System-wide auto-injection (all processes) is intentionally not wired into the
  tray; the loader stays per-process.
- `static mut` state should move to proper sync types; hot-path allocations
  (font bytes per draw) should be cached per face.

## Build & try (single process)

Requires `build/lib/freetype64.lib` first (run `build-core.ps1` once). Then, per
crate: `cargo build --release`. Quick demos:

```powershell
# offline render gallery (no hooking)
cargo run --release --manifest-path render-core/Cargo.toml

# GDI: inject into a test window
text-window\target\release\text-window.exe
injector\target\release\injector.exe text-window.exe <abs path>\RenderCore64.dll

# auto-injection via WH_GETMESSAGE, scoped to that window
loader\target\release\loader.exe <abs path>\RenderCore64.dll text-window.exe 8
```

The injected DLL logs to `%TEMP%\render-inject.log` and saves one capture PNG.
