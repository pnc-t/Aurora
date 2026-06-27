# プロトコル

Aurora は既存リモートデスクトップ (RDP/VNC/SPICE) に追従せず、独自プロトコルを使います。
2 つのチャネルを役割で分離するのが基本方針です。

| チャネル | トランスポート | 用途 | シリアライズ |
|------|------|------|------|
| 制御 | TCP (順序保証) | 入力・クリップボード・ファイル・統計・各種 RPC | `bincode` |
| 映像/音声 | UDP (低遅延・ロス許容) | H.264 NALU フラグメント / Opus フレーム | 生バイト |

どちらのチャネルも単一の Noise セッション鍵で暗号化します (詳細は [security.md](security.md))。
**制御チャネルに映像を流さない / 映像チャネルに制御メッセージを流さない**ことが
遅延予算の前提です。

定義の出どころ:
- 制御メッセージ … `crates/common/src/proto.rs`
- UDP フレーミング … `crates/common/src/framing.rs`
- 暗号ラッパー・nonce ドメイン … `crates/common/src/noise.rs`

## 接続シーケンス

```
client (initiator)                        host (responder)
   │  TCP connect :7000                        │
   │ ─────────── Noise XX msg1 (e) ──────────▶ │
   │ ◀────── Noise XX msg2 (e, ee, s, es) ──── │
   │ ──────── Noise XX msg3 (s, se, psk) ─────▶ │   ← ハンドシェイク完了
   │ ◀──────────── HostInfo ─────────────────  │   (displays, audio)
   │                                            │
   │ ─ SelectDisplay / Input / ... (制御) ───▶ │
   │ ◀──────────── UDP 映像/音声 ────────────  │   (別ソケット, 同一鍵)
```

- ハンドシェイクは Noise の `XXpsk3` パターン (3 メッセージ往復)。各ステップには
  **10 秒のタイムアウト**があり、無応答接続を掴み続けません。
- ハンドシェイク成立後、ホストは最初に `HostInfo` (ディスプレイ一覧・音声有無) を送ります。
- ホストは同時に**1 接続だけ**受理します (単一接続ガード)。

## 制御チャネル (TCP)

各メッセージは `bincode` でシリアライズし、長さ前置きフレームで送ります。本体は Noise で
暗号化されます (nonce はカウンタ; [security.md](security.md) 参照)。`ControlMsg` の主な
バリアント:

### セッション/映像制御
| メッセージ | 向き | 意味 |
|------|------|------|
| `HostInfo { displays, audio }` | H→C | 接続直後のホスト情報。各 `Display` は `id/width/height/name` に加え、仮想ディスプレイ (IDD/MttVDD) か否かの `is_virtual` を持つ |
| `SelectDisplay { id }` | C→H | キャプチャ対象ディスプレイの切替 |
| `SetVideoParams { fps, max_bitrate_bps }` | C→H | 送出 fps 上限・ビットレート上限の実行時変更 (0 = 無制限) |
| `SetResolution { max_height }` | C→H | 送出映像の最大高さ (px)。ホストが GPU で縮小 (アスペクト比維持) し帯域・負荷を下げる。0 = キャプチャ解像度のまま |
| `SetClientViewport { width, height }` | C→H | クライアント表示領域サイズ (物理 px)。ホストはキャプチャ対象ディスプレイ解像度を最も近い対応モードへ変更する (RDP 風、黒帯解消)。詳細は [architecture.md](architecture.md) |
| `DisplayResized { id, width, height }` | H→C | 解像度変更/ディスプレイ切替の結果、実解像度が変わったことを通知。クライアントは入力スケールとデコーダを更新する |
| `SetVideoPaused { paused }` | C→H | 映像送出の一時停止/再開。マルチタブで非アクティブタブの帯域を止める。再開時は `RequestKeyframe` で復帰 |
| `SetAudioMuted { muted }` | C→H | 音声送出のミュート切替。ミュート中はホストが音声 UDP を送らない |
| `SetCursorOverlay { enabled }` | C→H | 映像へのカーソル合成 ON/OFF。DXGI はカーソルを含めないため、マウスキャプチャ (ポインタロック) 中だけ ON にして焼き込ませる |
| `RequestKeyframe` | C→H | キーフレーム (IDR) 要求。ロス回復用 |
| `Ping { ts_us }` / `Pong { ts_us }` | 双方向 | RTT 計測 |
| `Stats { bitrate_kbps, fps, rtt_us, loss_permille }` | 双方向 | 適応ビットレート制御の入力 |

