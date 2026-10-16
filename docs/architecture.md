# アーキテクチャ

Aurora は「**Port & Adapter (Hexagonal)**」風の構成です。OS 依存部はトレイト (Port) で
定義し、OS 別実装 (Adapter) を `cfg` で切り替えます。`common` がドメイン (プロトコル型・
暗号化抽象)、`host` / `client` がアプリケーション層という分担です。

## クレート構成

```
crates/
├── common/     共有: プロトコル型 (proto)、Noise 暗号化 (noise)、
│               UDP/TCP フレーミング (framing)、クリップボード (clipboard)
├── host/       操作される側 (サーバ・素のバイナリ)
│   ├── capture/   画面キャプチャ Port + OS 別 Adapter (device/convert を含む)
│   ├── encode/    映像エンコード Port + Adapter (mf=SW H.264 / mf_hw=HW H.264/HEVC/AV1 / ffmpeg)。
│   │              コーデックネゴシエーションの「host 側能力」プローブもここ (probe_encoders)
│   ├── input/     マウス/キー入力注入 Port + OS 別 Adapter
│   ├── audio/     ループバック音声取り込み + Opus エンコード
│   ├── projfs/    ProjFS によるドライブ投影 (クライアント側ファイルをホストに見せる)
│   ├── display/   ディスプレイ解像度の動的変更 (RDP 風) / 切断時の復元 (mod.rs) +
│   │              仮想ディスプレイ (MttVDD/IddCx) 検出・テイクオーバー制御 (virtual_display.rs)
│   ├── net/       TCP 制御 + UDP 映像/音声のサーバ実装。bind/accept/セッション/適応制御/
│   │              認証/コーデックネゴシエーション/ファイル I/O を関心事別サブモジュール化
│   │              (mod.rs が起動配線を担い auth/bitrate/client/codec/files/frame/session を束ねる)
│   ├── security/  接続を許可する相手の制御 (secret=パスワードの DPAPI 暗号保存 /
│   │              devices=デバイスピン留め=クライアント静的鍵の許可リスト)
│   ├── system/    OS 統合とローカル永続化 (settings=待受ポート等の平文保存 /
│   │              state=%ProgramData%\Aurora\host の解決と ACL 保護 /
│   │              service=SYSTEM サービスと対話セッションへの本体・トレイ起動 /
│   │              desktop=UAC・ロック画面のデスクトップ追従 / user=ログオンユーザー偽装 /
│   │              autostart=旧方式のログオン時自動起動タスクの登録・解除)
│   ├── perf.rs    エンコード遅延・ソースビットレートの計測 (--perf)
│   ├── video/     画面取り込み+エンコードを子プロセス (取り込みワーカー) で動かす
│   │              (supervisor=本体側の起動・監視・中継 / worker=子側 / wire=パイプの枠付け)。
│   │              DXGI の固着はワーカーだけ作り直し、クライアントとの接続は切らない
│   ├── tray/      トレイ (mod=状態バス / control=操作と特権ガード / ui=Win32 /
│   │              ipc=サービス運用時の単独トレイ (管理者の昇格権限) との名前付きパイプ)
│   └── main.rs    エントリ・CLI・スレッド配線
├── wol/        WoL 中継デーモン (aurora-wol、素のバイナリ)。自宅 LAN の Raspberry Pi 等で
│               常駐し、Tailnet 経由の client からの `WolMsg::Wake` を Noise ハンドシェイク後に
│               受けてマジックパケットを送出する。host/client のセッションとは独立プロセス
└── client/     操作する側 (Tauri v2 + Vanilla TypeScript)
    ├── src-tauri/  Rust 側: ネットワーク、Webview への NALU 受け渡し、設定
    └── src/        フロント
        ├── core/   ブリッジ・ランタイム (a11y / codec / decoder / fullscreen / input / ipc /
        │           log / session / shortcuts / video-presets / webview-guard)
        ├── ui/     UI コンポーネント (avatar / connect-dialog / file-transfer / icons /
        │           launcher / reconnect-badge / stats-overlay / tabs / toast / toolbar /
        │           window-controls / wol-panel)
        └── styles/ 横断スタイル (base.css)
```

