# Build the one native dependency the shipped DLL needs: the FreeType fork
# (snowie2000/freetype, which adds FT_Glyph_To_BitmapEx) as a static lib.
# render-core links build/lib/freetype64.lib; everything else is Rust.
#
# The C++ MacType core is not built (vendor/mactype stays as the reference
# the port is checked against); Microsoft Detours and IniParser are not even
# vendored any more. The render core is RenderCore64.dll
# (render-inject/render-core) and hooks with retour.
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$vs   = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$msb  = Join-Path $vs 'MSBuild\Current\Bin\MSBuild.exe'
$lib  = "$PSScriptRoot\build\lib"
New-Item -ItemType Directory -Force $lib | Out-Null

Write-Host '== FreeType (snowie2000 fork, has FT_Glyph_To_BitmapEx)'
& $msb vendor\freetype\builds\windows\vc2010\freetype.vcxproj "/p:Configuration=Release Static" /p:Platform=x64 /p:WindowsTargetPlatformVersion=10.0 /nologo /v:q
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Copy-Item "vendor\freetype\objs\x64\Release Static\freetype.lib" "$lib\freetype64.lib" -Force
Get-Item "$lib\freetype64.lib" | Select-Object Name, Length
