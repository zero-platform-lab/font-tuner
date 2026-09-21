# Build everything and wrap it in an MSI with WiX v6:
#   1. MacType core DLL from vendor/ (build-core.ps1)
#   2. font-tuner.exe + MTBootStrap64.dll (release, workspace)
#   3. build/pkg = exe + DLLs + MacType.ini + ini\*.ini
#   4. wix build
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$ver = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value

& .\build-core.ps1
cargo build --release --workspace
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$stage = "$PSScriptRoot\build\pkg"
New-Item -ItemType Directory -Force $stage, "$stage\ini", dist | Out-Null
Copy-Item target\release\font-tuner.exe $stage -Force
Copy-Item target\release\MTBootStrap64.dll $stage -Force
Copy-Item "vendor\mactype\x64\Rel+Detours\MacType64.Core.dll" $stage -Force
Copy-Item profiles\MacType.ini $stage -Force
Copy-Item profiles\ini\*.ini "$stage\ini" -Force

wix build wix\font-tuner.wxs -ext WixToolset.Util.wixext -arch x64 `
  -d "Version=$ver" -d "StageDir=$stage" `
  -o "dist\font-tuner-$ver-x64.msi"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Get-Item "dist\font-tuner-$ver-x64.msi" | Select-Object Name, Length
