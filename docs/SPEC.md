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
* **一度きりの vtable パッチ**は直列化する。対象は解析オブジェクトの 3 スロット（`GetAlphaTextureBounds` / `CreateAlphaTexture` / `GetAlphaBlendParams`）と、Direct2D の全生成スロットとテキストスロット。解析オブジェクトは `Once` で、Direct2D はミューテックスの下で再確認する。さもないと競合する 2 スレッドが両方ともパッチ済みスロットから「元の関数」を捕まえ、detour が自分自身を呼ぶ → 無限再帰。Direct2D のスロットは (vtable, slot) を鍵にした 1 つのマップで管理する。レンダーターゲットのクラスごとに vtable が違うため。
* **Direct2D への到達** — `render-inject/src/d2d.rs` が upstream の生成チェーンを辿る。入口は `D2D1CreateFactory`・`D2D1CreateDevice`・`D2D1CreateDeviceContext` の 3 つ。`D2D1CreateFactory` からは `CreateHwnd/DC/WicBitmapRenderTarget` と `ID2D1Factory1..7::CreateDevice` に至る。`D2D1CreateDevice` からは `ID2D1Device..6::CreateDeviceContext` に至る。各ターゲットでパッチするスロットは 7 つ。生成系の `CreateCompatibleRenderTarget`（12）、描画系の `DrawText`（27）・`DrawTextLayout`（28）・`DrawGlyphRun`（29）・記述付きの overload（82）。設定系の `SetTextAntialiasMode`（34）・`SetTextRenderingParams`（36）。ターゲットの `ID2D1DeviceContext` の vtable が別なら、そちらにも同じものを当てる。12 はオフスクリーンのビットマップターゲットを生成時にフックするため。27 と 28 は upstream もフックしている。Scintilla（Notepad++）の文字はすべて `DrawTextLayout` を通り、`DrawGlyphRun` を通らない（実測とソース）。0.1.10 までは 27 と 28 が無く、params の差し替えすら効いていなかった。スロット番号は `windows` クレートの vtable 定義で照合した。描き方は 1.5 の「Direct2D の文字」。
* **注入前に作られた Direct2D のオブジェクト** — コアが入るのは、アプリが最初にメッセージを取りに来たとき。それより前に作られたファクトリや描画先は、生成のフックを通らない。Notepad++ は起動直後にファクトリを作り、HWND 用の描画先も最初の描画（`UpdateWindow` がメッセージキューを通さずに送る `WM_PAINT`）で作る（実測）。そこでアタッチ時に d2d1.dll がすでに読み込まれていれば、コア自身がファクトリ（シングル / マルチスレッド）と DC 用・HWND 用の描画先を 1 つずつ作る（HWND 用はメッセージ専用ウィンドウで作り、その場で破棄する）。これらはフック済みの入口を通るので、同じクラスの vtable がパッチされ、アプリが持っているオブジェクトにも届く。DXGI サーフェスのデバイスコンテキストは Direct3D のデバイスが要るので試さない。こちらは注入後に作られたものにだけ届く。
  * いつ・どこで: d2d1.dll がアタッチ時にすでに読み込まれているプロセスだけで、アタッチ用のスレッド（`on_attach`。ローダーロックの外）で 1 回。
  * 作るもの: ファクトリ 2 つ、DC 用の描画先 2 つ、メッセージ専用ウィンドウ 1 つ（`STATIC` クラス）、HWND 用の描画先 2 つ。関数を抜ける前に全部解放し、ウィンドウも同じスレッドで破棄する。
  * 描画先はソフトウェアの種類（`D2D1_RENDER_TARGET_TYPE_SOFTWARE`）で作る。GPU のデバイスを立ち上げない（ドライバの DLL をローダーロックの下で読み込むことを、Direct2D を持つ全プロセスで起こさない）ため。ソフトウェアの描画先でも、アプリが持つハードウェアの HWND / DC 用の描画先と同じ vtable に届くことを、Notepad++ の DirectWrite / DirectWrite DC モードで実測した。
  * 残るもの: パッチした vtable の項目（ほかのフックと同じく、プロセスの寿命のあいだ）。作ったオブジェクトは残らない。
  * 失敗したとき: 作れなかった数をログに書くだけで、ほかには何もしない（描画はフックの無い状態のまま）。
  * アプリのスレッドが使っている最中の vtable を書き換えることになる。元の関数をマップに登録してから、ポインタ 1 語を書き換える（`patch_once`）。ほかのスレッドが同時に呼んでも、古い関数か新しい detour のどちらかに入り、detour は元の関数をマップから必ず見つけられる。
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
| 描画状態 | `static RENDER: Mutex<Option<RenderState>>`（`Ft` + `Tables` + `Profile` + 最近の DirectWrite の面の一覧） | — | 1 プロセスに FreeType ライブラリが 1 つ。面とグリフのキャッシュは `Ft` の中にあり、ロックの下でだけ触る。描画はロックの下で直列。プロファイル再読み込みも同じロック（`Ft` ごと作り直すので、キャッシュも捨てる） |
| DirectWrite（`dwrite.rs`、`layout.rs`） | `IDWriteBitmapRenderTarget::DrawGlyphRun`、`IDWriteFactory{,2,3}::CreateGlyphRunAnalysis` の vtable スロット | `windows` クレートの vtable 定義とスロット番号が一致すること（照合済み）。ランの配列は `glyphCount` 要素（null の `glyphAdvances` / `glyphOffsets` は読まない） | 一度きりのパッチはミューテックスで直列化。面はローカルのフォントファイルならパス + index で開き（複製しない）、そうでなければ bytes + index で開く。`IDWriteFontFace` を `RenderState` が clone で保持してアドレスの再利用を防ぐ（1.5「フォントとグリフのキャッシュ」）。OS に描かせるときの params は、呼び出しの間 clone を持ち続ける（プロファイルの再読み込みで解放されないように） |
| Direct2D（`d2d.rs`） | `D2D1CreateFactory` / `D2D1CreateDevice` / `D2D1CreateDeviceContext` と各ターゲットの vtable スロット 12 / 27 / 28 / 29 / 82 / 34 / 36。`IDWriteTextRenderer` の実装（`windows::core::implement`） | 同上 | (vtable, slot) → 元関数のマップ `SLOT_ORIG` を 1 つのミューテックスで管理。濃淡のビットマップを塗るあいだだけターゲットの変換・アンチエイリアスとブラシの変換を差し替え、必ず戻す |
| 自己常駐固定 | `GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN)` | — | `DllMain(DLL_PROCESS_ATTACH)` で最初に行う。以降 `FreeLibrary` は no-op |
| ログ | `%TEMP%\render-inject.log` に追記 | — | ロック付き。初回の描画結果を `render-inject-capture.png` に保存する（検証用） |

