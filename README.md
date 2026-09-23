# Font-tuner

Windows のフォント描画チューナ。upstream の非公開 (Delphi 製) トレイ / ウィザード / ブートストラップを使わず、
公開ソースの描画 DLL (`RenderCore64.dll`) を自前でビルドして同梱する。

- 64bit プロセスのみ (32bit は対象外)
- Windows 11 前提
- UI は日本語 / 英語 (OS の UI 言語で切替)
- ライセンス: GPL-3.0-only。権利関係の詳細は [NOTICE.md](NOTICE.md)

## 使い方

`dist\font-tuner-<ver>-x64.msi` を入れると `C:\Program Files\Font-tuner\` に配置され、ログオン時に起動する。
タスクトレイのアイコンを右クリック:

| 項目 | 動作 |
|---|---|
| 有効 | フックの ON/OFF。OFF にしても既に DLL を読み込み済みのプロセスからは DLL をアンロードしない。以降に起動するプロセスへ注入しなくなるだけ。ON にするとき、古い `RenderCore64.dll` を保持したままのプログラムが残っていれば、その一覧を出してフックを張らない (張るとそれらが落ちるため)。サインアウトして入り直してから ON にする |
| プロファイル | `ini\*.ini` の一覧。選ぶと `font-tuner.ini` の `AlternativeFile=` を書き換える。新規プロセスから反映。末尾の「カスタム」は下の調整ダイアログで保存した自分用プロファイル (`%APPDATA%\Font-tuner\Custom.ini`) |
| カスタムを調整... | ヒンティング / アンチエイリアス / LCD フィルタ / ガンマ / コントラスト / ウェイト / 太さをスライダーで変えるダイアログ。動かすと下のプレビューが即座に変わる (トレイ自身が描画コアで描く)。「フォント...」でプレビューのフォントを選べる。「適用」で `Custom.ini` に保存し、起動中のアプリにも反映する。`font-tuner.exe --custom` で直接開ける |
| システムフォント | シェルの UI フォントを切替 (BIZ UDPゴシック / BIZ UDゴシック / Noto Sans JP / メイリオ)。初回に元設定を退避し「既定に戻す」で復元 |
| Font-tuner を再起動 | 終了と同じ手順で抜けてから同じ exe を起動し直す (手で終了 → 起動と同じ) |
| 終了 | フックを外して終了 |

トレイアイコンはタスクバーのテーマ (明/暗) に合わせてシルバー / ブラックを自動で切り替える。

## 制限事項

- **Chrome / Edge のレンダラー / GPU プロセスには描画差し替えを適用できない。**
  これらのプロセスは `MITIGATION_FORCE_MS_SIGNED_BINS` (Microsoft 署名必須) により
  未署名 DLL の読み込みを拒否する。
  そのため `RenderCore64.dll` を注入できない。
  crashpad-handler や utility など、この緩和が無い子プロセスには注入される。
  レンダラーに適用するにはブラウザ側で `RendererCodeIntegrityEnabled=0` ポリシーの
  設定が必要 (サンドボックスの保護を下げるため本ソフトでは設定しない)。
- **メッセージポンプを持たないプロセス** (コンソールアプリ / サービス) にはフックが
  発火しないため注入されない。
- **32bit プロセス**には注入されない (64bit 専用ビルド)。
- **自分より高い整合性レベルのプロセス**へは UIPI によりフックメッセージが届かず注入されない。
- **MSI は未署名**のため、インストール時に UAC が「発行元不明」と表示する
  (インストールは中断されない)。

## ビルド

必要なもの: Visual Studio 2022 (C++ デスクトップ ワークロード), Rust (msvc), WiX v6 (`dotnet tool install -g wix`, `wix extension add -g WixToolset.Util.wixext/6.0.2`)

```powershell
git clone --recurse-submodules https://github.com/zero-platform-Lab/font-tuner
cd font-tuner
.\build-msi.ps1     # build-core.ps1 (FreeType の静的ライブラリ) → cargo → wix
```

`build-core.ps1` は upstream のプロジェクトファイルを書き換えず、`build/mactype.props` で include / lib パスを注入する。

## 仕組み

`SetWindowsHookEx(WH_GETMESSAGE)` のフックプロシージャに `RenderCore64.dll` が export する `GetMsgProc` を指定するだけ。
これで GUI を持つ全 64bit プロセスに DLL がマップされ、DLL 側の `DllMain` が自分の隣の `font-tuner.ini` を読んで GDI / DirectWrite をフックする。
Font-tuner 自身は `[UnloadDll]` に載せてあり、描画差し替えの対象外。

Portions of this software are copyright © The FreeType Project (www.freetype.org). All rights reserved.
