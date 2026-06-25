// WebCodecs VideoDecoder ラッパー (1 セッション = 1 インスタンス)。
// 複数タブ (セッション) が 1 枚の canvas を共有するため、描画先 canvas はインスタンス間で
// 同一でよいが、デコーダ状態 (configure / 参照フレーム) はセッションごとに独立させる。
// 非アクティブタブは Session 側で映像チャンクを渡さない (host も一時停止) ため、ここは
// 受け取ったチャンクを淡々とデコードして描画するだけでよい。
//
// 受信バイナリ形式 (net.rs udp_reader_loop):
//   [0]    flags (bit0=keyframe)
//   [1..3] frame_id (be u16)
//   [3..]  Annex-B H.264 (start code 00 00 00 01 区切りで複数 NALU を含む)

import { logger } from "./log";

// --- ジッタバッファ (P2: 滑らかさ) ---
// 旧実装はデコード完了フレームを onFrame で即 drawImage していた。WebCodecs の出力は
// ネットワーク到着ジッタ (パケットが束で来て→疎、を繰り返す) をそのまま引き継ぐため、
// 各フレームの表示時間が不均一になり「ヌルヌル動かずカクつく」体感の一因になっていた。
// ここでは小さなクッション (TARGET_BUFFER_MS) を持ち、到着間隔の推定 (EMA) で**均等間隔**に
// 提示時刻をスケジュールして requestAnimationFrame で出す。これで到着ジッタを吸収し表示が
// 滑らかになる。リフレッシュレート非依存 (時刻ベースなので 60/120/144Hz いずれでも機能)。

/// ジッタ吸収クッションの目標遅延 (ms)。到着ジッタをこの分の先読みで吸収する。操作の
/// 体感遅延 (E2E < 50ms 目標) を圧迫しないよう小さく保つ (≒1.5 フレーム @60fps)。
const TARGET_BUFFER_MS = 24;
/// 提示バックログの上限 (ms)。源 fps > 表示 Hz 等で提示が追いつかないとき、これを超える
/// 古いフレームは捨てて遅延の累積 (バッファブロート) を防ぐ。
const MAX_BUFFER_MS = 80;
/// フレーム間隔の初期推定 (60fps)。実際の到着間隔の EMA で源 fps 変化に追従する。
const DEFAULT_INTERVAL_MS = 1000 / 60;

type DecoderOpts = {
  /** デコード不能 (設定前のキーフレーム待ち / エラー) で復帰を促すための要求口。 */
  requestKeyframe: () => void;
};

export class Decoder {
  private decoder: VideoDecoder | null = null;
  private configured = false;
  private readonly ctx: CanvasRenderingContext2D | null;
  // デコードエラーログのレート制限。壊れたストリームではエラーがフレーム毎に出て IPC 転送が
  // 連発しうるため、最大 1 秒 1 回に間引き、その間に潰した件数を併記する。
  private lastErrLogMs = 0;
  private suppressedErrs = 0;

  // --- ジッタバッファ状態 ---
  // 提示待ちのデコード済みフレーム (到着順 = 提示順)。各フレームに提示予定時刻を持たせる。
  private queue: { frame: VideoFrame; presentAt: number }[] = [];
  // 提示ループの requestAnimationFrame ハンドル (キューが空の間は止める)。
  private rafId: number | null = null;
  // 到着間隔の指数移動平均 (源 fps 推定)。
  private emaIntervalMs = DEFAULT_INTERVAL_MS;
  // 直近フレームの到着時刻 (EMA 更新用)。0 = 未到着。
  private lastArrivalMs = 0;
  // 次フレームに割り当てる提示予定時刻 (均等間隔スケジューリングのアンカー)。
  private nextPresentAt = 0;

