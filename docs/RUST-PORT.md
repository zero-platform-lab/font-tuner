# MacType パイプラインの Rust 再実装

MacType がやっていること — 全プロセスでテキスト描画を横取りし、FreeType +
独自のガンマ/LCD 調整で描く — をゼロから Rust で再実装したもの。これが**出荷
される**コアで、`font-tuner` の MSI は C++ の MacType コアではなくこのクレート
の `RenderCore64.dll` を入れる。

FreeType 自体はフォークの `freetype64.lib` を変更せず再利用する。それ以外 —
調整の数式、フック、FreeType との橋渡し — はすべて Rust。出荷 DLL にリンクされる
ネイティブコードはその FreeType 静的ライブラリだけ。

## クレート

| クレート | 種別 | 役割 |
|---|---|---|
| `render-core` | lib | 描画エンジン: ガンマ/コントラスト/LCD の LUT + ブレンド（ft.cpp から移植、計算式に対して 1 階調以内で一致）、FreeType への直接 FFI（C シムなし）、`Profile`（`from_ini` 含む）、グリフ/文字列の合成 |
| `render-inject` | cdylib `RenderCore64.dll` | 各プロセスに注入され、GDI + DirectWrite/Direct2D のテキストをフックして render-core で描く。自動注入用に `GetMsgProc` を export。自己を常駐固定し、動作中プロセスから決してアンマップされない |
| `loader` | bin | RenderCore64.dll を使う WH_GETMESSAGE フックを 1 プロセスだけに張る。単一アプリでコアを試すテストハーネス |

## パイプライン（実装済み）

```
トレイのグローバル（または loader の単一プロセス）WH_GETMESSAGE フックが RenderCore64.dll をマップ
  → DllMain が自己を常駐固定（GetModuleHandleEx FLAG_PIN）し、スレッドを起こす
    （ローダーロックの外で。プロセスごとに一度、名前付きミューテックスで保証）:
      有効なプロファイルを読む（インストール先 font-tuner.ini の AlternativeFile、無ければ既定）
      gdi32!ExtTextOutW をフック          （retour インライン detour、他スレッド凍結。
                                           TextOutW/TextOutA/ExtTextOutA もここに来る）
      gdi32!GetGlyphOutlineW/A をフック    （ClipBoxFix: メトリクスのみの問い合わせを補正）
      共有 vtable の IDWriteBitmapRenderTarget::DrawGlyphRun をパッチ
      IDWriteFactory{,2,3}::CreateGlyphRunAnalysis をパッチ（→ CreateAlphaTexture）
      d2d1!D2D1CreateFactory / D2D1CreateDevice / D2D1CreateDeviceContext をフックし、
        ID2D1Factory1..7::CreateDevice → ID2D1Device..6::CreateDeviceContext →
        全ターゲットで DrawGlyphRun (29) / 記述付き DrawGlyphRun (82) /
        SetTextAntialiasMode (34) / SetTextRenderingParams (36)
  → テキスト描画ごとに:
      DC / グリフラン からフォントを解決（TTC は GetFontData 'ttcf'、
        または IDWriteFontFace のファイルバイト + index）
      render-core で描画（プロファイルに応じ grey/LCD）し、DC の既存ピクセルに重ねる
      blit で書き戻し、OS ラスタライザを飛ばす
  → GDI DC を貸せない Direct2D ターゲット（DXGI サーフェス）: DrawGlyphRun は
      プロファイルの [DirectWrite] IDWriteRenderingParams、グレースケール/ClearType
      のアンチエイリアスモード、grid fit 無効時の 1/65535 変換ずらしで OS に描かせる
      — Direct2D 全体に対して上流がやっていることと同じ
  → 動作中プロセスから決してアンロードされない（常駐固定）。DllMain の DETACH は
    プロセス終了時にだけ来る no-op
```

描画はミューテックスで直列化する（共有 FreeType face 1 つ、描画ごとに reface）。
フックは **retour**（純 Rust、iced-x86 逆アセンブラ）で、MinHook/Detours ではない。
retour はパッチ中に他スレッドを止めないので、`install_hook` がバイトパッチの前後で
他スレッドを凍結する。一度きりの vtable パッチはミューテックスで直列化し、2 つの
スレッドが同時に「元の関数」を捕まえて再帰するのを防ぐ。

## 動くもの

- **render-core**: グレースケール + LCD、ガンマモード、weight/embolden — ブレンド
  計算式（docs/SPEC.md §2.3）を f32 で実装（上流の固定小数点の整数ではない。上流と
  1 階調以内で一致し、こちらの方が正確）。
- **GDI** テキストを注入下で置換（文字列 + `ETO_GLYPH_INDEX`）。DC のフォント/色/
  ベースラインで、既存の内容の上に描く。
- **DirectWrite**（`IDWriteBitmapRenderTarget::DrawGlyphRun`）を共有 vtable の
  パッチで注入下に置換。
- **自動注入** を WH_GETMESSAGE フックで（上流の仕組み）。`loader` は 1 プロセスに
  限定して試すため、トレイはグローバルに張る。DLL は自己を常駐固定するので、
  動作中プロセスから決してアンマップされない。
