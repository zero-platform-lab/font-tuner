# render-inject — design & roadmap (NOT yet functional)

The system-wide half of MacType: a DLL injected into every process that
intercepts text drawing and renders it with `render-core` instead of Windows'
own rasteriser. This is the hard, high-risk part — a bug here runs **inside every
process on the machine**, so a fault crashes apps or destabilises the desktop.

This crate currently contains only a **safe skeleton** (a `DllMain` that does
nothing, linking `render-core` to prove the packaging path). No hooking, no
injection, no writeback is implemented. Each stage below is gated on the
previous one being verified.

## Target architecture

```
render-core (lib)          rendering brain (verified bit-exact)
      ^ links
render-inject (cdylib DLL)  RenderCore64.dll — injected into each process
  - DllMain: on attach, install hooks (off the loader lock)
  - hook ExtTextOutW / TextOutW / (GDI text APIs)  via retour/minhook
  - hook DirectWrite (IDWriteBitmapRenderTarget / IDWriteFontFace) via COM vtable
  - for each intercepted draw: shape -> render-core -> blit into the target DC/DIB
```

Injection into other processes reuses the existing loader design (the tray sets
a `WH_GETMESSAGE` hook whose proc lives in this DLL; the child bootstrap
`MTBootStrap64.dll` LoadLibrary's it). This crate replaces `MacType64.Core.dll`,
not the injection mechanism.

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
  `MTBootStrap64` already does).
- Every stage stays opt-in and process-scoped until proven; no system-wide
  enable without a tested kill switch.
- Chrome/Edge renderer & GPU stay unreachable (MS-signed-binaries mitigation) —
  same limitation as the C++ core.

## Status

Stage 1 only. Stages 2+ are deliberately not started; they need careful,
tested, opt-in work rather than bulk code generation.
