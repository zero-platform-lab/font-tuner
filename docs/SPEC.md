# Font-tuner 仕様

Windows 向けの小さな自己完結型フォント描画チューナ（トレイ + 注入する描画コア）。upstream は https://github.com/snowie2000/mactype（移植元コミット `05052e8`）。upstream の非公開トレイが「トレイモード」でやることを再現し、描画コアを Rust で書き直して同梱し、描画プロファイル一式を束ね、トレイメニューのシステムフォント切替とカスタムプロファイルを足す。Windows 11、64bit 専用。

この文書は実装の現状を書く。upstream と違うところ、実機で確かめたこと、未確認のことは書き分ける。

構成:

| クレート | 成果物 | 役割 |
|---|---|---|
| `font-tuner`（`src/`） | `font-tuner.exe` | トレイ。フックを張る。プロファイル選択、カスタムダイアログ、システムフォント切替 |
| `render-inject` | `RenderCore64.dll` | 注入されるコア。GDI / DirectWrite / Direct2D をフックし、`render-core` で描く |
| `render-core` | （lib） | 描画エンジン。FreeType の上のヒンティング・ガンマ・ブレンド。注入もフックもしない |

---

## 1. アーキテクチャ

### 1.1 注入の流れ

```
font-tuner.exe ──(SetWindowsHookExW WH_GETMESSAGE, グローバル)──▶ 全 64bit GUI プロセス
                                                          RenderCore64.dll をマップ
                                                          （フックプロシージャがそこにある）
                          コアの DllMain がフォント API をフック（GDI / DirectWrite / Direct2D）
```

* **font-tuner.exe** — `RenderCore64.dll` が export する `GetMsgProc` を使うグローバルな `WH_GETMESSAGE` フックを 1 つ張る。64bit GUI プロセスがメッセージを取り出すと、Windows がコア DLL をそこにマップしてフックが発火し、コアの `DllMain` が走ってフォント描画 API をパッチする。
* **RenderCore64.dll** — アタッチ時にプロセスの寿命の間だけ自己をアンロード不可にする（`GetModuleHandleEx` + `GET_MODULE_HANDLE_EX_FLAG_PIN`）。フックを外しても（トレイ OFF / 終了 / 更新 / アンインストール）*新規*プロセスへの注入が止まるだけで、動作中プロセスからは決してアンマップされない。だから「アンマップ後にコードが走る」クラッシュ経路がない。プロファイル切替・ON/OFF・更新はすべて以降に起動するプロセスで効く。
* **フックプロシージャの RVA 固定** — `SetWindowsHookEx` は `GetMsgProc - hmod` しか記録しない。対象プロセスでは Windows が DLL をパスで解決し、そのプロセスが既に持つ（常駐固定された）イメージを見つけて `その base + RVA` を呼ぶ。更新後も動作中プロセスは*前*のビルドを保持するので、RVA がビルド間で同一でなければ、次のメッセージでそれらが一斉にランダムなバイトを実行して落ちる（一度実際に起きた: リファクタで `GetMsgProc` が `0x1100` から `0x8300` に動いた）。コアの `build.rs` はリンカの `/ORDER` で `GetMsgProc` を RVA `0x1000`（`.text` の先頭）に固定する。`build-msi.ps1` は動いたコアをパッケージせず、トレイは張らない（1.2）。バージョン資源は `.rsrc` に入るので RVA に影響しない（確認済み）。
* **子プロセスへの注入は移植しない** — upstream のコアは `CreateProcess` を横取りし、非公開のブートストラップ DLL（`expfunc.cpp` `GdippInjectDLL`）を子に送り込んで、メッセージポンプが回る前にコアをロードする。移植にはこの経路がない。メッセージポンプを持つ子プロセスなら `WH_GETMESSAGE` で届く（最初のメッセージを取り出す前の描画には効かない）。0.1.1 までは Rust 版のブートストラップ DLL を同梱していた。ロードする側が無いので 0.1.2 で外した。理由: 得られるのは起動直後の数フレームと、署名の壁（1.3）で届かない Chrome 系の子だけ。その代償として `CreateProcess` の detour が全プロセスで走る。

### 1.2 フックと並行性

