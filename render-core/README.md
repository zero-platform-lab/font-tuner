# render-core (experimental)

An offline Rust port of the **MacType glyph-rendering core** — the
gamma/contrast/LCD tuning and linear-space blend in upstream [`ft.cpp`](https://github.com/snowie2000/mactype/blob/05052e88c7ce134f93b66db95132284a1ed10de7/ft.cpp) that
sit on top of FreeType. Given *a character + a rendering profile* it produces
pixels exactly as MacType would.

**What this is not:** it does not hook or inject anything. The system-wide half
of MacType (GDI / DirectWrite interception, DLL injection, writing pixels back
into each process) is out of scope and stays in the C++ core.

## Relationship to FreeType

FreeType is the rasteriser (outline → coverage bitmap); MacType is the tuning +
hooking layer on top. This crate **reuses the same FreeType fork unchanged**
(`build/lib/freetype64.lib`, via the small C shim in `shim.c`), so glyph
rasterisation is identical — only MacType's tuning/blend is reimplemented in
Rust.

## Layout

| file | role |
|---|---|
| `src/filter.rs` | gamma/contrast LUTs + linear-space blend (`CAlphaBlend::init` / `doAB`) |
| `src/ft.rs` | safe wrapper over the FreeType shim; load-flag / render-mode selection |
| `src/config.rs` | `Profile` (subset of MacType.ini) + presets |
| `src/render.rs` | glyph layout + greyscale / LCD compositing onto an RGB canvas |
| `src/main.rs` | demo CLI + `verify` curve dumps |
| `shim.c` | FreeType C shim (uses the fork headers, links `freetype64.lib`) |

## Verification (bit-exact)

The blend math is proven identical to the C++ original, not just "close":

- **Greyscale**: all 256 coverage values × 6 profiles — bit-for-bit equal.
- **LCD**: 1296 vectors (AAMode 2/3 × backgrounds × coverage triples) — equal.

`verify/oracle.cpp` is the exact `ft.cpp` math compiled standalone (MSVC);
`verify/compare.py` diffs it against this crate's dumps.

```powershell
# from render-core/
cargo run --release -- verify           # writes rust-*.txt, lcd-rust.txt
cl /nologo /O2 /EHsc verify\oracle.cpp /Fe:oracle.exe   # in a VS x64 prompt
.\oracle.exe                            # writes cpp-*.txt, lcd-cpp.txt
python verify\compare.py                # -> ALL BIT-EXACT
```

## Build

Requires `build/lib/freetype64.lib` first — run `build-core.ps1` at the repo
root once. Then:

```powershell
cargo build --release        # from render-core/
cargo run --release          # renders clean-greyscale / clean-sharp / accurate PNGs
```

This crate is excluded from the repo workspace (it links a build artifact and is
not part of the shipped product).

## Status / not yet done

- Colour text beyond black, shadow, mono, BGRA/emoji paths
- Outline embolden variants (BolderMode 1/2, synthetic bold), italic slant
- The hook / injection / DirectWrite layer (the hard, system-wide part)
