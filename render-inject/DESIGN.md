# render-inject — design & history

The system-wide half of the tool: `RenderCore64.dll`, injected into every
process, intercepts text drawing and renders it with `render-core` instead of
Windows' own rasteriser. This is the hard, high-risk part — the code runs
**inside every process on the machine**, so a fault crashes apps or
destabilises the desktop.

This is now **implemented and shipped** (see `docs/RUST-PORT.md` for the
consolidated, current picture). The staged roadmap below is kept as history:
each stage was built and verified before the next.

## Target architecture

```
render-core (lib)          rendering brain (verified bit-exact)
      ^ links
render-inject (cdylib DLL)  RenderCore64.dll — injected into each process
  - DllMain: on attach, install hooks (off the loader lock)
  - hook ExtTextOutW (GDI text APIs) via retour inline detour (pure Rust)
  - hook DirectWrite (IDWriteBitmapRenderTarget / IDWriteFontFace) via COM vtable
  - for each intercepted draw: shape -> render-core -> blit into the target DC/DIB
```

Injection into other processes reuses the existing loader design (the tray sets
a `WH_GETMESSAGE` hook whose proc lives in this DLL; the child bootstrap
`RenderBootstrap64.dll` LoadLibrary's it). This crate is the render core
itself, not the injection mechanism.

## Stages (each verified before the next)

1. **Skeleton (done):** cdylib builds; `DllMain` returns TRUE; a test export
   calls into `render-core`. No hooks. Safe.
2. **GDI capture harness (offline):** in a *single test process only*, hook
   `ExtTextOutW`, capture its arguments (string, DC, position, font), and log —
   do **not** yet change output. Verify we can reconstruct MacType's inputs.
3. **GDI writeback (single process):** render with `render-core` and blit into
   the DC's DIB; compare on-screen against the C++ core in the same app. Still
   opt-in, one process, easy to kill.
4. **Injection (few processes):** enable the loader path for a small allowlist
   (e.g. notepad), never system-wide, with a hard kill switch.
5. **DirectWrite:** COM vtable interception. Hardest; version-dependent.
6. **Broaden** cautiously.

## Risks / rules

- Never LoadLibrary under the loader lock (spawn a thread from DllMain, as
  `RenderBootstrap64` already does).
- Self-pin on attach (GetModuleHandleEx FLAG_PIN): never unmapped from a
  running process, so no code can run after unmap. DllMain DETACH is a no-op.
- retour does not stop threads while patching, so freeze the other threads
  around each byte patch; serialise one-time vtable patches with a mutex.
- Every stage stays opt-in and process-scoped until proven; no system-wide
  enable without a tested kill switch.
- Chrome/Edge renderer & GPU stay unreachable (MS-signed-binaries mitigation) —
  same limitation as the C++ core.

## Status

All stages below are now implemented (GDI + DirectWrite interception, writeback,
font resolution, cross-process and WH_GETMESSAGE injection, profile loading,
self-pinning instead of unload). See `docs/RUST-PORT.md` for the current, consolidated picture and
`../loader` to try the core in a single process. The per-stage probe/window
crates that built this up were removed once the work landed in this DLL.
Remaining work is fidelity/robustness (see RUST-PORT.md "Not done").
