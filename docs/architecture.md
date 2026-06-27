# アーキテクチャ

Aurora は「**Port & Adapter (Hexagonal)**」風の構成です。OS 依存部はトレイト (Port) で
定義し、OS 別実装 (Adapter) を `cfg` で切り替えます。`common` がドメイン (プロトコル型・
暗号化抽象)、`host` / `client` がアプリケーション層という分担です。

## クレート構成

```
crates/
├── common/     共有: プロトコル型 (proto)、Noise 暗号化 (noise)、
│               UDP フレーミング (framing)、クリップボード (clipboard)
├── host/       操作される側 (サーバ・素のバイナリ)
│   ├── capture/   画面キャプチャ Port + OS 別 Adapter (device/convert を含む)
│   ├── encode/    H.264 エンコード Port + Adapter (mf=SW / mf_hw=HW / ffmpeg)
│   ├── input/     マウス/キー入力注入 Port + OS 別 Adapter
│   ├── audio/     ループバック音声取り込み + Opus エンコード
│   ├── projfs/    ProjFS によるドライブ投影 (クライアント側ファイルをホストに見せる)
│   ├── display.rs ディスプレイ解像度の動的変更 (RDP 風) / 切断時の復元
│   ├── virtualdisplay.rs  仮想ディスプレイ (MttVDD/IddCx) 検出・テイクオーバー制御
│   ├── net.rs     TCP 制御 + UDP 映像/音声のサーバ実装
│   ├── conn.rs    接続状態 (単一接続ガード) のプロセス間共有
│   ├── devices.rs デバイスピン留め (クライアント静的鍵の許可リスト)
│   ├── secret.rs  接続パスワードの DPAPI 暗号保存
│   ├── perf.rs    エンコード遅延・ソースビットレートの計測 (--perf)
│   ├── settings.rs / autostart.rs / tray.rs  設定・自動起動タスク・トレイ常駐 UI
│   └── main.rs    エントリ・CLI・スレッド配線
└── client/     操作する側 (Tauri v2 + Vanilla TypeScript)
    ├── src-tauri/  Rust 側: ネットワーク、Webview への NALU 受け渡し、設定
    └── src/        フロント
        ├── core/   ブリッジ・ランタイム (ipc / decoder / input / session /
        │           fullscreen / video-presets / shortcuts / a11y /
        │           webview-guard / log)
        ├── ui/     UI コンポーネント (launcher / tabs / toolbar / connect-dialog /
        │           file-transfer / stats-overlay / toast / window-controls / icons)
        └── styles/ 横断スタイル (base.css)
```

クレート間の依存は一方向です: `host` と `client` がそれぞれ `common` に依存し、
`host` と `client` は互いに依存しません。両者を結ぶのは `common` のワイヤプロトコルだけです。

## データフロー (全体像)

```
[host]                                            [client]
 画面 ──capture──▶ GPU テクスチャ                  WebCodecs VideoDecoder ──▶ <canvas>
                     │ (CPU コピーなし)                     ▲
                     ▼                                      │ NALU (binary Channel)
                  encode (H.264)                       src-tauri (Rust)
                     │ NALU                                 ▲
                     ▼                                      │ UDP 映像/音声 (Noise 暗号化)
                  net.rs ──UDP──────────────────────────────┘
                     ▲
                     │ TCP 制御 (入力/クリップボード/ファイル/統計, Noise 暗号化)
                     ▼
                  input 注入 ◀── マウス/キー ─────────── input.ts (フロント)
```

- **映像**: capture → encode → net は GPU テクスチャを CPU メモリへコピーせず、
  Windows では DXGI Desktop Duplication → Media Foundation を GPU 上で完結させる方針です。
- **役割分離**: 制御は TCP (順序保証)、映像/音声は UDP (低遅延・ロス許容) に分け、
  遅延予算の根拠にしています。詳細なワイヤフォーマットは [protocol.md](protocol.md) を参照。

## ホスト側のタスク分割 (アクター風)

