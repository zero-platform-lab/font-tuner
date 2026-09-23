# AGENTS.md

font-tuner の repo 固有ルール。一般的な文章術やエージェントの振る舞いは書かない。

## 名称・ライセンス
- 製品の名称は `font-tuner` / `RenderCore64.dll` / `font-tuner.ini` を使う
- 製品のファイル名・設定キー・UI 文言に upstream プロジェクト名を使わない（GPL の帰属を示す表記としてのみ残す）
- ライセンスは GPL-3.0-only と書く（`-or-later` にしない。upstream が「or later」を許諾していない）

## ビルド
- `build-core.ps1` で `build/lib/freetype64.lib` を先に作る（Visual Studio / MSBuild、WiX が要る）
- 意図的なキャストは各ファイルの `#![allow]` に理由を添えて残す（検証済みの固定小数点・FFI 境界・境界チェック済みの座標）
- コミット前に [`check-lint.ps1`](check-lint.ps1) を通す（PSScriptAnalyzer / textlint + prh / `cargo clippy`）

## 注入 DLL（`RenderCore64.dll`）
- `GetMsgProc` は RVA `0x1000` から動かさない。常駐固定のため、更新後も旧ビルドを持つプロセスが `old_base + RVA` を呼ぶ。ずれると全プロセスが一斉に落ちる（`build.rs` の `/ORDER` で固定し、トレイと `build-msi.ps1` が検証する）
- upstream にない機構（監視スレッド・独自の設定ファイル・ライブ再読込など）を足さない。足すなら結果（スレッドの寿命・アンロード後に何が残るか・そのファイルを誰が書くか・失敗時にどのプロセスが落ちるか）を先に列挙する
- 「全プロセスが同時に落ちる」経路を作らない。作らざるを得ない場合は根拠と検証結果を docs に書く
- 変更は単一プロセス（`loader` + 対象アプリ）で検証してから書く。「動くはず」を根拠にしない

## 反映タイミング
- プロファイル切替・有効/無効・更新は「次に起動するプロセスから」効く。起動中のプロセスは「プロファイルを再読み込み」で反映する
- 検証はトレイを止めた状態で行う。動作中のトレイはインストール済みコアを全プロセスに注入するため、2 つのビルドが同じフックを奪い合う

## インストール
- `msiexec` は `MSIRESTARTMANAGERCONTROL=Disable` を付けて実行する（Restart Manager がフック DLL を持つ全プロセスを閉じるため）

## 裏取り
- upstream の挙動は [MacType の実コード](https://github.com/snowie2000/mactype/blob/05052e88c7ce134f93b66db95132284a1ed10de7)（移植元コミット `05052e8`）で確認する。推測で書かない
- Windows API の挙動は一次資料（Microsoft Learn）で確認する。似た API からの外挿で断定しない
- 実機で確かめた事実と、未確認の仮説を書き分ける

## 書き方
- コミットメッセージ・PR・ドキュメントは日本語で書く（コード内のコメント・識別子は英語でよい）
- 実装と食い違う記述を残さない。挙動を変えたら同じコミットで docs も直す
- メールアドレスを出さない（GitHub noreply を使う）。コミット・PR にセッション URL を入れない
