# プロトコル

Aurora は既存リモートデスクトップ (RDP/VNC/SPICE) に追従せず、独自プロトコルを使います。
2 つのチャネルを役割で分離するのが基本方針です。

| チャネル | トランスポート | 用途 | シリアライズ |
|------|------|------|------|
| 制御 | TCP (順序保証) | 入力・クリップボード・ファイル・統計・各種 RPC | `bincode` |
| 映像/音声 | UDP (低遅延・ロス許容) | H.264/HEVC/AV1 映像フラグメント (コーデックネゴシエーション) / Opus フレーム | 生バイト |

どちらのチャネルも単一の Noise セッション鍵で暗号化します (詳細は [security.md](security.md))。
**制御チャネルに映像を流さない / 映像チャネルに制御メッセージを流さない**ことが
遅延予算の前提です。

定義の出どころ:
- 制御メッセージ (`ControlMsg` / `WolMsg`) … `crates/common/src/proto.rs`
- UDP 映像/音声フレーミング + TCP 制御フレーミング (`read_tcp_frame`/`write_tcp_frame`、
  `send_ctrl_msg`/`decode_ctrl_msg`、`send_wol_msg`/`decode_wol_msg`) … `crates/common/src/framing.rs`
  (host/client 双方が同じ実装を使う。片側だけ変えるとワイヤ互換が静かに壊れるため、
  この 1 箇所に集約されている)
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
- ホストは同時に**最大 4 接続** (`MAX_CLIENTS`) を受理します (セマフォで制限)。上限到達時の
  新規接続は受理直後 (ハンドシェイク前) に切断されます。共有 capture/encode の出力を全接続へ
  ファンアウトするマルチクライアントモデルです。

## 制御チャネル (TCP)

各メッセージは `bincode` でシリアライズし、長さ前置きフレームで送ります。本体は Noise で
暗号化されます (nonce はカウンタ; [security.md](security.md) 参照)。`ControlMsg` の主な
バリアント:

### セッション/映像制御
| メッセージ | 向き | 意味 |
|------|------|------|
| `HostInfo { displays, audio, mac }` | H→C | 接続直後のホスト情報。各 `Display` は `id/width/height/name` に加え、仮想ディスプレイ (IDD/MttVDD) か否かの `is_virtual` を持つ。`mac` はホストのプライマリ NIC の MAC アドレス (取得失敗時は `None`)。クライアントは host エントリに保存し、WoL 中継 (`aurora-wol`) への wake 要求に使う |
| `SelectDisplay { id }` | C→H | キャプチャ対象ディスプレイの切替 |
| `SetVideoParams { fps, max_bitrate_bps }` | C→H | 送出 fps 上限・ビットレート上限の実行時変更 (0 = 無制限) |
| `SetResolution { max_height }` | C→H | 送出映像の最大高さ (px)。ホストが GPU で縮小 (アスペクト比維持) し帯域・負荷を下げる。0 = キャプチャ解像度のまま |
| `SetClientViewport { width, height }` | C→H | クライアント表示領域サイズ (物理 px)。**アクティブが仮想ディスプレイ (MttVDD/IddCx) で、かつ登録済み端末がその仮想を明示選択済みのときだけ**、ホストは仮想ディスプレイをテイクオーバー (物理を全てデタッチし、仮想を primary としてこの viewport に最も近い解像度へ変更) する。物理ディスプレイをキャプチャしている場合は解像度を一切変更しない (はみ出しはクライアント側の object-fit/レターボックスで吸収)。詳細は [architecture.md](architecture.md) |
| `DisplayResized { id, width, height }` | H→C | 仮想ディスプレイのテイクオーバーが成立し実解像度が確定/変化したことを通知 (物理ディスプレイ選択時は送られない)。クライアントは入力スケールとデコーダを更新する |
| `SetVideoPaused { paused }` | C→H | 映像送出の一時停止/再開。マルチタブで非アクティブタブの帯域を止める。再開時は `RequestKeyframe` で復帰 |
| `SetAudioMuted { muted }` | C→H | 音声送出のミュート切替。ミュート中はホストが音声 UDP を送らない |
| `SetCursorOverlay { enabled }` | C→H | 映像へのカーソル合成 ON/OFF。DXGI はカーソルを含めないため、マウスキャプチャ (ポインタロック) 中だけ ON にして焼き込ませる |
| `SetSupportedCodecs { codecs }` | C→H | クライアントが対応する映像コーデックの申告 (`VideoCodec`: H264/Hevc/Av1)。接続直後のハンドシェイク完了直後と、手動コーデック切替時に再申告する。ホストは全クライアント申告の共通集合と自身の能力から送出コーデックを決める (優先順位 AV1 > HEVC > H264、共有エンコーダのため 1 クライアントでも非対応なら選べない) |
| `VideoCodecChanged { codec }` | H→C | 送出コーデックの確定/変更通知。エンコーダの (再) 作成の都度、全クライアントへ送りキーフレームを強制する。途中参加クライアントには `HostInfo` 送信直後にも現在値を送る |
| `RequestKeyframe` | C→H | キーフレーム (IDR) 要求。ロス回復用 |
| `Ping { ts_us }` / `Pong { ts_us }` | 双方向 | RTT 計測 |
| `Stats { bitrate_kbps, fps, rtt_us, loss_permille }` | 双方向 | 適応ビットレート制御の入力 |