### 入力
`Input(InputEvent)` で運びます。`InputEvent` は:
`MouseMove { x, y }` / `MouseButton { button, down }` / `MouseWheel { delta, horizontal }` /
`Key { vk, down }` / `MouseMoveRelative { dx, dy }` (Pointer Lock 中の相対移動)。

### クリップボード
`Clipboard { text }` を双方向に送り、テキストクリップボードを同期します。

### ファイル転送 (TCP 制御チャネル上の chunked stream)
`FileOffer { id, name, size }` → `FileChunk { id, offset, data, last }` … →
`FileResult { id, ok, message }`。`FileRequest { id, path }` で取得要求も出せます。

### ドライブ投影 (ProjFS / RDP 風)
クライアント側のフォルダをホストの Explorer に仮想ドライブとして見せる RPC 群です。
ホスト (ProjFS プロバイダ) ⇄ クライアント (実ファイルの持ち主) で次をやり取りします。

| メッセージ | 向き | 意味 |
|------|------|------|
| `FsList { req_id, path, offset }` / `FsListReply { req_id, entries, done, error }` | H→C / C→H | ディレクトリ列挙 (`done=false` で継続) |
| `FsRead { req_id, path, offset, length }` / `FsReadReply { req_id, data, error }` | H→C / C→H | ファイル読み取り (空 `data` かつ `error=None` で EOF) |
| `FsCreate` / `FsWrite` / `FsDelete` / `FsRename` | H→C | ホスト Explorer 側の変更をクライアントへ書き戻す (一方向・応答なし) |

> プロトコル型を変更するときは **host/client 双方の互換性**を確認してください。`bincode` は
> enum のバリアント順 (タグ) に依存するため、新バリアントは**末尾に追加**します
> (`proto.rs` の `MouseMoveRelative` / `SetVideoParams` がその例)。

## 映像/音声チャネル (UDP)

映像と音声は**同一 UDP ソケット・同一 Noise セッション**に相乗りし、暗号化前 plaintext の
先頭 1B のストリームタグ (`STREAM_VIDEO=0` / `STREAM_AUDIO=1`) で多重化します。

### データグラム全体の構造

net 層が 1 データグラムに付けるオーバーヘッドを含めた全体像:

```
[ 平文 nonce 8B ][ ストリームタグ 1B ][ framed payload ][ Poly1305 タグ 16B ]
                 └──────────── ChaCha20-Poly1305 で暗号化 ────────────┘
```

- 先頭の nonce は平文で送り、受信側が復号に使います (ストリームのドメインビットも兼ねる。
  [security.md](security.md) 参照)。
- MTU は Tailscale (WireGuard) のオーバーヘッドを見て安全側に **1200B** を上限とし、
  暗号化後のデータグラムがこれを超えないよう `MAX_PAYLOAD` を逆算しています。

### 映像パケット (framed payload, ヘッダ 7B)

H.264 NALU を MTU に収まるフラグメントへ分割して送ります。

```
[ seq 4B (BE) ][ flags 1B ][ frame_id 2B (BE) ][ payload (NALU fragment) ]
```

`flags` ビット: `KEYFRAME` (0x01) / `FRAME_START` (0x02) / `FRAME_END` (0x04)。
受信側は `frame_id` でフレームを束ね、`FRAME_START`〜`FRAME_END` で 1 フレームを再構成します。
**FEC・再送はありません**。欠落は次のキーフレームで自然回復させ、必要なら制御チャネルの
`RequestKeyframe` で明示要求します (低遅延優先)。

### 音声パケット (framed payload, ヘッダ 4B)

```
[ seq 4B (BE) ][ payload (Opus フレーム) ]
```

1 パケット = 1 Opus フレーム (20ms)。Opus フレームは MTU 内に収まるため分割しません。
`seq` は欠落検出用です。

## デコーダ初期化メモ (クライアント)

WebCodecs の H.264 デコードは `avc1.*` プロファイル文字列が必須です。エンコーダ出力の
SPS/PPS から構築します (`crates/client/src/core/decoder.ts` が参照点)。Tauri の WebView は
すべて WebCodecs 対応のため、H.264 デコードのフォールバックは持ちません。
