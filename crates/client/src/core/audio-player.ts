// モバイル音声再生 (方式A: WebView 再生)。
// Rust 側 (src-tauri/src/audio.rs #[cfg(mobile)]) は Opus フレームをデコードせず、そのまま
// バイナリ Channel でここへ転送する (NDK は cpal/opus をクロスコンパイルできないため)。
// ここで WebCodecs `AudioDecoder` (codec="opus") でデコードし、Web Audio (GainNode) で再生する。
//
// host 側のエンコードパラメータ (crates/host/src/audio/mod.rs) と一致させる必要がある:
// SAMPLE_RATE=48000, CHANNELS=2 (stereo), 20ms/フレーム (960 サンプル/ch)。ここが食い違うと
// AudioDecoder の configure が失敗するか、再生速度がずれる。

import { logger } from "./log";

/** host 側エンコーダと同じサンプリングレート (Hz)。 */
const SAMPLE_RATE = 48_000;
/** host 側エンコーダと同じチャンネル数 (ステレオ固定)。 */
const CHANNELS = 2;

/**
 * 再生開始時に確保する先読み (ms)。小さいほど低遅延だが、到着ジッタでアンダーラン
 * (次フレームが間に合わず無音が挟まる) しやすくなる。本プロジェクトは低遅延最優先の
 * 設計目標 (CLAUDE.md §1) だが、音声は映像と違い 1 サンプルの途切れも聴感上目立つため、
 * 決め打ちで映像より少し厚めの 40ms を持つ (適応化はスコープ外、映像ジッタバッファほどの
 * 精密さは要求しない)。
 */
const MIN_LEAD_MS = 40;
/**
 * スケジュール済み再生キューがこれを超えて溜まったら先頭へ巻き戻す (backlog 破棄)。
 * ネットワーク遅延の蓄積をここで断ち切らないと、無音区間明けに音声が「早送り」で
 * 遅延分をまとめて再生する事故になる (低遅延優先のため、遅延回収より新しさを優先する)。
 */
const MAX_QUEUE_MS = 150;

/**
 * モバイル向け Opus 音声プレイヤー。WebCodecs `AudioDecoder` でデコードし、
 * `AudioContext` + `GainNode` で再生する。1 セッション = 1 インスタンス。
 */
export class AudioPlayer {
  private decoder: AudioDecoder | null = null;
  private ctx: AudioContext | null = null;
  private gain: GainNode | null = null;
  /** 次にスケジュールする再生フレームの開始時刻 (AudioContext.currentTime 基準)。 */
  private nextStartTime = 0;
  private volume = 1.0;
  private closed = true;
  /** EncodedAudioChunk の timestamp (us)。単調増加であればよく、再生タイミングには使わない。 */
  private chunkTimestampUs = 0;

  /**
   * デコーダと AudioContext を初期化する。AudioContext はユーザージェスチャ起点でないと
   * 開始しない仕様のため、接続操作 (ボタン押下) の呼び出しチェーン内で呼ぶこと。
   */
  async start(): Promise<void> {
    // 再生中の再入では時刻系をリセットしない。呼び出し側 (mobile-main.ts) は session.ts の
    // setAudioAvailable 経由で 1 接続あたり複数回ここへ来るが、chunkTimestampUs を巻き戻すと
    // EncodedAudioChunk の timestamp 単調増加が破れて AudioDecoder がエラーを吐く。
    if (!this.closed && this.decoder?.state === "configured" && this.ctx) {
      if (this.ctx.state === "suspended") {
        try {
          await this.ctx.resume();
        } catch (e) {
          logger.warn("audio context resume failed", e);
        }
      }
      return;
    }
    this.closed = false;
    this.chunkTimestampUs = 0;
    this.nextStartTime = 0;
    if (!this.ctx) {
      this.ctx = new AudioContext({ sampleRate: SAMPLE_RATE });
      this.gain = this.ctx.createGain();
      this.gain.gain.value = this.volume;
      this.gain.connect(this.ctx.destination);
    }
    if (this.ctx.state === "suspended") {
      try {
        await this.ctx.resume();
      } catch (e) {
        logger.warn("audio context resume failed", e);
      }
    }
    if (!this.decoder || this.decoder.state === "closed") {
      this.decoder = new AudioDecoder({
        output: (data) => this.onAudioData(data),
        error: (e) => logger.warn("audio decode error", e),
      });
      this.decoder.configure({
        codec: "opus",
        sampleRate: SAMPLE_RATE,
        numberOfChannels: CHANNELS,
      });
    }
  }

  /** 再生ゲインを設定する (0.0〜1.0)。host へは送らないローカル適用のみ。 */
  setVolume(volume: number): void {
    this.volume = Math.max(0, Math.min(1, volume));
    if (this.gain) this.gain.gain.value = this.volume;
  }

  /** host から届いた Opus フレーム 1 個 (生バイト列) をデコードキューへ渡す。 */
  push(buf: ArrayBuffer): void {
    if (this.closed || !this.decoder || this.decoder.state !== "configured") return;
    try {
      const chunk = new EncodedAudioChunk({
        // Opus はフレーム間参照が無い (各フレーム独立デコード可能) ため常に "key" 扱いでよい。
        type: "key",
        timestamp: this.chunkTimestampUs,
        data: buf,
      });
      this.chunkTimestampUs += 20_000; // 20ms/フレーム固定 (host 側と同じ)
      this.decoder.decode(chunk);
    } catch (e) {
      logger.warn("audio decode enqueue failed", e);
    }
  }

  /** デコード出力 1 フレームを Web Audio へスケジュールする。 */
  private onAudioData(data: AudioData): void {
    if (this.closed || !this.ctx || !this.gain) {
      data.close();
      return;
    }
    const frames = data.numberOfFrames;
    const buffer = this.ctx.createBuffer(CHANNELS, frames, SAMPLE_RATE);
    for (let ch = 0; ch < CHANNELS; ch++) {
      const channelData = buffer.getChannelData(ch);
      // AudioData のチャンネル数が host と食い違う場合の保険 (モノラルへフォールバック等)。
      data.copyTo(channelData, { planeIndex: Math.min(ch, data.numberOfChannels - 1) });
    }
    data.close();

    const now = this.ctx.currentTime;
    const queuedMs = (this.nextStartTime - now) * 1000;
    // 初回、またはアンダーラン (再生予定時刻を過ぎた)、または backlog 過多のいずれかで
    // 先読みクッションから再スケジュールする (遅延を溜め込まず「今」に追従する)。
    if (queuedMs < 0 || queuedMs > MAX_QUEUE_MS) {
      this.nextStartTime = now + MIN_LEAD_MS / 1000;
    }

    const src = this.ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(this.gain);
    src.start(this.nextStartTime);
    this.nextStartTime += frames / SAMPLE_RATE;
  }

  /** 再生を止め、AudioDecoder/AudioContext を確実に閉じる (リークさせない)。 */
  stop(): void {
    this.closed = true;
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        /* noop */
      }
    }
    this.decoder = null;
    if (this.ctx) {
      void this.ctx.close().catch(() => {
        /* noop */
      });
    }
    this.ctx = null;
    this.gain = null;
    this.nextStartTime = 0;
  }
}