`net/` `display/` `security/` `system/` は Port ではなく「関心事グルーピング dir」
(役割が近く単独では小さいモジュールを 1 dir に束ねたもの) です。`mod.rs` が `pub(crate) use`
で平坦再エクスポートし、呼び出し側は `crate::net::run_server` のように旧単一ファイル時代と
同じ外部パスで参照できます。

クレート間の依存は一方向です: `host` / `client` / `wol` がそれぞれ `common` に依存し、
互いには依存しません。host↔client は `common` の `ControlMsg` ワイヤプロトコルで、
client↔wol は同じく `common` の `WolMsg` (別チャネル・別プロトコル) で結ばれます。

## データフロー (全体像)

```
[host]                                            [client]
 画面 ──capture──▶ GPU テクスチャ                  WebCodecs VideoDecoder ──▶ <canvas>
                     │ (CPU コピーなし)                     ▲
                     ▼                                      │ NALU/OBU (binary Channel)
                  encode (H.264/HEVC/AV1)               src-tauri (Rust)
                     │ EncodedNal                            ▲
                     ▼                                      │ UDP 映像/音声 (Noise 暗号化)
                  net/ ──UDP──────────────────────────────────┘
                     ▲
                     │ TCP 制御 (入力/クリップボード/ファイル/統計/コーデックネゴ, Noise 暗号化)
                     ▼
                  input 注入 ◀── マウス/キー ─────────── input.ts (フロント)
```

- **映像**: capture → encode → net は GPU テクスチャを CPU メモリへコピーせず、
  Windows では DXGI Desktop Duplication → Media Foundation を GPU 上で完結させる方針です。
- **コーデック**: 送出コーデックは H.264/HEVC/AV1 からネゴシエーションで決まります
  (host 能力 ∩ 全クライアント申告の共通集合、優先順位 AV1 > HEVC > H264)。詳細は
  [protocol.md](protocol.md) の「コーデックネゴシエーション」を参照。
- **役割分離**: 制御は TCP (順序保証)、映像/音声は UDP (低遅延・ロス許容) に分け、
  遅延予算の根拠にしています。詳細なワイヤフォーマットは [protocol.md](protocol.md) を参照。

## ホスト側のタスク分割 (アクター風)

ホストは各サブシステムを独立スレッド/タスクにし、`tokio::sync::mpsc` でメッセージ交換します。
共有可変状態 (`Arc<Mutex<T>>`) は最小化しています (`main.rs` の `run_server` が配線元)。

| スレッド/タスク | 役割 | 入出力 |
|------|------|------|
| `capture` | 選択ディスプレイをキャプチャし、fps 上限でペーシング | → `Frame` チャネル |
| `encode` | H.264/HEVC/AV1 エンコード (ネゴシエーション結果に従う)、キーフレーム要求/ビットレート変更/コーデック変更を反映 | `Frame` → `EncodedNal` チャネル、実効コーデックは `CodecStatus` で net へ報告 |
| `input` | `SendInput` 等で入力注入 (同期 API なので tokio の外) | `InputCmd` チャネル ← |
| audio capture | WASAPI ループバック → Opus | → `EncodedAudio` チャネル |
| `net` (tokio, `net/`) | TCP 制御受信 + UDP 映像/音声送信、ハンドシェイク、適応制御、コーデックネゴシエーション | 各チャネルを束ねる |
| `tray` (UI) | システムトレイ常駐、パスワード/ポート変更を net へ反映 | `Arc` 共有 + `Notify` |

ポイント:

