# 権利関係 / Third-party notices

Font-tuner は GPL-3.0-only で配布する。MSI に同梱するものと、それぞれの出どころ・ライセンス・配布時の義務を以下にまとめる。

## 同梱物と出どころ

| 同梱物 | 出どころ | ライセンス | 配布時にすること |
|---|---|---|---|
| `font-tuner.exe` (トレイ) | このリポジトリ (`src/`) | GPL-3.0-only | ソース公開 (このリポジトリ) |
| `RenderCore64.dll` (描画コア + フック) | このリポジトリ (`render-inject/`, `render-core/`)。描画アルゴリズム (`render-core`) は [snowie2000/mactype](https://github.com/snowie2000/mactype) の `ft.cpp` ほかを Rust に移植したもの (翻訳 = 改変物) | GPL-3.0-only (派生元 MacType が GPLv3 の LICENSE のみで「or later」を明示していないため、v3 限定) | ソース公開。派生元の表記を消さない |
| ↳ 内部に静的リンク: FreeType | [snowie2000/freetype](https://github.com/snowie2000/freetype) (`vendor/freetype`)。本家 FreeType に `FT_Glyph_To_BitmapEx` を足したフォーク | FTL または GPLv2+ の二択 | **表示義務**: 「Portions of this software are copyright © The FreeType Project (www.freetype.org). All rights reserved.」を README か About に載せる |
| ↳ フック機構: retour + iced-x86 | [Hpmason/retour-rs](https://github.com/Hpmason/retour-rs) (inline detour) / [icedland/iced](https://github.com/icedland/iced) (逆アセンブラ) | retour: BSD-3-Clause / iced-x86: MIT | 著作権表示 (crate に同梱) |
| `RenderBootstrap64.dll` (子プロセス用ローダ) | このリポジトリ (`bootstrap/`) | GPL-3.0-only | ソース公開 |
| `font-tuner.ini` | このリポジトリ (`profiles/`)。`[UnloadDll]` の除外リストは純正 MacType 同梱のものを元にした | GPL-3.0 | — |
| `ini\*.ini` (プロファイル 5 本) | 純正 MacType 同梱のプロファイルを元にした。各ファイル冒頭に作者名あり (Samantha Glocker, mufunyo) | MacType 配布物の一部として GPL-3.0 で配布されている | 作者コメント行を削らない |
| Rust `windows` / `windows-numerics` crate | microsoft/windows-rs | MIT または Apache-2.0 | 著作権表示 |
| Rust `image` crate (`render-core` の PNG 出力。DLL には含めない) | image-rs | MIT または Apache-2.0 | — |

## 同梱しないもの

| もの | 理由 |
|---|---|
| 純正 MacType のバイナリ一式 (コア DLL、トレイ、ウィザード、ブートストラップ、更新ツール) | Font-tuner が置き換える対象。コアは Rust 移植 (`RenderCore64.dll`) に置き換え済み |
| Microsoft Detours / IniParser / MinHook | C++ コアを同梱しなくなったので不要 (サブモジュールからも外した)。フックは純 Rust の retour + iced-x86 |
| `easyhk64.dll` (EasyHook) / `wow64ext` | 使わない (32bit 非対応) |

## 系譜

`render-core` の描画コード (ガンマ・コントラスト・LCD フィルタ) は MacType の `ft.cpp` を Rust に移植したもので、本家との一致を検証しながら書いた。MacType の描画コードは 2006〜2008 年の gdi++ (FreeType 版) を直接の祖先とし、ソース冒頭に higambana (菅野友紀) 氏と 2ch の有志 (555 氏, 693 氏) への言及がある。MacType 側のリポジトリが全体を GPL-3.0 として公開しているので、移植物もそのまま GPL-3.0 として扱う。

## 注意点

1. **FreeType の表示義務**を README に入れること (上表)。
2. **「MacType」の名称**は snowie2000 氏 (FlyingSnow) のプロジェクト名。Font-tuner は別名にしてあり、純正と誤認されない表記にする。製品側のファイル名・設定ファイル名・UI 文言には使わない。派生元の表記としてのみ残す。
3. MSI から純正インストーラを呼ばない。純正のインストールとは独立して `C:\Program Files\Font-tuner\` に入る。

## 同梱するライセンス本文

- `LICENSE` — GPL-3.0 (Font-tuner、RenderCore64 の派生元)
- `vendor/freetype/docs/FTL.TXT`, `vendor/freetype/docs/GPLv2.TXT`

Portions of this software are copyright © The FreeType Project (www.freetype.org). All rights reserved.