  /** デコードパイプラインのエラーを 1 秒 1 回に間引いて記録する (抑制件数を併記)。 */
  private throttledError(msg: string, ...extra: unknown[]): void {
    const now = performance.now();
    if (now - this.lastErrLogMs < 1000) {
      this.suppressedErrs++;
      return;
    }
    if (this.suppressedErrs > 0) {
      logger.error(`${msg} (+${this.suppressedErrs} suppressed in last 1s)`, ...extra);
      this.suppressedErrs = 0;
    } else {
      logger.error(msg, ...extra);
    }
    this.lastErrLogMs = now;
  }

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly opts: DecoderOpts,
  ) {
    this.ctx = canvas.getContext("2d");
    this.reset();
  }

  /** デコーダを作り直す (接続/再接続/タブ復帰時)。次のキーフレームから再構成する。 */
  reset(): void {
    this.flushQueue();
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        /* noop */
      }
    }
    this.decoder = new VideoDecoder({
      output: (frame) => this.onFrame(frame),
      error: (e) => {
        this.throttledError("VideoDecoder error", e);
        this.configured = false;
        this.opts.requestKeyframe();
      },
    });
    this.configured = false;
  }

  /** セッション破棄時に閉じる。 */
  close(): void {
    this.flushQueue();
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        /* noop */
      }
    }
    this.decoder = null;
    this.configured = false;
  }

  /** WebCodecs のデコード出力。即描画せず、均等間隔の提示予定を割り当ててキューに積む。 */
  private onFrame(frame: VideoFrame): void {
    if (!this.canvas || !this.ctx) {
      frame.close();
      return;
    }
    const now = performance.now();

    // 到着間隔の EMA を更新 (源 fps 変化に追従)。大きな gap (ロス回復・再開直後) は外れ値
    // として無視し、推定が暴れないようにする。
    if (this.lastArrivalMs > 0) {
      const dt = now - this.lastArrivalMs;
      if (dt > 0 && dt < 250) {
        this.emaIntervalMs += 0.1 * (dt - this.emaIntervalMs);
      }
    }
    this.lastArrivalMs = now;

    // 提示予定時刻: 前フレームの予定 + 推定間隔 = 均等間隔。クッション分だけ先に倒す。
    // アンダーラン (キュー空) / 遅れすぎ / 先読みしすぎはクッション基準へ貼り直し、提示時刻の
    // ドリフトを是正する。
    const earliest = now + TARGET_BUFFER_MS;
    let presentAt = this.nextPresentAt + this.emaIntervalMs;
    if (this.queue.length === 0 || presentAt < now || presentAt > now + MAX_BUFFER_MS) {
      presentAt = earliest;
    }
    this.nextPresentAt = presentAt;
    this.queue.push({ frame, presentAt });

    // バックログ上限 (源 fps > 表示 Hz でキューが伸びる場合): 古いフレームを捨てて遅延を抑える。
    const maxFrames = Math.max(2, Math.ceil(MAX_BUFFER_MS / this.emaIntervalMs));
    while (this.queue.length > maxFrames) {
      this.queue.shift()?.frame.close();
    }

    this.ensureRafRunning();
  }

  /** 提示ループが止まっていれば起動する。 */
  private ensureRafRunning(): void {
    if (this.rafId === null) {
      this.rafId = requestAnimationFrame(this.presentLoop);
    }
  }

  /**
   * requestAnimationFrame 駆動の提示ループ。提示予定が来たフレームを描画する。表示同期
   * (vsync) に揃うので、到着がジッタっていても提示は均等になる。複数フレームが同時に due に
   * なったら最新の due だけ描き、古い due は捨ててフレッシュさを優先する (追いつき)。
   */
  private presentLoop = (): void => {
    // キューに何か残っていればループ継続。空なら止め、次の onFrame で再起動する。
    this.rafId = this.queue.length > 0 ? requestAnimationFrame(this.presentLoop) : null;
    if (this.queue.length === 0) return;

    const now = performance.now();
    let toDraw: VideoFrame | null = null;
    while (this.queue.length > 0 && this.queue[0].presentAt <= now) {
      const item = this.queue.shift();
      if (!item) break;
      // 既に保持していた古い due フレームは破棄 (追いつきのため最新の due のみ描画)。
      if (toDraw) toDraw.close();
      toDraw = item.frame;
    }
    if (toDraw) this.drawFrame(toDraw);
  };

  /** 1 フレームを canvas へ描画して閉じる。 */
  private drawFrame(frame: VideoFrame): void {
    const canvas = this.canvas;
    const ctx = this.ctx;
    if (!canvas || !ctx) {
      frame.close();
      return;
    }
    if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
      canvas.width = frame.displayWidth;
      canvas.height = frame.displayHeight;
      // 解像度確定/変化時のみ (毎フレームではない)。表示サイズの追跡用に info で残す。
      logger.info(`video size ${frame.displayWidth}x${frame.displayHeight}`);
    }
    ctx.drawImage(frame, 0, 0);
    frame.close();
  }

  /** 提示キューを空にし (保持フレームを閉じ)、ループを止めてスケジューラ状態を初期化する。 */
  private flushQueue(): void {
    if (this.rafId !== null) {
      cancelAnimationFrame(this.rafId);
      this.rafId = null;
    }
    for (const item of this.queue) item.frame.close();
    this.queue = [];
    this.emaIntervalMs = DEFAULT_INTERVAL_MS;
    this.lastArrivalMs = 0;
    this.nextPresentAt = 0;
  }

  /** バイナリチャンク 1 個を受け取って decoder に渡す。 */
  handleChunk(buf: ArrayBuffer): void {
    if (buf.byteLength < 3) return;
    const u8 = new Uint8Array(buf);
    const keyframe = (u8[0] & 1) === 1;
    // u8[1..3] = frame_id (将来 RTT 算出用、いまは未使用)
    const annexB = u8.subarray(3);

    const decoder = this.decoder;
    if (!decoder) return;

    if (!this.configured) {
      if (!keyframe) {
        // 設定前のキーフレームを待つ。届かなければ要求。
        this.opts.requestKeyframe();
        return;
      }
      // SPS を探して codec 文字列を作る
      const nalus = splitNalus(annexB);
      let sps: Uint8Array | null = null;
      for (const nal of nalus) {
        const nalType = nal[0] & 0x1f;
        if (nalType === 7) {
          sps = nal;
          break;
        }
      }
      if (!sps) {
        this.opts.requestKeyframe();
        return;
      }
      const codec = codecStringFromSps(sps);
      if (!codec) {
        this.opts.requestKeyframe();
        return;
      }
      try {
        decoder.configure({ codec, optimizeForLatency: true });
        this.configured = true;
        // 構成は 1 セッション 1 回 (キーフレーム待ち成立時) のみ。低頻度なので info で残す。
        logger.info(`decoder configured: ${codec}`);
      } catch (e) {
        this.throttledError("configure failed", codec, e);
        return;
      }
    }

    try {
      const chunk = new EncodedVideoChunk({
        type: keyframe ? "key" : "delta",
        timestamp: performance.now() * 1000,
        data: annexB,
      });
      decoder.decode(chunk);
    } catch (e) {
      this.throttledError("decode failed", e);
      this.configured = false;
      this.opts.requestKeyframe();
    }
  }
}

