# Font-tuner 仕様

Windows 向けの小さな自己完結型フォント描画チューナ（トレイ + 注入する描画コア）。上流は https://github.com/snowie2000/mactype にある。上流の非公開トレイが「トレイモード」でやることを再現し、ソースからビルドした描画コア DLL を同梱し、描画プロファイル一式を束ね、トレイメニューのシステムフォント切替を足す。Windows 11、64bit 専用。

---

## 1. アーキテクチャ

### 1.1 注入の流れ

```
font-tuner.exe ──(SetWindowsHookExW WH_GETMESSAGE, グローバル)──▶ 全 64bit GUI プロセス
                                                          RenderCore64.dll をマップ
                                                          （フックプロシージャがそこにある）
                          コアの DllMain がフォント API をフック（GDI / DirectWrite / Direct2D）
                          │
                          └─ 子プロセス生成時、コアが RenderBootstrap64.dll を注入
                             （GdippInjectDLL）。子の中でコアを LoadLibraryW する
```

* **font-tuner.exe** — トレイプロセス。`RenderCore64.dll` が export するプロシージャを使うグローバルな `WH_GETMESSAGE` フックを 1 つ張る。64bit GUI プロセスがメッセージを取り出すと、Windows がコア DLL をそこにマップしてフックが発火し、コアの `DllMain` が走ってフォント描画 API をパッチする。
* **RenderCore64.dll** — 描画エンジン（上流コアの Rust 移植）。同梱の FreeTypeフォークで実際にグリフを描画する。アタッチ時にプロセスの寿命の間だけ自己をアンロード不可にする（`GetModuleHandleEx` + `GET_MODULE_HANDLE_EX_FLAG_PIN`）。フックを外しても（トレイ OFF / 終了 / 更新 / アンインストール）*新規*プロセスへの注入が止まるだけで、動作中プロセスからは決してアンマップされない。だから「アンマップ後にコードが走る」クラッシュ経路がない。プロファイル切替・ON/OFF・更新はすべて以降に起動するプロセスで効く。
* **フックプロシージャの RVA 固定** — `SetWindowsHookEx` は `GetMsgProc - hmod`しか記録しない。対象プロセスでは Windows が DLL をパスで解決し、そのプロセスが既に持つ（常駐固定された）イメージを見つけて `その base + RVA` を呼ぶ。更新後も動作中プロセスは*前*のビルドを保持するので、RVA がビルド間で同一でなければ、次のメッセージでそれらが一斉にランダムなバイトを実行して落ちる（一度実際に起きた: リファクタで `GetMsgProc` が `0x1100` から `0x8300` に動いた）。コアの`build.rs` はリンカの `/ORDER` で `GetMsgProc` を RVA `0x1000`（`.text` の先頭）に固定する。`build-msi.ps1` は動いたコアをパッケージせず、トレイは張らない（1.2）。
* **RenderBootstrap64.dll** — 非公開ブートストラップの小さな Rust 代替（`bootstrap/`、クレート `render-bootstrap`、出力名 `RenderBootstrap64`）。コアが生成直後の子プロセスに注入する。役割はバックグラウンドスレッドからコアを`LoadLibraryW` することだけ。デッドロックを避けるため `DllMain` から`CreateThread` する（ローダーロックの下で `LoadLibrary` しない）。

### 1.2 フックと並行性