`DllMain` では常駐固定・ミューテックス取得・スレッド起動だけを行い、フックの設置と FreeType の初期化は別スレッド（`on_attach`）で行う。ローダーロックの下で detour を張らない。

**テキスト配置** — `SetTextAlign` はランの原点を (x, y) からどう測るかを決める。水平は LEFT / RIGHT / CENTER、垂直は TOP / BOTTOM / BASELINE で、CENTER と BASELINE はどちらも 2 ビット立つのでマスクして等値比較する（`&` で判定すると CENTER が RIGHT にも一致する）。GDI は自前の描画にこれを適用するので、移植でも適用しないと右揃え・中央揃えの文字が文字列の幅ぶんずれる。実測（素の GDI と比較、`ALIGN-TEST` を x=160 に 14px で描画）: `TA_RIGHT` はインクが 86〜159、`TA_CENTER` は 123〜196、`TA_LEFT` は 160〜233。0.1.6 までは水平を無視して常に 160 から描いていた。幅は `dx` 配列があればその合計、無ければ実測（`text_width`）。upstream も同じ（`override.cpp` の `switch (horiz)` / `switch (vert)`）。

**`ETO_PDY`（縦に進むラン）** — このフラグが立つと `lpDx` は 1 文字あたり `(dx, dy)` の 2 要素になり、配列長は文字数の 2 倍になる。正の `dy` はペンを**上**へ動かす（実測。upstream も `ft.cpp` で `FTInfo.y -= clpdx.gety(0)`）。`Layout { pdy }` が 2 要素ずつ読み、`render-core` は `pen_x` と `base_y` の両方を動かす。キャンバスは縦の移動量ぶん広げ、横幅は `GetTextExtentPoint*` の実測を下回らせない（`dx` が全て 0 の縦書きでもグリフ自身の幅は要るため）。0.1.9 までは配列を文字数ぶんしか読まず、1 つおきの `dy` をアドバンスとして扱っていた。縦に積むはずの文字が横に並んで 2 文字ずつ重なる（実測: 素の GDI は幅 10px・高さ 27px、こちらは幅 35px・高さ 11px）。