* **仕組み** — GDI `ExtTextOutW` は **retour**（純 Rust、iced-x86 逆アセンブラ）のインライン detour でフックする。DirectWrite/Direct2D の入口は COM vtable を直接パッチする。MinHook/Detours は使わない。
* **スレッド安全なパッチ** — retour は対象の先頭バイトを書き換える間、他スレッドを止めない。そこで `install_hook` はパッチの前後でプロセス内の他スレッドを全部凍結し（`CreateToolhelp32Snapshot` + `SuspendThread`）、後で再開する。MinHook が内部で閉じている窓と同じ。
* **アタッチは一度だけ** — WH_GETMESSAGE のマップと別のロードで、DLL が 1 プロセス内に 2 つのモジュールインスタンスになりうる。プロセスごとの名前付きミューテックス（`Local\FontTuner.Attached.<pid>`）で最初のアタッチだけがフックするようにし、2 度目のアタッチが自分のジャンプの上に detour を張ってトランポリンを壊すのを防ぐ。
* **一度きりの vtable パッチ**（CreateAlphaTexture、Direct2D の全生成スロットとテキストスロット）はミューテックスで直列化し、その下で再確認する。さもないと競合する 2 スレッドが両方ともパッチ済みスロットから「元の関数」を捕まえ、detour が自分自身を呼ぶ → 無限再帰。Direct2D のスロットは (vtable, slot) を鍵にした 1 つのマップで管理する。レンダーターゲットのクラスごとに vtable が違うため。
* **Direct2D への到達** — `render-inject/src/d2d.rs` が upstream の生成チェーンを辿る。入口は `D2D1CreateFactory`・`D2D1CreateDevice`・`D2D1CreateDeviceContext` の 3 つ。`D2D1CreateFactory` からは `CreateHwnd/DC/WicBitmapRenderTarget` と `ID2D1Factory1..7::CreateDevice` に至る。`D2D1CreateDevice` からは `ID2D1Device..6::CreateDeviceContext` に至る。各ターゲットで `CreateCompatibleRenderTarget`（12）・`DrawGlyphRun`（29）・記述付きの overload（82）・`SetTextAntialiasMode`（34）・`SetTextRenderingParams`（36）をパッチする。12 はオフスクリーンのビットマップターゲットを生成時にフックするため。フォント処理の前に `GetDC` を試すので、DC を貸せないターゲットは試行以上のコストがかからない。GDI DC を貸せるターゲットはランを render-core で描く。貸せないターゲット（DXGI サーフェス: スワップチェーン、コンポジション）は OS に描かせる。その際、プロファイルの `[DirectWrite]` `IDWriteRenderingParams` と、`AntiAliasMode` から導いたアンチエイリアスモードを渡す。`HintingMode=1` のときは upstream と同じ 1/65535 の変換ずらしも加える。スロット番号は `windows` クレートの vtable 定義で照合した。
* **再入**はスレッドごとに（`thread_local`）ガードする。あるスレッドの描画が、別スレッドの描画を未調整の GDI 経路に落とすことはない。
* **トレイのフック設置ガード**（`Hook::install`、`src/stale.rs`）— `LoadLibraryW` の前に確認する。コアをロードするとトレイ自身に常駐固定とフックがかかるためだ。トレイは 2 点を見る。(a) ディスク上のファイルからコアの `GetMsgProc` が RVA `0x1000` にあること。(b) 同じパスのコアを `GetMsgProc` が別の場所にある状態で保持する動作中プロセスが無いこと。(b) の判定は Toolhelp のモジュール走査とそのイメージの export テーブルの `ReadProcessMemory` で行う。モジュールはあるがイメージを読めないプロセスは stale 扱いにする（その間に終了していれば除く）。別ディレクトリから読み込んだコピー（`loader` ハーネス）は問題ない。Windows はフック DLL をパスで解決し、インストール済みのものを別イメージとしてマップし、アタッチ一度きりミューテックスが 2 つ目を不活性にするからだ。どちらの確認が失敗してもエラーを出してフックを張らない。(b) ではメッセージが該当プログラムを列挙し、サインアウトして入り直す（または再起動する）よう促す。コアが常駐固定なので、それが stale なイメージを消す唯一の方法だからだ。トレイが開けないプロセス（別ユーザーやより高い整合性レベル）はトレイのフックも届かないので、飛ばしても安全。

### 1.3 届かないもの

* **Chrome/Edge のレンダラー・GPU プロセス** — `MITIGATION_FORCE_MS_SIGNED_BINS`（Microsoft 署名バイナリのみ）で阻まれる。未署名のコア DLL は `LoadLibrary` で拒否される。通常の子プロセス（crashpad-handler、utility）には届く。これは Windows のセキュリティ境界であってバグではない。回避策は Microsoft 署名（任意の DLL には得られない）か、ブラウザごとに緩和を無効化すること（`RendererCodeIntegrityEnabled=0` ポリシー、サンドボックスを弱めるので本プロジェクトでは設定しない）だけ。
* **メッセージポンプを持たないプロセス**（コンソールアプリ、サービス）— `WH_GETMESSAGE` が発火しない。
* **自前でテキストを描くアプリ**（Windows ターミナルの AtlasEngine など）— GDI / `IDWriteBitmapRenderTarget` / Direct2D のテキスト API を通らない描画には手が届かない。
* **32bit プロセス** — 対象外。64bit 専用ビルド。
* **より高い整合性レベルのプロセス** — UIPI が低整合性の Font-tuner からのフックメッセージを遮る。

### 1.4 コアの Win32 境界（`render-inject`）

`unsafe` は「OS の API を呼ぶ行」と「OS から受け取ったポインタを読む行」に限り、残りは安全な Rust で書く。境界ごとに何を信頼しているかを示す。

