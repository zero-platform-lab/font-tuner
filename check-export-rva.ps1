# Print the RVA of one named export of a 64-bit PE file (or nothing if the
# export is absent). Pure PowerShell so build-msi.ps1 does not need dumpbin.
#   .\check-export-rva.ps1 render-inject\target\release\RenderCore64.dll GetMsgProc
param(
    [Parameter(Mandatory)][string]$Path,
    [Parameter(Mandatory)][string]$Export
)
$ErrorActionPreference = 'Stop'
$d = [IO.File]::ReadAllBytes($Path)
$u32 = { param($o) [BitConverter]::ToUInt32($d, $o) }
$u16 = { param($o) [BitConverter]::ToUInt16($d, $o) }

$pe = & $u32 0x3c
if ([BitConverter]::ToUInt32($d, $pe) -ne 0x4550) { throw "$Path is not a PE file" }
$nsec = & $u16 ($pe + 6)
$optsz = & $u16 ($pe + 20)
$secs = 0..($nsec - 1) | ForEach-Object {
    $s = $pe + 24 + $optsz + 40 * $_
    [pscustomobject]@{ va = & $u32 ($s + 12); vs = & $u32 ($s + 8); raw = & $u32 ($s + 20) }
}
function Rva2Off([uint32]$rva) {
    foreach ($s in $secs) { if ($rva -ge $s.va -and $rva -lt $s.va + $s.vs) { return $s.raw + $rva - $s.va } }
    throw "RVA 0x$('{0:X}' -f $rva) is outside every section"
}

$exp = & $u32 ($pe + 24 + 112)   # export directory RVA (PE32+ data directory 0)
if ($exp -eq 0) { return }
$e = Rva2Off $exp
$names = & $u32 ($e + 24)
$funcs = Rva2Off (& $u32 ($e + 28))
$nameTab = Rva2Off (& $u32 ($e + 32))
$ordTab = Rva2Off (& $u32 ($e + 36))
for ($i = 0; $i -lt $names; $i++) {
    $n = Rva2Off (& $u32 ($nameTab + 4 * $i))
    $end = [Array]::IndexOf($d, [byte]0, [int]$n)
    if ([Text.Encoding]::ASCII.GetString($d, $n, $end - $n) -eq $Export) {
        $ord = & $u16 ($ordTab + 2 * $i)
        return (& $u32 ($funcs + 4 * $ord))
    }
}