* **仕組み** — GDI `ExtTextOutW` は **retour**（純 Rust、iced-x86 逆アセンブラ）のインライン detour でフックする。DirectWrite/Direct2D の入口は COM vtable を直接パッチする。MinHook/Detours は使わない。
* **スレッド安全なパッチ** — retour は対象の先頭バイトを書き換える間、他スレッドを止めない。そこで `install_hook` はパッチの前後でプロセス内の他スレッドを全部凍結し（`CreateToolhelp32Snapshot` + `SuspendThread`）、後で再開する。MinHook が内部で閉じている窓と同じ。
* **アタッチは一度だけ** — WH_GETMESSAGE のマップと別のロードで、DLL が 1 プロセス内に 2 つのモジュールインスタンスになりうる。プロセスごとの名前付きミューテックス（`Local\FontTuner.Attached.<pid>`）で最初のアタッチだけがフックするようにし、2 度目のアタッチが自分のジャンプの上に detour を張ってトランポリンを壊すのを防ぐ。
* **一度きりの vtable パッチ**（CreateAlphaTexture、Direct2D の全生成スロットとテキストスロット）はミューテックスで直列化し、その下で再確認する。さもないと競合する 2 スレッドが両方ともパッチ済みスロットから「元の関数」を捕まえ、detourが自分自身を呼ぶ → 無限再帰。Direct2D のスロットは (vtable, slot) を鍵にした 1 つのマップで管理する。レンダーターゲットのクラスごとに vtable が違うため。
* **Direct2D への到達** — `render-inject/src/d2d.rs` が上流の生成チェーンを辿る。入口は `D2D1CreateFactory`・`D2D1CreateDevice`・`D2D1CreateDeviceContext` の 3 つ。`D2D1CreateFactory` からは `CreateHwnd/DC/WicBitmapRenderTarget` と `ID2D1Factory1..7::CreateDevice` に至る。`D2D1CreateDevice` からは `ID2D1Device..6::CreateDeviceContext` に至る。各ターゲットで `CreateCompatibleRenderTarget`（12）・`DrawGlyphRun`（29）・記述付きの overload（82）・`SetTextAntialiasMode`（34）・`SetTextRenderingParams`（36）をパッチする。12 はオフスクリーンのビットマップターゲットを生成時にフックするため。フォント処理の前に `GetDC` を試すので、DC を貸せないターゲットは試行以上のコストがかからない。GDI DC を貸せるターゲットはランを render-core で描く。貸せないターゲット（DXGI サーフェス: スワップチェーン、コンポジション）は OS に描かせる。その際、プロファイルの `[DirectWrite]` `IDWriteRenderingParams` と、`AntiAliasMode` から導いたアンチエイリアスモードを渡す。`HintingMode=1` のときは上流と同じ 1/65535 の変換ずらしも加える。スロット番号は `windows` クレートの vtable 定義で照合した。
* **再入**はスレッドごとに（`thread_local`）ガードする。あるスレッドの描画が、別スレッドの描画を未調整の GDI 経路に落とすことはない。
* **トレイのフック設置ガード**（`Hook::install`、`src/stale.rs`）— `LoadLibraryW` の前に確認する。コアをロードするとトレイ自身に常駐固定とフックがかかるためだ。トレイは 2 点を見る。(a) ディスク上のファイルからコアの `GetMsgProc` が RVA `0x1000` にあること。(b) 同じパスのコアを `GetMsgProc` が別の場所にある状態で保持する動作中プロセスが無いこと。(b) の判定は Toolhelp のモジュール走査とそのイメージの export テーブルの `ReadProcessMemory` で行う。モジュールはあるがイメージを読めないプロセスは stale 扱いにする（その間に終了していれば除く）。別ディレクトリから読み込んだコピー（`loader` ハーネス）は問題ない。Windows はフック DLL をパスで解決し、インストール済みのものを別イメージとしてマップし、アタッチ一度きりミューテックスが 2 つ目を不活性にするからだ。どちらの確認が失敗してもエラーを出してフックを張らない。(b) ではメッセージが該当プログラムを列挙し、サインアウトして入り直す（または再起動する）よう促す。コアが常駐固定なので、それが stale なイメージを消す唯一の方法だからだ。トレイが開けないプロセス（別ユーザーやより高い整合性レベル）はトレイのフックも届かないので、飛ばしても安全。

### 1.3 届かないもの

* **Chrome/Edge のレンダラー・GPU プロセス** — `MITIGATION_FORCE_MS_SIGNED_BINS`（Microsoft 署名バイナリのみ）で阻まれる。未署名のコア/ブートストラップ DLL は`LoadLibrary` で拒否される。通常の子プロセス（crashpad-handler、utility）には届く。これは Windows のセキュリティ境界であってバグではない。回避策はMicrosoft 署名（任意の DLL には得られない）か、ブラウザごとに緩和を無効化すること（`RendererCodeIntegrityEnabled=0` ポリシー、サンドボックスを弱めるので本プロジェクトでは設定しない）だけ。
* **メッセージポンプを持たないプロセス**（コンソールアプリ、サービス）—`WH_GETMESSAGE` が発火しない。
* **32bit プロセス** — 対象外。64bit 専用ビルド。
* **より高い整合性レベルのプロセス** — UIPI が低整合性の Font-tuner からのフックメッセージを遮る。

---

## 2. 描画プロファイル

