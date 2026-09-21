# Build the MacType core DLL (x64, Detours flavour) and its three static
# dependencies from the submodules under vendor/. Nothing upstream is edited:
# include/lib paths are injected through build/mactype.props.
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$vs   = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$msb  = Join-Path $vs 'MSBuild\Current\Bin\MSBuild.exe'
$vc64 = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$lib  = "$PSScriptRoot\build\lib"
New-Item -ItemType Directory -Force $lib | Out-Null

Write-Host '== Detours'
cmd /c "`"$vc64`" >nul && cd /d `"$PSScriptRoot\vendor\detours\src`" && nmake" | Out-Null
Copy-Item vendor\detours\lib.X64\detours.lib "$lib\detours64.lib" -Force

Write-Host '== IniParser'
& $msb vendor\iniparser\IniParser\IniParser.vcxproj /p:Configuration=Release /p:Platform=x64 /p:PlatformToolset=v143 /p:WindowsTargetPlatformVersion=10.0 /nologo /v:q
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Copy-Item vendor\iniparser\IniParser\x64\Release\IniParser64.lib $lib -Force

Write-Host '== FreeType (snowie2000 fork, has FT_Glyph_To_BitmapEx)'
& $msb vendor\freetype\builds\windows\vc2010\freetype.vcxproj "/p:Configuration=Release Static" /p:Platform=x64 /p:WindowsTargetPlatformVersion=10.0 /nologo /v:q
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Copy-Item "vendor\freetype\objs\x64\Release Static\freetype.lib" "$lib\freetype64.lib" -Force

Write-Host '== MacType64.Core.dll'
& $msb vendor\mactype\gdipp.vcxproj "/p:Configuration=Rel+Detours" /p:Platform=x64 "/p:SolutionDir=$PSScriptRoot\vendor\mactype\" "/p:ForceImportBeforeCppTargets=$PSScriptRoot\build\mactype.props" /nologo /v:q /m
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Get-Item "vendor\mactype\x64\Rel+Detours\MacType64.Core.dll" | Select-Object Name, Length