**字間** — `SetTextCharacterExtra` は各文字のアドバンスに定数を足す。`render-core` の `Layout { dx, extra }` が `lpDx` と一緒に受け取り、`pen_x += advance + extra` で送る。upstream も自前のレイアウトループで同じことをする（`ft.cpp` の `FTInfo.x += charExtra`。`override.cpp` の `SetTextCharacterExtra(hCanvasDC, ...)` は GDI に落とす経路の辻褄合わせで、字間の再現そのものではない）。幅の扱いは `lpDx` の有無で変わる。`lpDx` が無ければ `GetTextExtentPoint*` の実測に字間が含まれるので、そのまま使う。`lpDx` があれば GDI がそこに字間を上乗せするので `sum(dx) + n * extra` とする（実測: `lpDx`=20×5・字間 10 で 5 文字目が 4×30 の位置から始まる）。

**`TA_UPDATECP`** — 原点は引数の (x, y) ではなく DC の現在位置で、描画後にその位置が進む。`GetCurrentPositionEx` で取り、描画後に `MoveToEx` で進める（左揃えは GDI の実測幅ぶん進め、右揃えは戻し、中央揃えは動かさない。upstream と同じ）。0.1.7 までは引数の位置に描き、現在位置も動かさなかった（実測で x=120 のところを x=1 に描いていた）。

**論理座標と写像** — `ExtTextOutW` の座標・矩形・`lpDx` はすべて論理単位で、マップモード（`SetMapMode` + 窓/ビューポートの拡張）とワールド変換（`GM_ADVANCED` の `SetWorldTransform`）が効く。render-core はデバイスピクセルでラスタライズするので、写像のかかった DC ではランの全体をデバイス単位に換算してから描き、結果もデバイス単位で blit する（`xform.rs`、`dib.rs` の `DeviceUnits`）。

写像は `LPtoDP` に 3 点通して復元する。gdi32 の未公開 export `GetTransform` は使わない（両者が一致することは実測で確認した）。線形部だけを見て、平行移動は見ない。正の軸平行スケール（`m12 == m21 == 0`、`m11 > 0`、`m22 > 0`）なら倍率を掛けて描き直し、回転・せん断・鏡像・退化した写像は素の `ExtTextOutW` に委ねる。upstream も同じ判断（`override.cpp` 1200-1217。その下の `GetMapMode` / `GetWorldTransform` を見るブロックはコメントアウトされた旧実装）。

換算するもの: 原点（`LPtoDP`、平行移動込み）、アセント・ディセント・高さ・`em_px`（`sy` 倍）、幅と字間（`sx` 倍）、`ETO_OPAQUE` / `ETO_CLIPPED` の矩形、そして `lpDx`。`lpDx` は累積位置ベースで換算し、要素ごとに丸めて誤差を溜めない（upstream の `TransformlpDx` と同じ）。`TA_UPDATECP` の現在位置だけは論理単位なので、GDI の論理実測幅で進める。