プロファイルは `profiles/ini/*.ini` にある。有効なものは `font-tuner.ini` の`[General] AlternativeFile=ini\<名前>.ini` で選び、以降に生成されるプロセスへ適用される。トレイの「プロファイル」サブメニューで項目を選ぶと、このキーを書き込む（`WritePrivateProfileString`）。

注入されたコアはこのキーをアタッチ時に一度だけ読む。だから切替は以降に起動するプロセスで現れる。動作中プロセスを更新するには、トレイの「プロファイルを再読み込み」を使う。登録メッセージ（`FontTuner.ReloadProfile`）をブロードキャストする。各コアはそれを自分の `GetMsgProc` で受ける（すでにそのプロセスの UI スレッド上）。描画ロックの下で `font-tuner.ini` を読み直す。監視スレッドはなく、DLL が消えた後に走るものもない。トレイの「Font-tuner を再起動」は「終了」と同じ経路（フック解除・アイコン削除）で抜けた後、`main` の末尾で単一インスタンスのミューテックスを閉じてから同じ exe を起動し直す。手で終了 → 起動するのと同じで、コアには何もしない。トレイの「バージョン」は About を開く。バージョン・ライセンス（GPL-3.0-only）・ソース URL・必須の FreeType クレジットを載せる。

### 2.1 5 つのプロファイル

| プロファイル | ヒンティング | アンチエイリアス | 性格 |
|---|---|---|---|
| **Clean Greyscale** *(既定)* | 2（FreeType オートヒント） | 0 グレースケール | 中庸で柔らかく、色にじみなし。出荷時の既定。 |
| **Clean Dark Greyscale** | 0（フォント内蔵） | 0 グレースケール | 暗い背景向けに調整したグレースケール（低め gamma 1.1、contrast 0.9、やや太め）。 |
| **Accurate** | 2（FreeType オートヒント） | 4 LightLCD | FreeType のオートヒンタで最も強くグリッドフィット。小さい/UI サイズで最も鮮鋭、形が最もピクセル整列。 |
| **Clean Sharp** | 1（なし） | 2 LCD | サブピクセル（カラー）LCD、ヒンティングなし。横方向の細部が高く鮮鋭。（旧「Clean」） |
| **Clean Sharp Dark** | 0（フォント内蔵） | 2 LCD | 暗い背景向けに調整した LCD サブピクセル。（旧「Clean Dark」） |

ヒンティングモード（上流 `ft.cpp` の `FreeTypePrepare` と同じ対応。`render-core/src/ft.rs` の `flags`）: **0** = フラグなし＝FreeType の既定。フォント内蔵の TrueType バイトコードがあればそれでヒントする。**1** = `FT_LOAD_NO_HINTING`（アウトラインのまま、最も柔らかく最も忠実な形）。**2** = `FT_LOAD_FORCE_AUTOHINT`（FreeType のオートヒンタ、最も強く小サイズで最も鮮鋭、形が少し歪みうる）。

アンチエイリアスモードもロードターゲットを決める（同じく `FreeTypePrepare`）: **0** グレースケール = `FT_LOAD_TARGET_NORMAL`。**2/3** LCD = `FT_LOAD_TARGET_LCD`。**4/5** LightLCD = `FT_LOAD_TARGET_LIGHT`（縦方向だけスナップする軽いオートヒント）で描画は LCD。ヒンティングの見え方はフォント次第（`render-core` を直接呼んで HintingMode 0/1/2 を並べて確認した）。自前の TrueType ヒントを持つフォント（Yu Gothic）では、グレースケール × オートヒント（HintingMode 2）で 12px 前後の欧文の大文字の高さが字ごとにずれる（C や 3 が大きく、Z が小さい）。逆に欧文にヒントを持たないフォント（BIZ UDPゴシック。インタープリタ v35 と v40 で出力が同一）では、フォント内蔵（0）は事実上ヒントなしで、Z の上端が半ピクセルにかかって灰色の行になり、オートヒントなら揃う。既定の Clean Greyscale はオートヒント。Yu Gothic を嫌う人はシステムフォントをトレイで替えるので、替えた先のフォントで揃う方を既定にした。

全プロファイルが DirectWrite `RenderingMode=2`（GDI_CLASSIC）を使い、GDI と DirectWrite のテキストを一致させる。

