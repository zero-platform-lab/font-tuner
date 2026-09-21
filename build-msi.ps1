# Build everything and wrap it in an MSI with WiX v6:
#   1. native static libs incl. the FreeType fork (build-core.ps1)
#   2. RenderCore64.dll = the Rust render-inject core (links render-core)
#   3. font-tuner.exe + RenderBootstrap64.dll (release, workspace)
#   4. build/pkg = exe + DLLs + font-tuner.ini + ini\*.ini
#   5. wix build
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$ver = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value

& .\build-core.ps1
cargo build --release --workspace
if ($LASTEXITCODE) { exit $LASTEXITCODE }

# render-inject is outside the workspace (links the FreeType fork lib from
# build-core.ps1); build it on its own to produce the shipped RenderCore64.dll.
Push-Location render-inject
cargo build --release
Pop-Location
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$stage = "$PSScriptRoot\build\pkg"
# Clean the stage each run: it is harvested wholesale (ini\*.ini via WiX Files,
# and the DLLs by name), so stale files from an earlier build — removed
# profiles, the old C++ core — would otherwise leak into the MSI.
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage, "$stage\ini", dist | Out-Null
Copy-Item target\release\font-tuner.exe $stage -Force
Copy-Item target\release\RenderBootstrap64.dll $stage -Force
# GetMsgProc must stay at RVA 0x1000 (render-inject\build.rs pins it with
# /ORDER). Running processes keep the previous core mapped, and the new tray's
# hook is resolved as old_base + this RVA inside them — if it moved, every one
# of them would crash on its next message. Refuse to ship such a build.
$core = 'render-inject\target\release\RenderCore64.dll'
$rva = & .\check-export-rva.ps1 $core GetMsgProc
if ($rva -ne 0x1000) { throw "GetMsgProc is at RVA 0x$('{0:X}' -f $rva), expected 0x1000 - see render-inject\build.rs" }
Copy-Item $core $stage -Force
Copy-Item profiles\font-tuner.ini $stage -Force
Copy-Item profiles\ini\*.ini "$stage\ini" -Force

wix build wix\font-tuner.wxs -ext WixToolset.Util.wixext -arch x64 `
  -d "Version=$ver" -d "StageDir=$stage" `
  -o "dist\font-tuner-$ver-x64.msi"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Get-Item "dist\font-tuner-$ver-x64.msi" | Select-Object Name, Length
