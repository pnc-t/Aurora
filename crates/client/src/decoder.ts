// WebCodecs VideoDecoder ラッパー。
// 受信バイナリ形式 (net.rs udp_reader_loop):
//   [0]    flags (bit0=keyframe)
//   [1..3] frame_id (be u16)
//   [3..]  Annex-B H.264 (start code 00 00 00 01 区切りで複数 NALU を含む)

import { requestKeyframe } from "./ipc";

type State = {
  decoder: VideoDecoder | null;
  configured: boolean;
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D | null;
};

const state: State = {
  decoder: null,
  configured: false,
  canvas: null as unknown as HTMLCanvasElement,
  ctx: null,
};

export function initDecoder(canvas: HTMLCanvasElement): void {
  state.canvas = canvas;
  state.ctx = canvas.getContext("2d");
  resetDecoder();
}

export function resetDecoder(): void {
  if (state.decoder && state.decoder.state !== "closed") {
    try { state.decoder.close(); } catch { /* noop */ }
  }
  state.decoder = new VideoDecoder({
    output: onFrame,
    error: (e) => {
      console.error("VideoDecoder error", e);
      state.configured = false;
      void requestKeyframe();
    },
  });
  state.configured = false;
}

function onFrame(frame: VideoFrame): void {
  const canvas = state.canvas;
  const ctx = state.ctx;
  if (!canvas || !ctx) {
    frame.close();
    return;
  }
  if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
    canvas.width = frame.displayWidth;
    canvas.height = frame.displayHeight;
  }
  ctx.drawImage(frame, 0, 0);
  frame.close();
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
    } else if (i + 3 < n && buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 0 && buf[i + 3] === 1) {
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

/** バイナリチャンク 1 個を受け取って decoder に渡す。 */
export function handleVideoChunk(buf: ArrayBuffer): void {
  if (buf.byteLength < 3) return;
  const u8 = new Uint8Array(buf);
  const keyframe = (u8[0] & 1) === 1;
  // u8[1..3] = frame_id (将来 RTT 算出用、いまは未使用)
  const annexB = u8.subarray(3);

  if (!state.decoder) return;

  if (!state.configured) {
    if (!keyframe) {
      // 設定前のキーフレームを待つ。届かなければ要求。
      void requestKeyframe();
      return;
    }
    // SPS を探して codec 文字列を作る
    const nalus = splitNalus(annexB);
    let sps: Uint8Array | null = null;
    for (const nal of nalus) {
      const nalType = nal[0] & 0x1f;
      if (nalType === 7) { sps = nal; break; }
    }
    if (!sps) {
      void requestKeyframe();
      return;
    }
    const codec = codecStringFromSps(sps);
    if (!codec) {
      void requestKeyframe();
      return;
    }
    try {
      state.decoder.configure({
        codec,
        optimizeForLatency: true,
      });
      state.configured = true;
    } catch (e) {
      console.error("configure failed", codec, e);
      return;
    }
  }

  try {
    const chunk = new EncodedVideoChunk({
      type: keyframe ? "key" : "delta",
      timestamp: performance.now() * 1000,
      data: annexB,
    });
    state.decoder.decode(chunk);
  } catch (e) {
    console.error("decode failed", e);
    state.configured = false;
    void requestKeyframe();
  }
}