仮想ディスプレイのテイクオーバーは**接続時に一度だけ**解像度を確定する。テイクオーバー中に
`SetClientViewport` が再度届いても解像度追従は行わない (Desktop Duplication 中に仮想の解像度を
変えると回復不能な `DXGI_ERROR_ACCESS_LOST` に陥るため)。ウィンドウサイズ差分はクライアント側の
object-fit が吸収し、再フィットしたい場合は再接続する。

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

### WoL 中継 (`WolMsg`、ControlMsg とは別プロトコル)

映像セッションを持たない別クレート `crates/wol` (`aurora-wol`。自宅 LAN の Raspberry Pi 等で
常駐) を起こすための独立したメッセージです。`ControlMsg` とは**別チャネル・別プロトコル**で、
1 接続 = 1 Wake 要求 = 即切断という軽量なラウンドトリップです。ワイヤ形式 (bincode →
CTRL ドメイン nonce で Noise 暗号化 → u16 長さ prefix フレーム) は `ControlMsg` と同じ実装
(`framing.rs` の `send_wol_msg`/`decode_wol_msg`) を共有しますが、混同しないよう型を分けています。

| メッセージ | 向き | 意味 |
|------|------|------|
| `Wake { mac }` | C→W | 対象 MAC アドレスへのマジックパケット送出要求 |
| `Ack { ok, message }` | W→C | 送出結果 |

接続シーケンスは host と同じ Noise `XXpsk3` (3 メッセージ往復) です。クライアントは
`HostInfo.mac` で保存した MAC を使って `aurora-wol` へ接続し `Wake` を送ります。

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

現在ネゴシエーション済みのコーデック (H.264 / HEVC / AV1) のビットストリームを MTU に収まる
フラグメントへ分割して送ります。H.264/HEVC は Annex-B (`00 00 00 01` start code 区切りの
NALU 列)、AV1 は OBU (Open Bitstream Unit) 列です。ヘッダ形式・フラグメント方式はコーデックに
依らず共通です。

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

## コーデックネゴシエーション

- クライアントは起動時に `crates/client/src/core/codec.ts` の `probeSupportedCodecs()` で
  `VideoDecoder.isConfigSupported` を使い、実際にデコード可能なコーデックを優先順 (AV1 >
  HEVC > H264) でプローブし、`SetSupportedCodecs` で申告します。
- ホストは起動時に `crates/host/src/encode/mod.rs` の `probe_encoders` で実エンコード可否を
  1 度だけプローブします (H.264 は SW MFT フォールバックが必ずあるため常に含む)。
- 送出コーデックの決定は `crates/host/src/net/codec.rs` の `negotiate_codec` (純関数) が、
  host 能力と全クライアント申告の共通集合から優先順位で選びます。単一エンコーダを全クライアント
  で共有するため、1 クライアントでも対応しないコーデックは選べません。
- 実行時にエンコーダが要求コーデックで失敗した場合は H.264 へ動的フォールバックし、
  `host_codecs` からそのコーデックを外して再ネゴシエーションします (単調に縮むため必ず収束)。

## デコーダ初期化メモ (クライアント)

WebCodecs はコーデックごとに異なる codec 文字列が必要です (`crates/client/src/core/decoder.ts`
が参照点)。

- **H.264**: `avc1.*` 形式。エンコーダ出力の SPS (`profile_idc`/`constraint_set_flags`/
  `level_idc`) から構築します (`codecStringFromH264Sps`)。SPS 未着時はキーフレームを要求して待ちます。
- **HEVC**: `hvc1.*` 形式。SPS (nal_type 33) の `profile_tier_level` 先頭固定部だけを読む
  最小パーサ (`codecStringFromHevcSps`) で構築します。パース失敗時は汎用文字列
  (`hvc1.1.6.L120.b0`) にフォールバックし、H.264 と異なり再要求ループには入りません。
- **AV1**: `av01.*` 形式。OBU 列から sequence header OBU (obu_type=1) を探し、AV1 仕様
  5.5.1 に沿った最小パーサ (`parseAv1SequenceHeader`) で `profile`/`level`/`tier`/`bitDepth`
  を読んで構築します。パース失敗時は汎用文字列 (`av01.0.08M.08`) にフォールバックします。

Tauri の WebView はすべて WebCodecs 対応のため、いずれのコーデックもフォールバックデコーダ
(ソフトウェア実装の自前デコード等) は持ちません。