**DirectWrite のラン配置** — `IDWriteBitmapRenderTarget::DrawGlyphRun` はランを render-core で描く。グリフごとの位置は DirectWrite と同じ規則で決める（`layout.rs`）。規則はどれも素の DirectWrite と `verify/dwrite-probe` で突き合わせて確かめた。

* ペンはベースライン原点から `glyphAdvances` ずつ進む。null ならフォント自身のアドバンスで進む。natural の計測モードはデザインメトリクス、GDI の計測モードは `GetGdiCompatibleGlyphMetrics` を使う。
* `advanceOffset` は読む方向へ、`ascenderOffset` は上へずらす。
* `bidiLevel` が奇数なら右から左へ進む。グリフは、ランのアドバンスではなく**そのグリフ自身のアドバンス**で右端をペンに合わせる。そのあとペンをランのアドバンスだけ左へ送る。正の `advanceOffset` は左へずらす（実測: 12 DIP 送りの "AVATo" で、インクの右端は原点の 1px 左）。
* `isSideways` は各グリフを反時計回りに 90° 回し、縦のメトリクスで進める。縦の原点 `(advanceWidth / 2, verticalOriginY)` をペンに置くので、グリフはベースラインを中心に上下へ振り分けられる。
* デバイスピクセル = pixelsPerDip ×（BRT の変換 × DIP 座標）。平行移動も DIP 単位。一様な正の拡大と平行移動だけを自前で扱う。回転・せん断・鏡像・縦横で違う拡大は DirectWrite に描かせる（下記）。
* em サイズは 26.6 固定小数点で FreeType に渡す（`FT_Set_Char_Size`）。13.5 DIP のような小数の大きさも丸めない。
* フォントの合成（`GetSimulations`）も再現する。斜体は横方向に 1/3 傾ける。太字は横に em/40、縦に em/60 太らせる（実測: Yu Gothic UI の "l" で、em 120 のとき傾き 29px / 高さ 89px、太字で幅 +3px・高さ +2px）。
* `blackBoxRect` には描いたインクの矩形を返す（ビットマップの外へ出た分も含め、切り詰めない）。インクが無いランはベースライン原点の空矩形を返す。DirectWrite と同じ。

実測（`verify/dwrite-probe`、Yu Gothic UI・24 DIP の 25 ケース）: インクの上下左右の端はすべて素の DirectWrite と 1px 以内に収まった。残る 1px は FreeType のヒンティングによる字形の差。0.1.10 までは advances・offsets・RTL・縦書き・pixelsPerDip・変換・合成をすべて無視していた。どのケースも同じ位置に同じ大きさで描いたうえ、`blackBoxRect` を書かずに S_OK を返していた（呼び出し側が再描画範囲を失う）。

**DirectWrite の解析経路** — グリフを自分で合成するアプリは、`IDWriteFactory{,2,3}::CreateGlyphRunAnalysis` で解析オブジェクトを作る。そして `GetAlphaTextureBounds` で範囲を聞き、`CreateAlphaTexture` で濃淡を受け取る。WPF がこの経路を通ることを実測で確かめた（Chromium/Skia も通るが、署名の壁（1.3）で注入できない）。生成のたびに、ランを上と同じ規則で配置し、解析オブジェクトのアドレスで覚える。範囲と濃淡は render-core で作るので、呼び出し側が確保するバッファの大きさと中身が同じグリフから決まる。