| 境界 | 使う API | 何を信頼するか | 守り |
|---|---|---|---|
| detour の設置（`hook.rs`） | `retour::RawDetour`、`VirtualProtect`、`CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD)` + `OpenThread` + `SuspendThread` / `ResumeThread` | 対象は `GetModuleHandleW` + `GetProcAddress` で得た gdi32 / d2d1 の export | パッチの前後で自スレッド以外を全部止める。スナップショットとスレッドハンドルは RAII で閉じる。作った detour は `static DETOURS: Mutex<Vec<RawDetour>>` に入れて解放しない（常駐固定なので、トランポリンが消えることはない） |
| `ExtTextOutW` の横取り（`gdi.rs`） | `GetTextMetricsW`、`GetTextExtentPoint32W` / `GetTextExtentPointI`、`GetTextColor` / `GetTextAlign` / `GetBkColor`、`GetCurrentObject` + `GetObjectW`（`LOGFONTW`）、`GetFontData` | 引数の `hdc`・`text`（`count` 要素）・`dx`（非 null なら `count` 要素）・`lprect` は呼び出し元の契約どおり | 測れないラン（`GetTextExtent` 失敗、`cx <= 0`）と読めないフォント（`GetFontData` 失敗）は元の `ExtTextOutW`（トランポリン）に落とす。オフスクリーン DIB（`dib.rs`: `CreateDIBSection` + `BitBlt` の往復）に描き、`ETO_OPAQUE` / `ETO_CLIPPED` / 背景モードは DIB 側で再現する（下記） |
| 再入 | `thread_local! IN_DETOUR: Cell<bool>` | 自分の GDI 呼び出しが自分の detour に入ることがある | スレッドごとにガードする。プロセス全体のフラグにすると、あるスレッドの描画中に他スレッドが未調整の GDI に落ちて窓ごとに見た目が違う |
| 描画状態 | `static RENDER: Mutex<Option<RenderState>>`（`Ft` + `Tables` + `Profile` + 現在のフォント鍵） | — | 1 プロセスに FreeType ライブラリと面は 1 つ。描画はロックの下で直列。プロファイル再読み込みも同じロック |
| DirectWrite（`dwrite.rs`） | `IDWriteBitmapRenderTarget::DrawGlyphRun`、`IDWriteFactory{,2,3}::CreateGlyphRunAnalysis` の vtable スロット | `windows` クレートの vtable 定義とスロット番号が一致すること（照合済み） | 一度きりのパッチはミューテックスで直列化。`IDWriteFontFace` の bytes + index で面を開き、その COM オブジェクトを `RenderState` が clone で保持してアドレスの再利用を防ぐ |
| Direct2D（`d2d.rs`） | `D2D1CreateFactory` / `D2D1CreateDevice` / `D2D1CreateDeviceContext` と各ターゲットの vtable スロット 12 / 29 / 82 / 34 / 36 | 同上 | (vtable, slot) → 元関数のマップ `SLOT_ORIG` を 1 つのミューテックスで管理。`GetDC` を貸せないターゲットは OS に描かせる |
| 自己常駐固定 | `GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN)` | — | `DllMain(DLL_PROCESS_ATTACH)` で最初に行う。以降 `FreeLibrary` は no-op |
| ログ | `%TEMP%\render-inject.log` に追記 | — | ロック付き。初回の描画結果を `render-inject-capture.png` に保存する（検証用） |

`DllMain` では常駐固定・ミューテックス取得・スレッド起動だけを行い、フックの設置と FreeType の初期化は別スレッド（`on_attach`）で行う。ローダーロックの下で detour を張らない。

**背景モード** — GDI は `SetBkMode(OPAQUE)`（既定）のとき、文字を描きながらその文字ボックスを背景色で塗る。同じ場所に値を描き直して更新するアプリ（Process Explorer の数値列）はこれに頼っている。render-core は既存のピクセルの上に合成するだけなので、コア側で塗りを再現する必要がある。`ETO_OPAQUE`（`lprect` を塗る）に加えて `GetBkMode(hdc) == OPAQUE` なら文字ボックス（`d.x` から幅ぶん、ベースライン - `tmAscent` から `tmHeight`）を背景色で塗ってから描く。幅は `dx` 配列があればその合計、無ければ `GetTextExtentPoint*` の実測。upstream も同じ判定（`override.cpp`: `fillrect || GetBkMode(hdc) == OPAQUE`）。これを見ていなかった 0.1.3 までは、Process Explorer の CPU 列などで古い数字が残って二重に見えた。

---

## 2. 描画コア（`render-core`）

コアは「文字 + プロファイル」をピクセルにする。FreeType（snowie2000 のフォーク、`build/lib/freetype64.lib`）がアウトラインをカバレッジにし、コアがヒンティングの指定・ガンマ・コントラスト・ブレンドを受け持つ。upstream `ft.cpp` の `FreeTypePrepare` と `CAlphaBlend` の移植。

### 2.1 プロファイルのキー

コアが `.ini` から読むキー（`render-core/src/config.rs`）。ほかのキーは読み飛ばす。プロセス別の節（`[Experimental@idea64.exe]` など）は upstream だけが読む。コアにプロセス別設定はない。

| 節 / キー | 値 | 既定（キーが無いとき） |
|---|---|---|
| `[General] HintingMode` | 0 フォント内蔵 / 1 なし / 2 オートヒント（2.2） | 0 |
| `[General] AntiAliasMode` | 0 グレースケール / 2 LCD RGB / 3 LCD BGR / 4 LightLCD RGB / 5 LightLCD BGR（2.2） | 0 |
| `[General] LcdFilter` | 0 なし / 1 標準 / 2 ライト / 3 レガシー（`FT_LCD_FILTER_*`） | 0 |
| `[General] GammaMode` | 0 べき乗 / 1 sRGB / 2 平均 / 負 線形（2.4） | 0 |
| `[General] GammaValue` | べき乗ガンマの指数 | 1.25 |
| `[General] Contrast` | カバレッジ曲線の指数（2.4） | 1.0 |
| `[General] RenderWeight` | カバレッジ曲線の重み（2.4） | 1.0 |
| `[General] NormalWeight` | アウトラインの太字化（26.6 固定小数。64 = 1px） | 0 |
| `[DirectWrite] GammaValue` `Contrast` `ClearTypeLevel` `RenderingMode` | 自前でラスタライズできない Direct2D 描画に渡す `IDWriteRenderingParams`（1.2） | 2.5 |
| `[Experimental] ClipBoxFix` | `GetGlyphOutline` のメトリクス補正（2.5） | 1 |