`[DirectWrite]` 節（`GammaValue`・`Contrast`・`ClearTypeLevel`・`RenderingMode`）は、自前でラスタライズできないテキストに対して Direct2D へ指定する値（1.2）。既定は上流に従う: gamma は一般の gamma から導出（`g² > 1.3 ? g²/2 : 0.7`）、contrast 1.0、ClearType level 1.0、mode 5。`GammaValue` が 0（グレースケール系プロファイルの出荷値）のときは「上書きしない」の意味で、導出 gamma にフォールバックする（DirectWrite は gamma > 0 を要求するため）。

`[Experimental] ClipBoxFix`（既定 1）は、メトリクスのみの問い合わせで`GetGlyphOutline` が返すメトリクスを補正する。原点を `floor(1.5·DPI/96)` px 上げ、黒箱を同じだけ広げ、どちらもフォントの ascent/height で頭打ちにする。これで、そのメトリクスにグリフをクリップするアプリ（Java2D）が、太めに描かれたグリフを切り落とさない。`[Experimental@idea64.exe]` のようなプロセス別の節は上流だけが読む。コアにプロセス別設定はない。

### 2.2 メニュー順

トレイはプロファイルを固定の優先順（`src/main.rs` の `ORDER`）で並べ、アルファベット順にはしない: グレースケールの 2 つ → Accurate → LCD の「Clean Sharp」系。一覧にないプロファイルはその後にアルファベット順で入るので、`.ini` を足せばコード変更なしで表示される。その下に区切りを挟んで「カスタム」（2.3）と「カスタムを調整...」が並ぶ。

### 2.3 カスタムプロファイル

「カスタムを調整...」（`src/custom.rs`）は、コアが読む 8 つのキーをコンボ / スライダーで変えるダイアログを開く。

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

スライダーの範囲は非常識な値（潰れる / 消える）にならない幅で切ってある。`--custom` 引数で起動すると同じダイアログを直接開く。

プレビューはトレイ自身が `render-core` をリンクして描く（注入なし）。フォントは `GetFontData` で GDI から取り出して FreeType にメモリ面として渡す。注入したコアが各 DC でやっているのと同じ経路なので、プレビューと実描画は同じラスタライザ・同じフォントデータを通る。プレビュー用のフォントは「フォント...」（`ChooseFont`）で替えられ、既定はシステムのメッセージフォント。トレイの exe はマニフェストで PerMonitorV2 の DPI 対応を宣言しているので、プレビューはビットマップ拡大されず 1:1 で出る。

「適用」は `%APPDATA%\Font-tuner\Custom.ini` を書き、`font-tuner.ini` の `AlternativeFile=` にその**絶対パス**を入れ、「プロファイルを再読み込み」と同じブロードキャストを送り、全ウィンドウを無効化して再描画させる。コアの `profile.rs` は `AlternativeFile` を `dir.join(rel)` で解決していて、Rust の `Path::join` は右辺が絶対パスならそれをそのまま返す。だからこの機能で `RenderCore64.dll` は変わらない。

結果として起きること:

* ファイルを書くのはトレイだけ。コアは読むだけ（従来どおり）。
* `Custom.ini` が読めないプロセス（ファイルを消した、別ユーザーの `%APPDATA%` を指している）は、従来と同じく組み込みの Clean Greyscale に落ちる。落ちる（クラッシュする）経路はない。
* `font-tuner.ini` は全ユーザー共通なので、あるユーザーがカスタムを選ぶと、別ユーザーのプロセスは他人の `%APPDATA%` を読めず既定に落ちる。共有 PC ではそのユーザーが自分のプロファイルを選び直す。
* メニューの「カスタム」は `Custom.ini` があるときだけ選べる（無ければグレー）。

`GammaMode` はこの機能に合わせてコアの ini パーサが読むようになった（`config.rs`）。出荷プロファイルはすべて `GammaMode=0`（べき乗）なので既存の挙動は変わらない。

```
1. Clean Greyscale
2. Clean Dark Greyscale
3. Accurate
4. Clean Sharp
5. Clean Sharp Dark
```

### 2.3 ブレンド計算式

コア（`render-core`）は、上流 MacType（`ft.cpp` `CAlphaBlend`）と同じ線形空間のアルファブレンドで FreeType のカバレッジをピクセルに変える。これが仕様で、`render-core/src/filter.rs` が `f32` で実装する。バイト値は `x = v/255` で正規化する。

**ガンマ符号化** `g(x)`（バイト → 線形光）。`GammaMode` で選ぶ:

```
g(x) = x                                  GammaMode < 0   (線形)
     = srgb(x)                            GammaMode = 1   (sRGB)
     = (srgb(x) + x) / 2                  GammaMode = 2   (sRGB と線形の平均)
     = x ^ GammaValue                     それ以外        (べき乗ガンマ)

srgb(x) = x / 12.92                       x <= 10/255
        = ((x + 0.055) / 1.055) ^ 2.4     それ以外
```

**カバレッジ曲線** `a(cov)`（FreeType カバレッジ → アルファ）。`RenderWeight` と`Contrast` による S 字で、`t = (cov/255) ^ (1/RenderWeight)`:

```
a(cov) = (2t) ^ Contrast / 2              t < 0.5
       = 1 - (2(1 - t)) ^ Contrast / 2    t >= 0.5
```

**ブレンド**。前景 `fg` を背景 `bg` にカバレッジ `cov` で合成:

```
out = g⁻¹( g(bg)·(1 - a(cov)) + g(fg)·a(cov) )
```

つまり両色を線形光に変換し、カバレッジのアルファで補間し、戻す。`g⁻¹` は `g` の数値的な逆関数（プロファイルごとの符号化テーブルの二分探索、閉じた式のない平均モードを含め、全 `GammaMode` を逆変換する）。各チャンネルは独立にブレンドするので、LCD サブピクセルのカバレッジは R/G/B へ別々に入る。

上流はこれを固定小数点の整数で計算し、最後の段で切り捨てる。計算式を `f32` で計算して最も近いバイトへ丸める移植は、上流と最大 1 階調しか違わない（知覚できず、丸めた値の方が正確）。計算式が正しさの基準で、実装はそれに対して検証する。上流の丸めに対してではない。

`[DirectWrite]` 節は、コアがラスタライズできない Direct2D 自身の描画向けに、別の値（gamma、contrast、ClearType level、rendering mode）を与える（1.2）。

---

## 3. システムフォント切替

トレイのサブメニュー「システムフォント / System font」（`src/sysfont.rs`）。シェルの UI フォント（caption、small-caption、menu、status、message、icon-title）を`SystemParametersInfo`（`SPI_SET{NONCLIENTMETRICS,ICONTITLELOGFONT}`）で入れ替える。これはユーザーごとに永続する設定で、`WM_SETTINGCHANGE` でブロードキャストされる。

* 固定の候補: `BIZ UDPゴシック`、`BIZ UDゴシック`、`Noto Sans JP`、`メイリオ`。
* 初回の変更時に元のフォント一式を`%LOCALAPPDATA%\font-tuner\sysfont-backup.bin` に保存する。「既定に戻す /Default (restore)」がそれを復元し、バックアップを消す。
* 堅牢化: 復元時、（ユーザーが書き換えられる）バックアップファイルの `cbSize` は**信頼しない**。本物の構造体サイズに強制するので、`SystemParametersInfo` がバッファの外を読むことはない。

新しく描かれる UI には即座に完全適用される。シェルはサインアウト/インの後に完全に反映する。

---

## 4. トレイアイコン

2 つのアイコンを `app.rc` / `build.rs`（`embed-resource`）で exe に埋め込む:

* `assets/tray-dark.ico` — **シルバー**の金属光沢の歯車 + 「A」。**暗い**タスクバー用。
* `assets/tray-light.ico` — **黒**の金属光沢の歯車 + 「A」。**明るい**タスクバー用。

起動時と `WM_SETTINGCHANGE` のたびに、Font-tuner は`HKCU\...\Themes\Personalize\SystemUsesLightTheme` を読んで一致するアイコンを選び、テーマ変更時にその場で入れ替える。値が無ければ暗（Windows 11 の既定）。アイコンの図案は CC0（パブリックドメインの歯車）に描画した「A」。

---

## 5. ビルド