* 変換: Factory1 の変換は DIP 単位で、そのあと pixelsPerDip で拡大される（実測: pixelsPerDip 1.5・平行移動 (10, 5) で原点が (75, 97.5)）。Factory2 / 3 には pixelsPerDip が無く、変換に畳み込まれていて、平行移動はピクセル単位。
* テクスチャの種類: 解析オブジェクトが作るのは 1 種類だけ。ClearType なら 3x1、グレースケールか aliased なら 1x1 で、もう一方の範囲は空になる（実測）。DirectWrite 自身が空を返す範囲（作らない種類、インクの無いラン）は空のまま返す。
* 3x1 の 3 つの値は、同じ濃さの 3 回ではなく、**横 3 倍の解像度でサブピクセルごとに測った濃さ**。呼び出し側はこれをサブピクセル単位でずらし、グリフを小数ピクセルの位置に置く（WPF で実測）。グレーを 3 回並べて渡すと、ずらしたところに赤とシアンのにじみが出た。そこで 3x1 には FreeType の LCD の濃淡を渡す。
* そのうえでアンチエイリアスはプロファイルが決める（GDI と同じ）。グレースケールのプロファイルでは解析オブジェクトをグレースケールで作らせ（Factory2 / 3 の `antialiasMode`）、1x1 のグレーの濃淡で描かせる。LCD のプロファイルでは ClearType で作らせる。upstream はアプリの指定のままにする。ここは upstream と違う判断。
* 覚えておく数は 256 まで（古い順に捨てる）。フォントはファイルの中身ではなく `IDWriteFontFace` の参照で持つ。0.1.10 までは解析オブジェクト 1 つごとにフォントファイルを丸ごと複製して 4096 件まで持っていた（日本語フォントは 1 ファイル 10〜20MB）。捨てた解析オブジェクトは、範囲と濃淡の両方が DirectWrite のものに戻る。同じアドレスを再利用した解析オブジェクトは、生成時に必ず上書きか削除をするので、古い記録を読むことはない。

実測（`verify/dwrite-probe`、Factory1 と Factory3 の ClearType / グレースケール、25 ケース）: 濃淡の上下左右の端はすべて素の DirectWrite と 1px 以内。WPF（検証用のアプリ）でも、行の位置・右から左の文字・太字・斜体が素の WPF と揃った。0.1.10 までは advances などを無視していたので、WPF で右から左の行が丸ごと消え、行末の文字が欠けていた（"Wavy" が "Wav"）。

自前で描けない解析オブジェクト（回転・せん断の変換など）は DirectWrite に作らせる。upstream の `IMPL_CreateGlyphRunAnalysis{,2,3}` と同じく、プロファイルの描画モード・グリッドフィット（`HintingMode=1` なら 1/65535 のずらし）で作らせ、拒まれたらアプリの引数に戻す。`GetAlphaBlendParams` は upstream と同じく、プロファイルの params に対する合成の値を返す。

自前で描けないラン（上記の変換、フォントファイルを読めない、プロファイルが無い）は DirectWrite に描かせる。そのときは upstream の `IMPL_BitmapRenderTarget_DrawGlyphRun` と同じく、アプリの rendering params の代わりにプロファイルの `[DirectWrite]` の params を渡す。`HintingMode=1` なら 1/65535 の変換ずらしも加える。DirectWrite が拒めば、ずらしなし、次にアプリの params のままで呼び直す。

**Direct2D の文字** — Direct2D の描画先には、クリップ・レイヤー・変換・グラデーションなどのブラシ・半透明がある。GDI の DC を借りて描くと、これらが全部抜け落ちる。0.1.10 までの実装は `ID2D1GdiInteropRenderTarget::GetDC` で DC を借りて描いていた。素の Direct2D と比べると、5 文字が幅 24px に重なり、クリップ・グラデーション・半透明・DPI・変換を無視していた。不透明度 50% のレイヤーでは描画範囲の外まで灰色で塗っていた（`verify/d2d-probe` で実測）。