ini が読めないときは組み込みの Clean Greyscale（`Profile::clean_greyscale`、出荷 ini と同じ値）に落ちる。

### 2.2 ヒンティングとロードターゲット

FreeType のロードフラグは upstream `FreeTypePrepare` と同じ対応（`render-core/src/ft.rs` の `flags`。テストで固定）。

| 指定 | フラグ | 意味 |
|---|---|---|
| `HintingMode=0` | （なし） | FreeType の既定。フォント内蔵の TrueType バイトコードがあればそれでヒントする |
| `HintingMode=1` | `FT_LOAD_NO_HINTING` | アウトラインのまま。最も柔らかく最も忠実な形 |
| `HintingMode=2` | `FT_LOAD_FORCE_AUTOHINT` | FreeType のオートヒンタ |
| `AntiAliasMode=0` | `FT_LOAD_TARGET_NORMAL` + `FT_RENDER_MODE_NORMAL` | グレースケール |
| `AntiAliasMode=2/3` | `FT_LOAD_TARGET_LCD` + `FT_RENDER_MODE_LCD` | LCD サブピクセル |
| `AntiAliasMode=4/5` | `FT_LOAD_TARGET_LIGHT` + `FT_RENDER_MODE_LCD` | 縦方向だけスナップする軽いオートヒントで、描画は LCD |

TrueType インタープリタは FreeType の既定（v40）。upstream は `INFINALITY` 定義時に v38 を要求するが、同梱フォーク（FreeType 2.14）は v38 を v40 に丸めるので結果は同じ。

**観察**（`render-core` を直接呼んで HintingMode 0/1/2 を並べ、8 倍に拡大して比較した。実機の事実）:

* 自前の TrueType ヒントを持つフォント（Yu Gothic）: グレースケール × オートヒントで、12px 前後の欧文の大文字の高さが字ごとにずれる（C や 3 が大きく、Z が小さい）。LCD 系 × オートヒントでは揃う。
* 欧文にヒントを持たないフォント（BIZ UDPゴシック。インタープリタ v35 と v40 で出力が同一、0 と 1 がほぼ同じ絵）: フォント内蔵（0）は事実上ヒントなし。Z の上端が半ピクセルにかかって灰色の行になり、B や I より低く見える。素の GDI（ClearType）は縦方向にアンチエイリアスしないので目立たない。オートヒントなら揃う。
* この見え方はブレンドの計算（2.4）とは無関係。f32 と固定小数点の違いは各ピクセルの濃さを最大 1 階調変えるだけで、形は変えない。

### 2.3 フォントの解決

注入したコアは各 DC のフォントを `GetFontData` で丸ごと読み（TTC は `'ttcf'` テーブルで判別）、FreeType にメモリ面として渡す。ディスク上のファイル名は使わない。カスタムダイアログのプレビュー（3.4）も同じ経路。

TTC の中のフェイスは GDI の顔名（`LOGFONTW.lfFaceName`）で選ぶ（`Ft::reface_memory`）。FreeType の ASCII の `family_name` に加えて、`name` テーブルの Windows プラットフォーム項目（ID 1 / 16 / 21、UTF-16BE）も比べる。GDI が渡す顔名は日本語（「BIZ UDPゴシック」「游ゴシック」）なので、ASCII 名だけだと一致せずフェイス 0 に落ちる。BIZ UD では 0 が等幅の BIZ UDGothic で、欧文が等幅に並んでいた（実機で確認、修正済み。テストで日本語名 → プロポーショナル、ASCII 名 → 等幅を固定）。どの名前にも一致しなければフェイス 0。

ピクセルサイズ（em）は upstream と同じく `GetTextMetricsW` の `tmHeight - tmInternalLeading`（`gdi.rs` の `em_px`）。`LOGFONTW.lfHeight` は負なら em、正ならセル高さ（em + 内部レディング）なので絶対値は使えない。0.1.1 までは絶対値を使っていて、`CreateFont(16, ...)` のような正の高さのフォントが内部レディングの分（Yu Gothic UI で 16 → 本来 12px のところ 16px）大きく描かれていた。フック経由で正負両方の高さを描いて修正を確認した。

### 2.4 ブレンド計算式

upstream `CAlphaBlend` と同じ線形空間のアルファブレンドで FreeType のカバレッジをピクセルに変える。これが仕様で、`render-core/src/filter.rs` が `f32` で実装する。バイト値は `x = v/255` で正規化する。

**ガンマ符号化** `g(x)`（バイト → 線形光）。`GammaMode` で選ぶ:

```
g(x) = x                                  GammaMode < 0   (線形)
     = srgb(x)                            GammaMode = 1   (sRGB)
     = (srgb(x) + x) / 2                  GammaMode = 2   (sRGB と線形の平均)
     = x ^ GammaValue                     それ以外        (べき乗ガンマ)

srgb(x) = x / 12.92                       x <= 10/255
        = ((x + 0.055) / 1.055) ^ 2.4     それ以外
```

**カバレッジ曲線** `a(cov)`（FreeType カバレッジ → アルファ）。`RenderWeight` と `Contrast` による S 字で、`t = (cov/255) ^ (1/RenderWeight)`:

```
a(cov) = (2t) ^ Contrast / 2              t < 0.5
       = 1 - (2(1 - t)) ^ Contrast / 2    t >= 0.5
```

**ブレンド**。前景 `fg` を背景 `bg` にカバレッジ `cov` で合成:

```
out = g⁻¹( g(bg)·(1 - a(cov)) + g(fg)·a(cov) )
```