- **プロファイル** は font-tuner 自身の `.ini`（`Profile::from_ini`）で駆動し、
  アタッチ時にインストール先の `font-tuner.ini`（`AlternativeFile`）を一度読む。
  プロファイル切替は以降に起動するプロセスで効く。トレイの「プロファイルを再読み込み」
  は登録メッセージをブロードキャストし、注入済みプロセスが自分の UI スレッドで
  ini を読み直す（監視スレッドなし）。
- **GDI の忠実性**: `render-inject` で `ETO_OPAQUE` / `ETO_CLIPPED` / `lpDx` を尊重。
- **性能**: フォントファイルの抽出 + reface はフォントが実際に変わったときだけ
  （キャッシュ）で、描画ごとではない。
- **テスト**: `render-core` はブレンド（端点、単調性、全 GammaMode、gamma 1.25 の
  回帰）と `Profile::from_ini` の `cargo test` を持つ。テストが照合する計算式は
  docs/SPEC.md §2.3。

## 移植範囲 = MacType の全フック

これは**移植**であり、対象は C++ の MacType が横取りするすべて
（[`hooklist.h`](https://github.com/snowie2000/mactype/blob/05052e88c7ce134f93b66db95132284a1ed10de7/hooklist.h)、[`directwrite.cpp`](https://github.com/snowie2000/mactype/blob/05052e88c7ce134f93b66db95132284a1ed10de7/directwrite.cpp)、上流コミット `05052e8`）で、絞った部分集合ではない。MacType が
フックするテキスト経路と、現状:

| 経路 | MacType | 本実装 |
|---|---|---|
| GDI `ExtTextOutW` | あり | **完了** |
| GDI `ExtTextOutA` / `TextOutW` / `TextOutA` | あり | **自前フック不要でカバー**: Windows 11 (26200) では 3 つとも我々のインライン detour が張る `ExtTextOutW` 入口に来る（プローブハーネスで確認） |
| GDI `GetGlyphOutlineW` / `GetGlyphOutlineA`（上流の "ClipBoxFix"） | あり | **完了**（`gdi_metrics.rs`。`[Experimental] ClipBoxFix`、既定オン。プロセス別の `[Experimental@exe]` 節は未適用） |
| DirectWrite `IDWriteBitmapRenderTarget::DrawGlyphRun`（vtbl 3） | あり | **完了** |
| DirectWrite `CreateGlyphRunAnalysis` → `CreateAlphaTexture`（Chromium/Skia、VS Code）、`IDWriteFactory2`/`3` の overload 含む | あり | **完了** |
| Direct2D `ID2D1RenderTarget::DrawGlyphRun`（vtbl 29） | あり | **完了**（`D2D1CreateFactory` → RT 生成 → vtable ごとのパッチ経由） |
| Direct2D `DrawGlyphRun1`（vtbl 82）/ `ID2D1DeviceContext` | あり | **完了**（`D2D1CreateDevice`、`D2D1CreateDeviceContext`、`ID2D1Factory1..7::CreateDevice`、`ID2D1Device..6::CreateDeviceContext`）。GDI DC を貸せる所は render-core、そうでなければ上流の rendering-params 経路 |
| Direct2D `SetTextAntialiasMode` (34) / `SetTextRenderingParams` (36) をプロファイルに強制 | あり | **完了** |
| `DWriteCreateFactory` / `GetGdiInterop` | あり | 不要: 上流はこれらを共有 vtable に到達するためだけに使う。我々は自前の factory から直接その vtable をパッチする |
| `CreateTextFormat` / `CreateFontFace`（上流の `[FontSubstitutes]` フォント置換） | あり | **判断で未移植**: Font-tuner はトレイのシステムフォント切替でフォントを置換する。出荷プロファイルはすべて `FontSubstitutes=0` |

MacType がカバーする経路を対象外扱いしない。残りの行は「まだ移植していない」で
あって「意図的に落とした」ではない。

## その他の残り

- DPI 変換と、自然でない DirectWrite measuring mode。
- カラー LCD は検証では黒/グレースケールのテキストでしか動かしていない。

## ビルドして試す（単一プロセス）

先に `build/lib/freetype64.lib` が要る（`build-core.ps1` を一度実行）。その後
クレートごとに `cargo build --release`。手早いデモ:

```powershell
# オフライン描画ギャラリー（フックなし）
cargo run --release --manifest-path render-core/Cargo.toml

# 動作中アプリ 1 つ（ここでは charmap）に 8 秒だけ注入し、その後フックを外す
loader\target\release\loader.exe <絶対パス>\RenderCore64.dll charmap.exe 8
```

検証はトレイを止めて行う。動作中のトレイはインストール済みコアを全プロセスに
注入するので、2 つのビルドが同じフックを奪い合う。

注入 DLL は `%TEMP%\render-inject.log` に記録し、キャプチャ PNG を 1 枚保存する。
各段階を開発した probe/window クレートは、成果が `render-inject` に落ちた時点で
削除した。必要なら git 履歴を見る。