いまは、ランを上と同じ規則で配置して render-core で描き、その濃淡を A8 のビットマップにする。それを描画先に `FillOpacityMask` でアプリのブラシのまま塗らせる。クリップやレイヤーは Direct2D がそのまま適用する。ビットマップはデバイスピクセルで作るので、塗るあいだだけ描画先の変換を単位行列にし、その変換をブラシの変換に畳み込む（グラデーションの位置が変わらない）。マルチスレッドのファクトリでは、ほかのスレッドが同じブラシや描画先を使いうるので、この差し替えから戻すまでを Direct2D 自身のロック（`ID2D1Multithread::Enter` / `Leave`、再入可）の中で行う。濃淡は `Tables::mask_alpha` でプロファイルのガンマとコントラストを通した不透明度にする。暗い文字を白に重ねた場合は GDI の合成と一致する。単色の明るいブラシなら、白い文字を黒に重ねた場合と一致する。`DrawTextLayout` と `DrawText` は、自前の `IDWriteTextRenderer` でレイアウトを glyph run に分解して描く。分解が途中で失敗したときは、何か描いていれば Direct2D には回さない（回すと描いた分が二重になる）。何も描いていなければ Direct2D に回す。下線と取り消し線は塗りつぶしの矩形にし、範囲ごとのブラシ（`SetDrawingEffect`）はそのブラシで塗り、埋め込みオブジェクトには自分で描かせる。スナップと DPI は描画先の値を答える。`DrawText` は、Direct2D と同じくレイアウト矩形でテキストレイアウトを作る（GDI の計測モードでは GDI 互換のレイアウト）。

濃淡のマスクはグレースケールしか表せない。そこで次のものは Direct2D に描かせる。upstream と同じく、プロファイルの params・アンチエイリアスモード・グリッドフィットのずらしを付ける。

* ClearType（LCD）のプロファイル
* aliased の文字
* 回転・せん断・鏡像の変換
* `ENABLE_COLOR_FONT` でカラーグリフを含むレイアウト（カラーの層に分けるのは Direct2D 自身のレイアウト描画だけ）
* フォントファイルを読めないラン

実測（`verify/d2d-probe`、25 ケース）: 描画先は 2 種類で測った。DC 用の描画先と、Direct3D 11 のテクスチャ（DXGI サーフェス）上のデバイスコンテキスト（`D2D_TARGET=dxgi`。WinUI / コンポジション系の作り方）。どちらでも、インクの端はすべて素の Direct2D と 1px 以内。クリップ・グラデーション・半透明のブラシ・不透明度 50% のレイヤー・範囲ごとの色・下線と取り消し線・右から左の段落・折り返し・`CLIP` 付きのレイアウト・カラー絵文字が素の Direct2D と揃った。Notepad++ 8.9.8（Scintilla の DirectWrite モードと DirectWrite DC モード）でも自前で描かれ、見た目がコアの GDI 経路と揃った。

速さは次の「フォントとグリフのキャッシュ」を参照。

**フォントとグリフのキャッシュ** — 0.1.10 までは、`Ft` が面を 1 つだけ持っていた。フォントが変わるたびに、フォントファイルを丸ごと読んで複製していた（日本語フォントは 10〜20MB）。グリフも描くたびに FreeType でラスタライズし直していた。いまは次のとおり。

* **面**: `Ft` は面を最大 8 つ、鍵つきで開いたままにする。古いものから閉じる。メモリから開いた面の複製は合計 64MB まで。鍵はフォントの出どころで決める。
  * DirectWrite の面: ローカルのファイル（`IDWriteLocalFontFileLoader` で分かる）なら、パスと index が鍵。FreeType がパスから必要な部分だけを読み、複製しない。FreeType はパスを ANSI で開くので、それ以外の文字を含むパスはメモリに読む。アプリがメモリから渡すフォントは、`IDWriteFontFace` のアドレスと index が鍵。その面は `RenderState` の一覧（最大 32）に clone で持ち、一覧から外すときに `Ft` でも閉じる。アドレスが別のフォントに再利用されても、古い面や古いグリフを出さないため。
  * GDI: 鍵は 0.1.10 までと同じ（フェイス名と `GetFontData` のデータ量）。キャッシュに無いときだけ `GetFontData` でデータを読む。