ホストは各サブシステムを独立スレッド/タスクにし、`tokio::sync::mpsc` でメッセージ交換します。
共有可変状態 (`Arc<Mutex<T>>`) は最小化しています (`main.rs` の `run_server` が配線元)。

| スレッド/タスク | 役割 | 入出力 |
|------|------|------|
| `capture` | 選択ディスプレイをキャプチャし、fps 上限でペーシング | → `Frame` チャネル |
| `encode` | H.264 エンコード、キーフレーム要求/ビットレート変更を反映 | `Frame` → `EncodedNal` チャネル |
| `input` | `SendInput` 等で入力注入 (同期 API なので tokio の外) | `InputCmd` チャネル ← |
| audio capture | WASAPI ループバック → Opus | → `EncodedAudio` チャネル |
| `net` (tokio) | TCP 制御受信 + UDP 映像/音声送信、ハンドシェイク、適応制御 | 各チャネルを束ねる |
| `tray` (UI) | システムトレイ常駐、パスワード/ポート変更を net へ反映 | `Arc` 共有 + `Notify` |

ポイント:

- **ディスプレイ切替**は制御スレッド → capture スレッドへ id を送り、capturer を作り直します。
- **fps 上限 / ビットレート上限**はクライアントの `SetVideoParams` で実行時に変更でき、
  それぞれ capture スレッド (ペーシング) と encode スレッド (エンコーダ目標) に伝わります。
- **トレイでのパスワード/ポート変更**は `Arc<Mutex<Psk>>` / `AtomicU16` を書き換え、
  `tokio::sync::Notify` で accept ループに再 bind を促すことで、**再起動なしに反映**されます。
- D3D11 デバイスは capture/convert/encode で**1 つだけ**生成し共有します (Windows)。

## 解像度の追従 (RDP 風) と仮想ディスプレイ

クライアント表示領域とホストのキャプチャ解像度が食い違うと黒帯やスケーリングが生じます。
これを避けるため、クライアントは表示領域サイズを `SetClientViewport` で送り、ホストが
キャプチャ対象ディスプレイを最も近い対応モードへ合わせます ([protocol.md](protocol.md))。

- **物理ディスプレイ** (`display.rs`): 解像度変更は既定 **OFF** のトグル。`ChangeDisplaySettingsEx`
  でモード変更し、切断時は記憶した元モードへ戻します。Win32 呼び出しはブロックしうるので
  `spawn_blocking` 上で実行します。
- **仮想ディスプレイ** (`virtualdisplay.rs`): MttVDD/IddCx 製の仮想モニタを検出し、物理画面を
  触らずに「クライアント表示領域ぴったりの仮想ディスプレイ」をキャプチャするテイクオーバー方式。
  物理を温存したまま 1:1 表示にできます。
- 解像度変更は DXGI Desktop Duplication を `ACCESS_LOST` にしますが、capture スレッドが
  capturer を作り直して新解像度で復帰します。実解像度が変わるとホストは `DisplayResized` を
  返し、クライアントが入力スケールとデコーダを更新します。

## クライアント側

- **src-tauri (Rust)**: TCP/UDP ソケットと Noise ハンドシェイクを担い、復号した NALU を
  Webview へ**バイナリ Channel** で渡します (JSON エンコードは映像に使わない)。
- **フロント (Vanilla TS)**: `core/decoder.ts` が WebCodecs `VideoDecoder` を初期化して
  `<canvas>` に描画、`core/input.ts` がキー/マウスを集めて `invoke` で送信します。
  グローバル状態管理ライブラリやコンポーネントフレームワークは使いません (バンドル軽量化)。

## OS 対応の方針

Windows を主対象として完成させ、Mac/Linux は OS ポート (`cfg` 分岐の Adapter 追加) で
まとめて対応します。capture / encode / input はいずれも `mod.rs` のトレイト定義 +
OS 別実装ファイルというパターンで、対応 OS を増やすときの差分が局所化されます。
