# 権利関係 / Third-party notices

Font-tuner は GPL-3.0-or-later で配布する。MSI に同梱するものと、それぞれの権利者・ライセンス・配布時の義務を以下にまとめる。

## 同梱物と出どころ

| 同梱物 | 出どころ | ライセンス | 配布時にすること |
|---|---|---|---|
| `font-tuner.exe` | このリポジトリ (`src/`) | GPL-3.0-or-later | ソース公開 (このリポジトリ) |
| `MacType64.Core.dll` | [snowie2000/mactype](https://github.com/snowie2000/mactype) (`vendor/mactype`) を Rel+Detours / x64 でビルド | GPL-3.0 | ソース公開。改変していない (ビルド設定の注入のみ、`build/mactype.props`) |
| ↳ 内部に静的リンク: FreeType | [snowie2000/freetype](https://github.com/snowie2000/freetype) (`vendor/freetype`)。本家 FreeType に `FT_Glyph_To_BitmapEx` を足したフォーク | FTL または GPLv2+ の二択。GPL 側で受ける | **表示義務**: 「Portions of this software are copyright © The FreeType Project (www.freetype.org). All rights reserved.」を README か About に載せる |
| ↳ 内部に静的リンク: Microsoft Detours | [microsoft/Detours](https://github.com/microsoft/Detours) (`vendor/detours`) | MIT | 著作権表示と MIT の本文を同梱 (`vendor/detours/LICENSE.md`) |
| ↳ 内部に静的リンク: IniParser | [snowie2000/IniParser](https://github.com/snowie2000/IniParser) (`vendor/iniparser` サブモジュール参照。ソースは本リポジトリに同梱せず、上流から取得) | MacType (GPL-3.0) の依存 | — |
| `MacType.ini` | 純正 MacType 同梱のものを元に `[UnloadDll]` へ `font-tuner.exe` を追加 | MacType 配布物の一部 (GPL-3.0) | — |
| `ini\*.ini` (プロファイル 5 本) | 純正 MacType 同梱のプロファイルを元にした。各ファイル冒頭に作者名あり (Samantha Glocker, mufunyo) | MacType 配布物の一部として GPL-3.0 で配布されている | 作者コメント行を削らない |
| Rust `windows` crate | microsoft/windows-rs | MIT または Apache-2.0 | 著作権表示 |

## 同梱しないもの

| もの | 理由 |
|---|---|
| 純正 `MacType64.dll` (MTBootStrap) / `MacTray.exe` / `MacWiz.exe` / `MacTuner.exe` / `updater.exe` | Delphi 製でソース非公開。Font-tuner が置き換える対象そのもの |
| `easyhk64.dll` (EasyHook) | Detours 構成ではリンクしない |
| `wow64ext` | 32bit 非対応にしたので不要 |

## 系譜

MacType の描画コード (`ft.cpp` ほか) は 2006〜2008 年の gdi++ (FreeType 版) を直接の祖先とし、ソース冒頭に higambana (菅野友紀) 氏と 2ch の有志 (555 氏, 693 氏) への言及がある。MacType 側のリポジトリが全体を GPL-3.0 として公開しているので、そのまま GPL-3.0 として扱う。

## 注意点

1. **FreeType の表示義務**を README に入れること (上表)。
2. **「MacType」の名称**は snowie2000 氏 (FlyingSnow) のプロジェクト名。Font-tuner は別名にしてあり、純正と誤認されない表記にする。
3. MSI から純正インストーラを呼ばない。純正のインストールとは独立して `C:\Program Files\Font-tuner\` に入る。

## 同梱するライセンス本文

- `LICENSE` — GPL-3.0 (Font-tuner, MacType core)
- `vendor/freetype/docs/FTL.TXT`, `vendor/freetype/docs/GPLv2.TXT`
- `vendor/detours/LICENSE.md`

Portions of this software are copyright © The FreeType Project (www.freetype.org). All rights reserved.