* **グリフ**: 鍵は面・グリフ番号・サイズ・スタイル（縦書き・太字・斜体）・FreeType の読み込みフラグ・描画モード・LCD フィルタ・embolden。ビットマップを `Arc` で共有して持ち、合計 8MB を超えたら丸ごと捨てる。面を閉じたとき、または同じ鍵で開き直したときは、その鍵のグリフも捨てる。

実測（500 回あたり。GDI は `verify/gdi-perf`、Direct2D は `verify/d2d-probe` の `perf` の行。どちらも Yu Gothic UI・24px の 19 文字）:

| 経路 | 素の OS | 0.1.10 | キャッシュあり |
|---|---|---|---|
| GDI（`ExtTextOutW`） | 約 27ms | 約 840ms | 約 590〜690ms |
| Direct2D（`DrawGlyphRun`） | 約 7ms | ―（描き間違えていた） | 約 128ms |

**GDI の DIB の使い回し** — GDI の描画は、DC の該当部分を DIB に写し、render-core で描いて書き戻す。この DIB を描くたびに作っていた（メモリ DC と DIB セクションの作成）。いまは upstream と同じく、画面の DC（とそれと互換のメモリ DC）に描くときは、スレッドごとに 1 枚を使い回す（upstream の `CThreadLocalInfo` の `CBitmapCache`、`cache.cpp`）。

* 予備が小さければ、両方を覆う大きさで作り直す。
* 256 回使うごとに、予備が描画より大きければ、描画の大きさで作り直す（upstream の `BITMAP_REDUCE_COUNTER`）。一度だけの大きな描画のために大きな DIB が居座らない。
* 4MB（1024 × 1024）を超える DIB は予備にせず、その場で解放する（upstream には無い）。
* 残るもの: 画面の DC に文字を描いたスレッドごとに、メモリ DC 1 つと DIB セクション 1 つ（4MB まで）。スレッドの終了時に解放する。DLL はアンロードしない（常駐固定）ので、スレッドローカルの後始末のコードは必ず残っている。
* プリンタとメタファイルの DC には、これまでどおり毎回その DC と互換の DIB を作る。

実測（`verify/gdi-perf`、交互に 5 回の中央値）: 約 573ms → 約 499ms。描いた結果はピクセル単位で同じ。

**合成の表引き** — 合成の逆ガンマ変換を表引きにした（2.4）。Direct2D の濃淡マスクに使う不透明度の曲線（`Tables::mask_alpha`）も、プロファイルの読み込み時に 256 要素の表にする（値は同じ）。実測: GDI は約 484ms → 約 413ms（`verify/gdi-perf`、交互に 5 回の中央値。描いた結果は 4 ピクセルが 1 階調違うだけ）、Direct2D は約 128ms → 約 94ms（`verify/d2d-probe` の `perf`）。

残りの時間の大半は、キャッシュ以外の処理にかかっている。GDI では 1 ピクセルごとの合成・描くたびの DIB の作成・DC との転送、Direct2D ではランごとの `CreateBitmap`。これらは挙動に関わるので、キャッシュとは別に扱う。

`BitBlt` も論理座標を取るので、DIB の出し入れの前後で DC を `SaveDC` → `MM_TEXT` + `GM_COMPATIBLE` + 恒等変換 → `RestoreDC` に挟む。0.1.8 まではこれをしておらず、写像のかかった DC では位置と大きさだけ `BitBlt` の引き伸ばしで偶然合い、**調整したグリフが最近傍拡大で潰れていた**（実測: 2 倍の DC で出力の 2×2 ブロックが一様 96 / 混在 0。素の GDI は同条件で混在 105）。素の GDI より悪い状態だった。

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
| `[DirectWrite] GammaValue` `Contrast` `ClearTypeLevel` `RenderingMode` | 自前でラスタライズできず OS に描かせる DirectWrite / Direct2D の描画に渡す `IDWriteRenderingParams`（1.2） | 2.5 |
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

