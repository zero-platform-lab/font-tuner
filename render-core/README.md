# render-core（実験的）

**MacType のグリフ描画コア**をオフラインで Rust に移植したもの。FreeType の
上に乗るガンマ/コントラスト/LCD 調整と線形空間ブレンド（上流
[`ft.cpp`](https://github.com/snowie2000/mactype/blob/05052e88c7ce134f93b66db95132284a1ed10de7/ft.cpp)）を移植している。*文字 + 描画プロファイル*を与えると、MacType と同じ計算式で
ピクセルを生成する。

**これでないもの:** フックや注入をしない。MacType のシステム全体側（GDI /
DirectWrite の横取り、DLL 注入、各プロセスへのピクセル書き戻し）は対象外で、
`render-inject` 側にある。

## FreeType との関係

FreeType がラスタライザ（アウトライン → カバレッジビットマップ）で、MacType は
その上の調整 + フック層。このクレートは**同じ FreeType フォークを変更せず再利用**
する（`build/lib/freetype64.lib`）。グリフのラスタライズは同一で、MacType の
調整・ブレンドだけを Rust で書き直している。

## 構成

| ファイル | 役割 |
|---|---|
| `src/filter.rs` | ガンマ/コントラスト LUT + 線形空間ブレンド（`CAlphaBlend::init` / `doAB`） |
| `src/ft.rs` | FreeType への直接 FFI。ロードフラグ / レンダーモードの選択 |
| `src/config.rs` | `Profile`（MacType.ini の一部）+ プリセット |
| `src/render.rs` | グリフ配置 + グレースケール / LCD を RGB キャンバスへ合成 |
| `src/main.rs` | デモ CLI（プロファイルごとにサンプル文字列を描く） |

## 検証

ブレンドの計算式が仕様。実装するガンマ符号化・カバレッジ曲線・線形空間ブレンド
は **docs/SPEC.md §2.3** を参照。正しさは「その計算式を実装しているか」なので、
`cargo test`（`src/filter.rs`）で検証する: 端点、単調性、全 `GammaMode` 分岐、
gamma 1.25 のグレースケール回帰。ブレンドテストは純粋なのでフォント不要。
FreeType 経路のテストはシステムフォントが無ければ綺麗にスキップする。

## ビルド

先に `build/lib/freetype64.lib` が要る。リポジトリ直下で `build-core.ps1` を
一度実行する。その後:

```powershell
cargo build --release        # render-core/ から
cargo run --release          # clean-greyscale / clean-sharp / accurate の PNG を描く
```

このクレートはリポジトリのワークスペースから除外している（ビルド生成物をリンク
し、出荷物には含まれないため）。

## 状態・未着手

- 黒以外の色テキスト、影、モノクロ、BGRA/絵文字の経路
- アウトラインの太字化バリエーション（BolderMode 1/2、合成ボールド）、斜体
- フック / 注入 / DirectWrite 層（難しいシステム全体側）
