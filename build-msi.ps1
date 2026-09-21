# Build everything and wrap it in an MSI with WiX v6:
#   1. native static libs incl. the FreeType fork (build-core.ps1)
#   2. RenderCore64.dll = the Rust render-inject core (links render-core)
#   3. font-tuner.exe + MTBootStrap64.dll (release, workspace)
#   4. build/pkg = exe + DLLs + MacType.ini + ini\*.ini
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
New-Item -ItemType Directory -Force $stage, "$stage\ini", dist | Out-Null
Copy-Item target\release\font-tuner.exe $stage -Force
Copy-Item target\release\MTBootStrap64.dll $stage -Force
Copy-Item render-inject\target\release\RenderCore64.dll $stage -Force
Copy-Item profiles\MacType.ini $stage -Force
Copy-Item profiles\ini\*.ini "$stage\ini" -Force

wix build wix\font-tuner.wxs -ext WixToolset.Util.wixext -arch x64 `
  -d "Version=$ver" -d "StageDir=$stage" `
  -o "dist\font-tuner-$ver-x64.msi"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Get-Item "dist\font-tuner-$ver-x64.msi" | Select-Object Name, Length