両色を線形光に変換し、カバレッジのアルファで補間し、戻す。`g⁻¹` は `g` の数値的な逆関数（プロファイルごとの符号化テーブルの二分探索、閉じた式のない平均モードを含め、全 `GammaMode` を逆変換する）。各チャンネルは独立にブレンドするので、LCD サブピクセルのカバレッジは R/G/B へ別々に入る。

upstream はこれを固定小数点の整数で計算し、最後の段で切り捨てる。計算式を `f32` で計算して最も近いバイトへ丸める移植は、upstream と最大 1 階調しか違わない。計算式が正しさの基準で、実装はそれに対して検証する（`cargo test`: 端点、単調性、全 `GammaMode`、gamma 1.25 の回帰値）。

### 2.5 DirectWrite 節と ClipBoxFix

`[DirectWrite]`（`GammaValue`・`Contrast`・`ClearTypeLevel`・`RenderingMode`）は、自前でラスタライズできないテキストに対して Direct2D へ指定する値（1.2）。既定は upstream に従う: gamma は一般の gamma から導出（`g² > 1.3 ? g²/2 : 0.7`）、contrast 1.0、ClearType level 1.0、mode 5。`GammaValue` が 0（グレースケール系プロファイルの出荷値）のときは「上書きしない」の意味で、導出 gamma にフォールバックする（DirectWrite は gamma > 0 を要求するため）。出荷プロファイルは全て `RenderingMode=2`（GDI_CLASSIC）で、GDI と DirectWrite のテキストを一致させる。

`[Experimental] ClipBoxFix`（既定 1）は、メトリクスのみの問い合わせで `GetGlyphOutline` が返すメトリクスを補正する。原点を `floor(1.5·DPI/96)` px 上げ、黒箱を同じだけ広げ、どちらもフォントの ascent/height で頭打ちにする。これで、そのメトリクスにグリフをクリップするアプリ（Java2D）が、太めに描かれたグリフを切り落とさない。

### 2.6 FreeType の FFI 境界

FreeType は C のヘッダを bindgen せず、使う分だけ手で宣言する（`render-core/src/ft.rs` の `mod sys`）。

* **関数**（14 個）:
  * ライブラリと面: `FT_Init_FreeType` / `FT_Done_FreeType`、`FT_New_Face` / `FT_New_Memory_Face` / `FT_Done_Face`
  * 文字とサイズ: `FT_Select_Charmap`、`FT_Set_Pixel_Sizes`、`FT_Get_Char_Index`
  * 描画: `FT_Load_Glyph`、`FT_Outline_EmboldenXY`、`FT_Render_Glyph`、`FT_Library_SetLcdFilter`
  * 名前: `FT_Get_Sfnt_Name_Count` / `FT_Get_Sfnt_Name`
  * 定数（`FT_LOAD_*`、`FT_RENDER_MODE_*`、`FT_PIXEL_MODE_*`）は公開 ABI の値をそのまま書く
* **構造体**: `FT_FaceRec`（`num_faces`、`family_name`、`glyph` まで）、`FT_GlyphSlotRec`（`metrics`、`advance`、`format`、`bitmap`、`bitmap_left` / `bitmap_top`、`outline`）。ほかに `FT_Bitmap`、`FT_Outline`、`FT_SfntName`。`#[repr(C)]` で必要なフィールドまで宣言する。x64 Windows（`long` = 4 バイト）のオフセットをフォークのヘッダから導き、`offset_of!` / `size_of` のテスト（`layout_tests`）で固定する。フォークを更新してレイアウトが変わればテストが落ちる。
* **所有**: `Ft` が `FT_Library` と現在の `FT_Face` を持ち、`Drop` で `FT_Done_Face` → `FT_Done_FreeType` の順に閉じる。面を差し替える `reface_*` は先に古い面を閉じる。メモリ面のバイト列は `Ft` の `UnsafeCell<Vec<u8>>` に置き、FreeType が参照している間は差し替えない（面を閉じてから入れ替える）。
* **借用**: `render` が返す `Glyph<'_>` はグリフスロットのビットマップを借りる。次の `FT_Load_Glyph` で上書きされるので、`&self` の寿命に縛って「描いてから次の文字」を型で強制する。
* **`unsafe` の範囲**: FreeType を呼ぶ行と、返ってきた `*mut` を読む行だけ。`name` テーブルの UTF-16BE 復号や名前比較、カバレッジの合成は安全な Rust。
* **upstream との違い**: upstream は `FTC_Manager`（FreeType のキャッシュ）を使う。移植は面 1 つを持ち、フォントが変わるたびに `GetFontData` で読み直す（`font_key` が同じなら読み直さない）。

---

## 3. プロファイル

### 3.1 選択と反映