- **ディスプレイ切替**は制御スレッド → capture スレッドへ id を送り、capturer を作り直します。
- **fps 上限 / ビットレート上限**はクライアントの `SetVideoParams` で実行時に変更でき、
  それぞれ capture スレッド (ペーシング) と encode スレッド (エンコーダ目標) に伝わります。
- **コーデックネゴシエーション**は `net/codec.rs` の `negotiate_codec` (純関数) が host 能力
  (`encode::probe_encoders` で起動時に 1 度プローブ) と全クライアントの `SetSupportedCodecs`
  申告から決めます。エンコーダは全クライアントで共有 (単一エンコーダを全クライアントが対応
  できる設定に合わせるため、1 クライアントでも非対応のコーデックは選べない) なので、決定結果は
  `VideoCodecChanged` で全クライアントへ配信されキーフレームを強制します。実行時にエンコーダが要求コーデックで失敗したら H.264 へ動的フォールバックし、
  host 能力からそのコーデックを外して再ネゴシエーションします。
- **トレイでのパスワード/ポート変更**は `Arc<Mutex<Psk>>` / `AtomicU16` を書き換え、
  `tokio::sync::Notify` で accept ループに再 bind を促すことで、**再起動なしに反映**されます。
- D3D11 デバイスは capture/convert/encode で**1 つだけ**生成し共有します (Windows)。

## 解像度の追従 (RDP 風) と仮想ディスプレイ

クライアント表示領域とホストのキャプチャ解像度が食い違うと黒帯やスケーリングが生じます。
これを避けるため、クライアントは表示領域サイズを `SetClientViewport` で送り、ホストが
キャプチャ対象ディスプレイを最も近い対応モードへ合わせます ([protocol.md](protocol.md))。

- **物理ディスプレイ** (`display/mod.rs`): 解像度変更は既定 **OFF** のトグル。`ChangeDisplaySettingsEx`
  でモード変更し、切断時は記憶した元モードへ戻します。Win32 呼び出しはブロックしうるので
  `spawn_blocking` 上で実行します。
- **仮想ディスプレイ** (`display/virtual_display.rs`): MttVDD/IddCx 製の仮想モニタを検出し、物理画面を
  触らずに「クライアント表示領域ぴったりの仮想ディスプレイ」をキャプチャするテイクオーバー方式。
  物理を温存したまま 1:1 表示にできます。
- 解像度変更は DXGI Desktop Duplication を `ACCESS_LOST` にしますが、capture スレッドが
  capturer を作り直して新解像度で復帰します。実解像度が変わるとホストは `DisplayResized` を
  返し、クライアントが入力スケールとデコーダを更新します。

## クライアント側

- **src-tauri (Rust)**: TCP/UDP ソケットと Noise ハンドシェイクを担い、復号した NALU/OBU を
  Webview へ**バイナリ Channel** で渡します (JSON エンコードは映像に使わない)。
- **フロント (Vanilla TS)**: `core/codec.ts` が `VideoDecoder.isConfigSupported` で対応コーデック
  をプローブして `SetSupportedCodecs` 申告に使い、`core/decoder.ts` が `VideoCodecChanged` に
  合わせてデコーダを再構成しつつ WebCodecs `VideoDecoder` で `<canvas>` に描画、`core/input.ts`
  がキー/マウスを集めて `invoke` で送信します。`ui/reconnect-badge.ts` は接続監視と自己修復による
  シームレス再接続の UI、`ui/wol-panel.ts` は `HostInfo.mac` を使った WoL 起動 UI です。
  グローバル状態管理ライブラリやコンポーネントフレームワークは使いません (バンドル軽量化)。

## OS 対応の方針

Windows を主対象として完成させ、Mac/Linux は OS ポート (`cfg` 分岐の Adapter 追加) で
まとめて対応します。capture / encode / input はいずれも `mod.rs` のトレイト定義 +
OS 別実装ファイルというパターンで、対応 OS を増やすときの差分が局所化されます。