両色を線形光に変換し、カバレッジのアルファで補間し、戻す。`g⁻¹` は `g` の数値的な逆関数。プロファイルごとの符号化テーブルの二分探索で、閉じた式のない平均モードを含め、全 `GammaMode` を逆変換する。1 ピクセルごとに探索しないよう、プロファイルの読み込み時にこの逆関数を 16384 点の表にする。表の点は線形光の平方根で等間隔に置く（暗い側を細かくする。急なガンマでは暗い色の線形光が詰まるため）。表引きの結果は、探索の結果と 1 階調以内で一致する（`decode_lut_matches_search`）。カバレッジが 255 のピクセルは計算せず前景色そのものにする（計算しても同じ値になる）。各チャンネルは独立にブレンドするので、LCD サブピクセルのカバレッジは R/G/B へ別々に入る。

upstream はこれを固定小数点の整数で計算し、最後の段で切り捨てる。計算式を `f32` で計算して最も近いバイトへ丸める移植は、upstream と最大 1 階調しか違わない。計算式が正しさの基準で、実装はそれに対して検証する（`cargo test`: 端点、単調性、全 `GammaMode`、gamma 1.25 の回帰値、逆関数の表と探索の一致）。

### 2.5 DirectWrite 節と ClipBoxFix

`[DirectWrite]`（`GammaValue`・`Contrast`・`ClearTypeLevel`・`RenderingMode`）は、自前でラスタライズできないテキストに対して DirectWrite / Direct2D へ指定する値（1.2）。upstream と同じく 2 組作る。Direct2D 用は `RenderingMode` をそのまま渡す。DirectWrite 用は 6（アウトライン）を 5（natural symmetric）に読み替える（upstream `GetDWParams` の「DW rendering in mode6 is horrible」）。既定は upstream に従う: gamma は一般の gamma から導出（`g² > 1.3 ? g²/2 : 0.7`）、contrast 1.0、ClearType level 1.0、mode 5。`GammaValue` が 0（グレースケール系プロファイルの出荷値）のときは「上書きしない」の意味で、導出 gamma にフォールバックする（DirectWrite は gamma > 0 を要求するため）。出荷プロファイルは全て `RenderingMode=2`（GDI_CLASSIC）で、GDI と DirectWrite のテキストを一致させる。

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
* **upstream との違い**: upstream は `FTC_Manager`（FreeType のキャッシュ）を使う。移植は `Ft` の中に自前の面とグリフのキャッシュを持つ（1.5「フォントとグリフのキャッシュ」）。

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

`font-tuner.ini` の `[UnloadDll]` 節は「調整を効かせないプログラム」の一覧（upstream の書式）。コアはアタッチ時に自分の exe 名をこの一覧と照合し、載っていればフックを張らずに戻る（`profile.rs` の `is_process_excluded`）。常駐固定は `DllMain` で先に済んでいるので DLL 自体はマップされたままだが、以降そのプロセスでコアのコードは走らず、描画は素の GDI になる。一覧の編集は以降に起動するプロセスから効く。upstream はここに自分のトレイを載せるが、出荷の一覧からは `font-tuner.exe` を外してある（0.1.5 から）。トレイのメニューとダイアログの文字をコアが描くと、フェイスやサイズの取り違えがその場で見えるため（TTC のフェイス選択の不具合はそれで見つけた）。

  `font-tuner.ini` は `NeverOverwrite`（7）なので、**0.1.4 以前から更新した環境では古い一覧がそのまま残り、トレイは除外されたままになる**。トレイも対象にしたい場合は、インストール先の `font-tuner.ini` から `font-tuner.exe` の行を手で消す。インストーラは触らない（利用者が足した項目を消さないため）。

  トレイを対象にする結果: コアの detour がトレイ自身の中でも走る。detour の中で panic すると `extern "system"` の境界で abort するので、そのプロセスは落ちる。これは全プロセスで同じだが、落ちるのがトレイだとプロファイル切替と無効化の UI を失い、他のプロセスは常駐固定されたコアで描き続ける（各プロセスが終了するまで）。復旧はトレイを起動し直すだけ。コアは `panic = "abort"` でビルドする。

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