* **ツールフラグ** — `.cargo/config.toml` が MSVC ターゲットに `+crt-static` を設定し、`VCRUNTIME140.dll` 依存（と DLL 探索順ハイジャックの面）を消す。Releaseプロファイル: `opt-level="s"`、LTO、`panic="abort"`、strip 済み。
* **`build-core.ps1`** — 出荷 DLL に必要な唯一のネイティブ依存だけをビルドする:snowie2000 の FreeType フォーク（`freetype64.lib`）を MSBuild/vswhere で。C++ のMacType コア・Detours・IniParser はもうビルドしない。描画コアは Rust（`RenderCore64.dll`）で、フックは `retour` を使う。
* **`build-msi.ps1`** — `build-core.ps1`、`cargo build --release`（ワークスペース + `render-inject`）を走らせる。`check-export-rva.ps1` でコアが `GetMsgProc` を RVA`0x1000` に export しているか確認する（違えば中止、1.1 参照）。exe と 2 つの DLL のファイルバージョンが `Cargo.toml` の版と一致するかも確認する（違えば中止、§6 参照）。次に exe + DLL 群 +`font-tuner.ini` + `ini\*.ini` を `build\pkg` に集める。最後に `wix build` で `dist\font-tuner-<ver>-x64.msi`。

---

## 6. インストーラ（MSI、WiX v6）

* **スコープ** perMachine、`C:\Program Files\Font-tuner` に入れる。「終了」後にトレイを起動し直せるようスタートメニューのショートカットを足す。
* **ログオン時に起動** — `HKLM\...\CurrentVersion\Run\Font-tuner` を書く。
* **インストール時** — 動作中の `font-tuner.exe` を止め、Font-tuner を起動する。
* **Restart Manager 無効化**（`MSIRESTARTMANAGERCONTROL=Disable`、`REBOOT=ReallySuppress`）: `RenderCore64.dll` は全 GUI プロセスにマップされている。無効化しないと Restart Manager がそれらを全部閉じる（ユーザーのシェルを落としたことがある）。閉じるのはトレイだけにする。
* **使用中コアの入れ替え** — コアが全動作中プロセスにマップ（かつ常駐固定）されているため、そのファイルは決して上書きできない。遅延カスタムアクション（`RenameOldCore`、`InstallInitialize` の直後、`RemoveExistingProducts` の前にスケジュール）が使用中の `RenderCore64.dll` を脇へリネームし、`InstallFiles` が新しいものをすぐ置ける。起動し直したトレイが新コアでフックする。脇へリネームしたコピーは次の再起動時の削除に予約する（`MoveFileEx DELAY_UNTIL_REBOOT`）。以降に起動するプロセスへ更新を効かせるのに再起動は要らない。脇へリネームしたイメージは元のパスのまま全動作中プロセスにマップされ続ける。だから新コアで`GetMsgProc` の RVA を同じに保つ必要がある（1.1）。コアが食い違う動作中プロセスをトレイが見つけたら、フックせずサインアウトを促す（1.2）。
* **バージョン資源** — 版は root `Cargo.toml` の `[workspace.package] version` が唯一の出所。exe と両 DLL は各 `build.rs` が生成する `VERSIONINFO` でそれを埋め込む。`render-inject` はワークスペース外なので、その `build.rs` は root `Cargo.toml` を読む。Windows Installer は版付きのファイルを「新しい版が高いときだけ」置き換える。版なしのファイルは、更新日時が作成日時と違うと「利用者が改変した」とみなして置き換えない（[File Versioning Rules](https://learn.microsoft.com/en-us/windows/win32/msi/file-versioning-rules)）。版なしだった頃は更新で `font-tuner.exe` が古いまま残った（ログに `Existing file is unversioned but modified`）。だからリリースごとに版を上げる。版を上げずに作り直した MSI は、同じ版のファイルを置き換えない（ログに `Existing file is of an equal version`）。`REINSTALL=ALL` も効かない（`wix build` のたびに ProductCode が変わり、未インストールの製品扱いで 1603 になる）。開発中に同じ版で入れ直すときは、アンインストールしてから入れる。
* **`font-tuner.ini`** は `NeverOverwrite` を付ける。ユーザーが選んだプロファイル（`AlternativeFile` の値）が更新をまたいで残る。
* **アンインストール** — 標準の「プログラムの追加と削除」項目、または`msiexec /x {ProductCode}`。ファイル・Run レジストリ値を消し、プロセスを止める。
* **署名** — MSI とそのペイロードは**未署名**なので、インストール時に UAC が「発行元不明」と出す（SmartScreen も出うる）。ブロックはされない。署名はリリースを帰属不能に保つためあえて省く。どのみちブラウザのレンダラー/GPU プロセスへ到達する助けにならない（§1.3）。

---

## 7. ライセンス

GPL-3.0-only。`vendor/` の FreeType フォークをそれぞれのライセンスで同梱する。トレイアイコンの図案は CC0。