/** Annex-B から NALU 単位に分割。start code (3 or 4 byte) をスキップ。 */
function splitNalus(buf: Uint8Array): Uint8Array[] {
  const out: Uint8Array[] = [];
  const n = buf.length;
  let i = 0;
  let nalStart = -1;
  while (i < n) {
    // 0 0 0 1 もしくは 0 0 1 を探す
    if (i + 2 < n && buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 1) {
      if (nalStart >= 0) out.push(buf.subarray(nalStart, i));
      i += 3;
      nalStart = i;
    } else if (
      i + 3 < n &&
      buf[i] === 0 &&
      buf[i + 1] === 0 &&
      buf[i + 2] === 0 &&
      buf[i + 3] === 1
    ) {
      if (nalStart >= 0) out.push(buf.subarray(nalStart, i));
      i += 4;
      nalStart = i;
    } else {
      i += 1;
    }
  }
  if (nalStart >= 0 && nalStart < n) out.push(buf.subarray(nalStart, n));
  return out;
}

/** SPS の先頭 4 バイトから avc1.* codec 文字列を構築 (profile/constraint/level)。 */
function codecStringFromSps(sps: Uint8Array): string | null {
  // sps[0] = nal_unit_header, sps[1]=profile_idc, sps[2]=constraint_set_flags, sps[3]=level_idc
  if (sps.length < 4) return null;
  const profile = sps[1].toString(16).padStart(2, "0");
  const constraints = sps[2].toString(16).padStart(2, "0");
  const level = sps[3].toString(16).padStart(2, "0");
  // fourCC "avc1" は小文字必須 (Chromium の codec パーサは大小区別する)。
  // 16 進部分は大小どちらも可だが慣例の小文字に揃える。
  return `avc1.${profile}${constraints}${level}`.toLowerCase();
}