プロファイルは `profiles/ini/*.ini`（インストール先の `ini\`）にある。有効なものは `font-tuner.ini` の `[General] AlternativeFile=` で選ぶ。相対パス（`ini\<名前>.ini`）は出荷プロファイル、絶対パスはカスタム（3.4）。コアは `dir.join(値)` で解決し、Rust の `Path::join` は右辺が絶対パスならそれをそのまま返す。

注入されたコアはこのキーをアタッチ時に一度だけ読む。だから切替は以降に起動するプロセスで現れる。動作中プロセスを更新するには、トレイの「プロファイルを再読み込み」を使う。登録メッセージ（`FontTuner.ReloadProfile`）をブロードキャストし、各コアはそれを自分の `GetMsgProc` で受け（すでにそのプロセスの UI スレッド上）、描画ロックの下で `font-tuner.ini` を読み直す。監視スレッドはなく、DLL が消えた後に走るものもない。

### 3.2 出荷プロファイル

| プロファイル | ヒンティング | アンチエイリアス | 性格 |
|---|---|---|---|
| **Clean Greyscale** *(既定)* | 2（オートヒント） | 0 グレースケール | 中庸で柔らかく、色にじみなし。 |
| **Clean Dark Greyscale** | 0（フォント内蔵） | 0 グレースケール | 暗い背景向け（gamma 1.1、contrast 0.9、やや太め）。 |
| **Accurate** | 2（オートヒント） | 4 LightLCD | 縦方向だけの軽いオートヒント + LCD。小さい/UI サイズで最も鮮鋭。 |
| **Clean Sharp** | 1（なし） | 2 LCD | サブピクセル LCD、ヒンティングなし。横方向の細部が高く鮮鋭。 |
| **Clean Sharp Dark** | 0（フォント内蔵） | 2 LCD | 暗い背景向けの LCD。 |

既定をオートヒントにした理由: 2.2 の観察のとおり、欧文にヒントを持たないフォント（BIZ UD など）で大文字の上端が揃うのはオートヒントだけ。Yu Gothic ではオートヒントで大文字の高さがずれるが、Yu Gothic を嫌う人はシステムフォントをトレイで替える（4）ので、替えた先で揃う方を既定にした。

### 3.3 メニュー順

トレイはプロファイルを固定の優先順（`src/main.rs` の `ORDER`）で並べ、アルファベット順にはしない: グレースケールの 2 つ → Accurate → LCD の「Clean Sharp」系。一覧にないプロファイルはその後にアルファベット順で入るので、`.ini` を足せばコード変更なしで表示される。その下に区切りを挟んで「カスタム」と「カスタムを調整...」が並ぶ。

### 3.4 カスタムプロファイル

「カスタムを調整...」（`src/custom.rs`）は、コアが読む `[General]` の 8 キー（2.1）をコンボ / スライダーで変えるダイアログを開く。純 Win32（`STATIC` / `COMBOBOX` / `msctls_trackbar32` / `EDIT` / `BUTTON`）で、GUI フレームワークは使わない。だからダイアログ自身のラベルも GDI 経由でコアが描き、実描画の確認に使える。

| キー | 選択肢 / 範囲 |
|---|---|
| `HintingMode` | 0 フォント内蔵 / 1 なし / 2 オートヒント |
| `AntiAliasMode` | 0 グレースケール / 2 LCD RGB / 3 LCD BGR / 4 LightLCD RGB / 5 LightLCD BGR |
| `LcdFilter` | 0 なし / 1 標準 / 2 ライト / 3 レガシー |
| `GammaMode` | 0 べき乗 / 1 sRGB / 2 平均 / -1 線形 |
| `GammaValue` | 0.50〜3.00 |
| `Contrast` | 0.50〜2.50 |
| `RenderWeight` | 0.50〜2.50 |
| `NormalWeight` | -16〜48（26.6 固定小数） |

スライダーの範囲は非常識な値（潰れる / 消える）にならない幅で切ってある。`--custom` 引数で起動すると同じダイアログを直接開く。初期値は `Custom.ini` があればそれ、なければ現在選択中のプロファイル。「現在のプロファイルから複製」で選択中の値を読み直せる。

プレビューはトレイ自身が `render-core` をリンクして描く（注入なし）。フォントは `GetFontData` で GDI から取り出して FreeType にメモリ面として渡す（2.3）。プレビュー用のフォントは「フォント...」（`ChooseFont`）で替えられ、既定はシステムのメッセージフォント。トレイの exe はマニフェストで PerMonitorV2 の DPI 対応を宣言しているので、プレビューはビットマップ拡大されず 1:1 で出る。

「適用」は `%APPDATA%\Font-tuner\Custom.ini` を書き、`font-tuner.ini` の `AlternativeFile=` にその**絶対パス**を入れ、「プロファイルを再読み込み」と同じブロードキャストを送り、全ウィンドウを無効化して再描画させる。`RenderCore64.dll` はこの機能のために変えていない。

結果として起きること:

* ファイルを書くのはトレイだけ。コアは読むだけ（従来どおり）。
* `Custom.ini` が読めないプロセス（ファイルを消した、別ユーザーの `%APPDATA%` を指している）は、従来と同じく組み込みの Clean Greyscale に落ちる。落ちる（クラッシュする）経路はない。
* `font-tuner.ini` は全ユーザー共通なので、あるユーザーがカスタムを選ぶと、別ユーザーのプロセスは他人の `%APPDATA%` を読めず既定に落ちる。共有 PC ではそのユーザーが自分のプロファイルを選び直す。
* メニューの「カスタム」は `Custom.ini` があるときだけ選べる（無ければグレー）。
* `Custom.ini` はアンインストールでも消えない（`%APPDATA%` はインストーラの管理外）。

実装上の注意: ダイアログの状態は `thread_local` の `RefCell` に置く。Win32 のメッセージは `ShowWindow` / `SetWindowText` の最中に同期的に再入する（`WM_CTLCOLORSTATIC` など）ので、借用中に届くメッセージのハンドラで再度借りると二重借用で panic し、`panic=abort` のためトレイごと落ちる（一度実際に起きた）。再入しうるハンドラで要る値は `RefCell` の外（`Cell`）に置く。

---

## 4. トレイ

`font-tuner.exe`。単一インスタンス（`Local\font-tuner` ミューテックス）。メッセージ専用ウィンドウ 1 つと通知アイコン。起動時にフックを張る（1.2 のガードを通る）。

メニュー:

| 項目 | 動作 |
|---|---|
| 有効 | フックの ON/OFF。OFF は新規プロセスへの注入を止めるだけ（1.1） |
| プロファイル ▶ | 3.3 の順で並ぶ。選ぶと `AlternativeFile=` を書く（`WritePrivateProfileString`）。「カスタム」「カスタムを調整...」 |
| プロファイルを再読み込み | `FontTuner.ReloadProfile` をブロードキャスト（3.1） |
| システムフォント ▶ | 5 |
| バージョン x.y.z | About。バージョン・ライセンス（GPL-3.0-only）・ソース URL・必須の FreeType クレジット |
| Font-tuner を再起動 | 「終了」と同じ経路（`WM_DESTROY` でフック解除・アイコン削除）で抜けた後、`main` の末尾で単一インスタンスのミューテックスを閉じてから同じ exe を起動し直す。手で終了 → 起動するのと同じで、コアには何もしない（メニューからの実操作を確認済み） |
| 終了 | フックを外して終了 |

アイコンは 2 つを `app.rc` / `build.rs`（`embed-resource`）で exe に埋め込む: `assets/tray-dark.ico`（シルバーの歯車 + 「A」、暗いタスクバー用）と `assets/tray-light.ico`（黒、明るいタスクバー用）。起動時と `WM_SETTINGCHANGE` のたびに `HKCU\...\Themes\Personalize\SystemUsesLightTheme` を読んで一致する方を選ぶ。値が無ければ暗（Windows 11 の既定）。図案は CC0。Explorer が再起動したら（`TaskbarCreated`）アイコンを置き直す。

トレイ側の Win32 境界:

* `windows` クレート（0.62）を使い、`unsafe` は API 呼び出しの行に限る。ウィンドウプロシージャ（`main.rs`、`custom.rs`）は `unsafe extern "system"` で、状態は `thread_local!` の `RefCell<Option<App>>` / `RefCell<Option<Dlg>>` に置き `with_app` / `with_dlg` で借りる。
* Win32 のメッセージは同期的に再入する。`MessageBoxW` や `ChooseFontW` のモーダルループ、`ShowWindow` / `SetWindowTextW` / `DestroyWindow` の中から同じウィンドウプロシージャが呼ばれる。借用中に再入するとRefCell の二重借用で panic し、`panic=abort` でトレイごと落ちる（カスタムダイアログの注意文で一度起きた）。だからモーダルを開く前に借用を返す（`set_enabled` はエラー文字列を返して呼び出し元が `MessageBoxW` を出す、`ChooseFont` の間は `Dlg` を `take()` して外に出す）。再入するハンドラで要る値は `Cell` に置く。
* フック設置（`stale.rs`）: `CreateToolhelp32Snapshot` でプロセスとモジュールを列挙し、`OpenProcess` + `ReadProcessMemory` でコアの PE ヘッダと export テーブルを読んで `GetMsgProc` の RVA を比べる。ハンドルは RAII で閉じる。
* `SetWindowsHookExW` に渡すプロシージャは、コア DLL を `LoadLibraryW` して `GetProcAddress` で得る（`hmod + 0x1000` を仮定しない）。
* `font-tuner.ini` の読み書きは `GetPrivateProfileStringW` / `WritePrivateProfileStringW`。ANSI 版は使わない。
* システムフォント（`sysfont.rs`）: `NONCLIENTMETRICSW` の `cbSize` は自分で埋め、バックアップから読んだ値は信頼しない（5）。

`font-tuner.ini` の `[UnloadDll]` 節は「調整を効かせないプログラム」の一覧（upstream の書式）。コアはアタッチ時に自分の exe 名をこの一覧と照合し、載っていればフックを張らずに戻る（`profile.rs` の `is_process_excluded`）。常駐固定は `DllMain` で先に済んでいるので DLL 自体はマップされたままだが、以降そのプロセスでコアのコードは走らず、描画は素の GDI になる。一覧の編集は以降に起動するプロセスから効く。upstream はここに自分のトレイを載せるが、出荷の一覧からは `font-tuner.exe` を外してある。トレイのメニューとダイアログの文字をコアが描くと、フェイスやサイズの取り違えがその場で見えるため（TTC のフェイス選択の不具合はそれで見つけた）。自己フックで問題が起きた事例はない。

---

## 5. システムフォント切替

トレイのサブメニュー「システムフォント」（`src/sysfont.rs`）。シェルの UI フォント（caption、small-caption、menu、status、message、icon-title）を `SystemParametersInfo`（`SPI_SET{NONCLIENTMETRICS,ICONTITLELOGFONT}`）で入れ替える。ユーザーごとに永続する設定で、`WM_SETTINGCHANGE` でブロードキャストされる。

* 固定の候補: `BIZ UDPゴシック`、`BIZ UDゴシック`、`Noto Sans JP`、`メイリオ`。
* 初回の変更時に元のフォント一式を `%LOCALAPPDATA%\font-tuner\sysfont-backup.bin` に保存する。「既定に戻す」がそれを復元し、バックアップを消す。
* 復元時、（ユーザーが書き換えられる）バックアップファイルの `cbSize` は信頼しない。本物の構造体サイズに強制するので、`SystemParametersInfo` がバッファの外を読むことはない。

新しく描かれる UI には即座に適用される。シェルはサインアウト/インの後に完全に反映する。

---

## 6. ビルド

* **版** — root `Cargo.toml` の `[workspace.package] version` が唯一の出所。`font-tuner` は `version.workspace = true` で継承し、ワークスペース外の `render-inject` は `build.rs` が root `Cargo.toml` を読む。exe とコア DLL は各 `build.rs` が生成する `VERSIONINFO` を埋め込む。リリースごとに上げる（7）。
* **ツールフラグ** — `.cargo/config.toml` が MSVC ターゲットに `+crt-static` を設定し、`VCRUNTIME140.dll` 依存（と DLL 探索順ハイジャックの面）を消す。Release プロファイル: `opt-level="s"`、LTO、`panic="abort"`、strip 済み。
* **`build-core.ps1`** — 出荷 DLL に必要な唯一のネイティブ依存だけをビルドする: snowie2000 の FreeType フォーク（`freetype64.lib`）を MSBuild/vswhere で。
* **`build-msi.ps1`** — `build-core.ps1`、`cargo build --release`（ワークスペース + `render-inject`）を走らせる。`check-export-rva.ps1` でコアが `GetMsgProc` を RVA `0x1000` に export しているか、exe とコア DLL のファイルバージョンが `Cargo.toml` の版と一致するかを確認する（違えば中止）。次に exe + DLL 群 + `font-tuner.ini` + `ini\*.ini` を `build\pkg` に集める。最後に `wix build` で `dist\font-tuner-<ver>-x64.msi`。
* **検証** — `check-lint.ps1`（PSScriptAnalyzer / textlint + prh / `cargo clippy`）と `cargo test`（`render-core` は 23 件。ブレンド、フラグ、TTC の名前一致、FFI 構造体のオフセット）。

---

## 7. インストーラ（MSI、WiX v6）

* **スコープ** perMachine、`C:\Program Files\Font-tuner` に入れる。スタートメニューのショートカットと `HKLM\...\CurrentVersion\Run\Font-tuner`（ログオン時に起動）を書く。
* **インストール時** — 動作中の `font-tuner.exe` を止め、Font-tuner を起動する。
* **Restart Manager 無効化**（`MSIRESTARTMANAGERCONTROL=Disable`、`REBOOT=ReallySuppress`）: `RenderCore64.dll` は全 GUI プロセスにマップされている。無効化しないと Restart Manager がそれらを全部閉じる（ユーザーのシェルを落としたことがある）。閉じるのはトレイだけにする。手で `msiexec` を打つときも `MSIRESTARTMANAGERCONTROL=Disable` を付ける。
* **使用中コアの入れ替え** — コアが全動作中プロセスにマップ（かつ常駐固定）されているため、そのファイルは決して上書きできない。遅延カスタムアクション（`RenameOldCore`、`InstallInitialize` の直後、`RemoveExistingProducts` の前）が使用中の `RenderCore64.dll` を脇へリネームし、`InstallFiles` が新しいものをすぐ置ける。脇へリネームしたコピーは次の再起動時の削除に予約する（`MoveFileEx DELAY_UNTIL_REBOOT`）。以降に起動するプロセスへ更新を効かせるのに再起動は要らない。脇へリネームしたイメージは元のパスのまま全動作中プロセスにマップされ続ける。だから新コアで `GetMsgProc` の RVA を同じに保つ必要がある（1.1）。
* **ファイルの置換規則** — Windows Installer は版付きのファイルを「新しい版が高いときだけ」置き換える。版なしのファイルは、更新日時が作成日時と違うと「利用者が改変した」とみなして置き換えない（[File Versioning Rules](https://learn.microsoft.com/en-us/windows/win32/msi/file-versioning-rules)）。0.1.0 は版なしで、更新で `font-tuner.exe` が古いまま残った（ログに `Existing file is unversioned but modified`）。0.1.1 から exe と DLL に版を埋め込み、版なし → 0.1.1、0.1.0 → 0.1.1 の更新でいずれも置き換わることを実機で確認した。版を上げずに作り直した MSI は同じ版のファイルを置き換えない（`Existing file is of an equal version`）。`REINSTALL=ALL` も効かない（`wix build` のたびに ProductCode が変わり、未インストールの製品扱いで 1603）。開発中に同じ版で入れ直すときは、アンインストール（`msiexec /x {ProductCode}`。新しい MSI ファイルを `/x` に渡しても 1605）してから入れる。
* **`font-tuner.ini`** は `NeverOverwrite`。版を上げた通常の更新ではユーザーの `AlternativeFile` が残る（0.1.1 → 0.1.2 で Accurate を選んだまま更新し、残ることを確認）。アンインストール → インストールでは消えて既定に戻る（確認済み）。
* **アンインストール** — 「プログラムの追加と削除」または `msiexec /x {ProductCode}`。ファイル・Run レジストリ値を消し、トレイを止める。`%APPDATA%\Font-tuner\Custom.ini` と `%LOCALAPPDATA%\font-tuner\sysfont-backup.bin` は残る。
* **署名** — MSI とそのペイロードは未署名なので、インストール時に UAC が「発行元不明」と出す（SmartScreen も出うる）。ブロックはされない。署名はリリースを帰属不能に保つためあえて省く。どのみちブラウザのレンダラー/GPU プロセスへ到達する助けにならない（1.3）。

---

## 8. ライセンス

GPL-3.0-only。`vendor/` の FreeType フォークをそれぞれのライセンスで同梱する。トレイアイコンの図案は CC0。
