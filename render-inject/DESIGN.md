# render-inject — 設計と経緯

ツールのシステム全体側。`RenderCore64.dll` を全プロセスに注入し、テキスト
描画を横取りして Windows 標準のラスタライザではなく `render-core` で描く。
ここが難しく危険な部分で、コードは**マシン上の全プロセスの中で動く**ため、
不具合はアプリを落とすかデスクトップを不安定にする。

現在は**実装・出荷済み**（統合された最新像は `docs/RUST-PORT.md`）。以下の
段階的ロードマップは経緯として残す。各段階は次に進む前にビルドして検証した。

## 目標アーキテクチャ

```
render-core (lib)          描画の頭脳（数式に対して検証済み）
      ^ リンク
render-inject (cdylib DLL)  RenderCore64.dll — 各プロセスに注入される
  - DllMain: アタッチ時にフックを張る（ローダーロックの外で）
  - ExtTextOutW（GDI テキスト API）を retour のインライン detour で（純 Rust）
  - DirectWrite（IDWriteBitmapRenderTarget / IDWriteFontFace）を COM vtable で
  - 横取りした描画ごとに: shape → render-core → 対象 DC/DIB へ blit
```

他プロセスへの注入は既存の loader 設計を再利用する（トレイが `WH_GETMESSAGE`
フックを張り、そのプロシージャがこの DLL にある）。上流が `CreateProcess` を
横取りして子プロセスに送り込むブートストラップ DLL は移植しない（docs/SPEC.md
1.1）。このクレートは描画コア本体であり、注入機構ではない。

## 段階（各段階を次の前に検証した）

1. **骨組み（完了）:** cdylib がビルドでき、`DllMain` が TRUE を返し、テスト
   用 export が `render-core` を呼ぶ。フックなし。安全。
2. **GDI キャプチャ（オフライン）:** *単一のテストプロセスだけ*で `ExtTextOutW`
   を横取りし、引数（文字列・DC・位置・フォント）を記録する。出力はまだ変え
   ない。MacType の入力を再構成できるか確認する。
3. **GDI 書き戻し（単一プロセス）:** `render-core` で描いて DC の DIB へ blit し、
   同じアプリ内で C++ コアと画面上で比較する。まだ opt-in、1 プロセス、止め
   やすい状態。
4. **注入（少数プロセス）:** 小さな許可リスト（例: notepad）だけで loader 経路
   を有効にする。システム全体では絶対に有効化せず、確実なキルスイッチを持つ。
5. **DirectWrite:** COM vtable の横取り。最難関でバージョン依存。
6. 慎重に**対象を広げる**。

## リスク・ルール

- ローダーロックの下で LoadLibrary しない（DllMain からスレッドを起こす）
- アタッチ時に自己を常駐固定する（GetModuleHandleEx FLAG_PIN）。動作中のプロ
  セスから決してアンマップされないので、アンマップ後にコードが走ることはない。
  DllMain の DETACH は何もしない
- `GetMsgProc` は RVA 0x1000 から動かさない（`build.rs`、リンカ `/ORDER`）。
  常駐固定のため、更新後も動作中プロセスは前のビルドを保持し、トレイのフックは
  その中で `old_base + RVA` に解決される。RVA がずれると全 GUI プロセスが一斉に
  落ちた（2026-09-22、lib.rs の分割）。`build.rs` / `order.txt` を消さない。
  `build-msi.ps1` とトレイの両方が検証する
- retour はパッチ中に他スレッドを止めないので、バイトパッチの前後で他スレッド
  を凍結する。一度きりの vtable パッチはミューテックスで直列化する
- 各段階は実証されるまで opt-in かつプロセス限定にする。検証済みのキルスイッチ
  なしにシステム全体で有効にしない
- Chrome/Edge のレンダラー・GPU プロセスは到達不能のまま（MS 署名バイナリ必須
  の緩和）。C++ コアと同じ制限

## 状態

以下の段階はすべて実装済み（GDI + DirectWrite 横取り・書き戻し・フォント解決・クロスプロセスと WH_GETMESSAGE 注入・プロファイル読み込み・アンロードの代わりの常駐固定）。最新の統合像は `docs/RUST-PORT.md`、単一プロセスで試すなら
`../loader` を参照。各段階を積み上げた probe/window クレートは、成果がこの DLL
に落ちた時点で削除した。残りは忠実性・堅牢性（RUST-PORT.md の「Not done」）。
