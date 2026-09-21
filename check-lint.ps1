# スクリプトとドキュメントを外部のチェッカにかける。
#
#   PowerShell  PSScriptAnalyzer (Microsoft)。除外ルールと理由は
#               PSScriptAnalyzerSettings.psd1 に書いてある
#   ドキュメント textlint + preset-ja-technical-writing + prh。助詞の重複・
#               一文の長さ・漢字の連続など、読んでも気づきにくいものを機械で拾う
#
# 初回だけ導入が要る。
#   Install-Module PSScriptAnalyzer -Scope CurrentUser
#   npm install
#
# 指摘があれば exit 1。
#Requires -Version 7.3
[CmdletBinding()]
param(
  # PowerShell だけ / ドキュメントだけ に絞る
  [switch]$ScriptsOnly,
  [switch]$DocsOnly,
  # textlint を --fix で走らせる（npm run lint:text:fix から使う）
  [switch]$Fix
)
$ErrorActionPreference = 'Stop'
$global:PSNativeCommandUseErrorActionPreference = $false
$fail = 0

# 自分で書いたものだけを見る。vendor は上流のソース、target/build/dist は生成物。
$skip = '[\\/](node_modules|\.git|vendor|target|build|dist)[\\/]'

if (-not $DocsOnly) {
  Write-Host '== PSScriptAnalyzer =='
  if (-not (Get-Module -ListAvailable PSScriptAnalyzer)) {
    Write-Warning '  PSScriptAnalyzer が無い。Install-Module PSScriptAnalyzer -Scope CurrentUser'
    $fail = 1
  } else {
    Import-Module PSScriptAnalyzer
    $settings = Join-Path $PSScriptRoot 'PSScriptAnalyzerSettings.psd1'
    $ps1 = Get-ChildItem -Path $PSScriptRoot -Filter *.ps1 -Recurse -File |
      Where-Object { $_.FullName -notmatch $skip }
    $r = foreach ($f in $ps1) { Invoke-ScriptAnalyzer -Path $f.FullName -Settings $settings }
    if ($r) {
      $r | ForEach-Object {
        '  {0}:{1} [{2}] {3}' -f ($_.ScriptPath -replace [regex]::Escape($PSScriptRoot + [IO.Path]::DirectorySeparatorChar), ''),
                                 $_.Line, $_.RuleName, $_.Message
      }
      Write-Host ('  指摘 {0} 件' -f $r.Count)
      $fail = 1
    } else {
      Write-Host '  指摘なし'
    }
  }
}

if (-not $ScriptsOnly) {
  Write-Host ''
  Write-Host '== textlint (ドキュメント) =='
  # node_modules/.bin には拡張子なしのシム (sh スクリプト) と .cmd が並ぶ。
  # Windows で拡張子なしのほうを起動すると、指摘の有無と関係なく失敗する。
  $tlBase = Join-Path $PSScriptRoot 'node_modules/.bin/textlint'
  $tl = if ($IsWindows -and (Test-Path "$tlBase.cmd")) { "$tlBase.cmd" } else { $tlBase }
  if (-not (Test-Path $tl)) {
    Write-Warning '  textlint が無い。npm install を実行する'
    $fail = 1
  } else {
    # 日本語用のルールなので、日本語で書いた .md だけを見る。英語の
    # docs/SPEC.md 等にかけると、文長・読点数・括弧対応が誤発火する。
    # 「かなを含む」では足りない (英語の文書にもプロファイル名などで数文字
    # 混ざる) ので、かなの比率で判定する。中身で選ぶので、日本語の文書が
    # 増えても設定を直さなくてよい。
    $md = Get-ChildItem -Path $PSScriptRoot -Filter *.md -Recurse -File |
      Where-Object { $_.FullName -notmatch $skip } |
      Where-Object {
        # 空ファイルでは -Raw が $null を返し、regex が投げる。
        # $ErrorActionPreference = 'Stop' なのでそこで全体が止まるため、先に '' にする。
        $t = (Get-Content $_.FullName -Raw -Encoding UTF8) ?? ''
        $kana = ([regex]::Matches($t, '[\p{IsHiragana}\p{IsKatakana}]')).Count
        $body = ($t -replace '\s', '').Length
        $body -gt 0 -and ($kana / $body) -ge 0.05
      }
    if (-not $md) {
      # ファイルを渡さずに起動すると textlint は usage を出して 0 で終わる。
      # それを指摘なしと報告しないよう、ここで打ち切る。
      Write-Host '  対象の日本語ドキュメントなし'
    } else {
      # $args は自動変数なので使わない
      $tlArgs = @('--config', (Join-Path $PSScriptRoot '.textlintrc.json'), '-f', 'compact')
      if ($Fix) { $tlArgs += '--fix' }
      $out = & $tl @tlArgs @($md.FullName) 2>&1
      $rc = $LASTEXITCODE
      if ($out) { $out | ForEach-Object { "  $_" } }
      if ($rc -ne 0 -and -not $out) {
        # 指摘が 1 件も出ていないのに非ゼロ = textlint 自体が動いていない。
        Write-Warning "  textlint を実行できない (exit $rc)。npm install をやり直す"
        $fail = 1
      } elseif ($rc -ne 0) { $fail = 1 }
      else { Write-Host '  指摘なし' }
    }
  }
}

Write-Host ''
if ($fail) { Write-Host '指摘あり'; exit 1 }
Write-Host 'どちらも指摘なし'
