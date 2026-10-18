// 映像コーデック能力プローブ + codec 文字列構築ヘルパー。
// host とのネゴで H.264/HEVC/AV1 のいずれかになる。実際にブラウザ (WebCodecs) が対応するかは
// VideoDecoder.isConfigSupported で判定する以外に手段が無いため、起動時に 1 回だけプローブして
// キャッシュする (対応状況は実行中に変わらない前提)。

import { logger } from "./log";

/** host と交渉しうる映像コーデック。 */
export type CodecId = "h264" | "hevc" | "av1";

/** 設定で選べる値 ("auto" はプローブ結果全部を申告する)。 */
export type CodecChoice = "auto" | CodecId;

/** プローブに使う代表 codec 文字列 (優先順: av1 > hevc > h264)。 */
const PROBE_CODECS: { id: CodecId; codec: string }[] = [
  { id: "av1", codec: "av01.0.08M.08" },
  { id: "hevc", codec: "hvc1.1.6.L120.90" },
  { id: "h264", codec: "avc1.640028" },
];

let cached: CodecId[] | null = null;
let inFlight: Promise<CodecId[]> | null = null;

/**
 * ブラウザ (WebCodecs) が実際にデコード対応するコーデックを優先順で返す。
 * 1 回だけプローブしてモジュールスコープにキャッシュする。例外は非対応扱い。
 */
export async function probeSupportedCodecs(): Promise<CodecId[]> {
  if (cached) return cached;
  if (inFlight) return inFlight;
  inFlight = (async () => {
    const out: CodecId[] = [];
    for (const p of PROBE_CODECS) {
      try {
        const support = await VideoDecoder.isConfigSupported({ codec: p.codec });
        if (support.supported) out.push(p.id);
      } catch (e) {
        logger.warn(`codec probe failed: ${p.id}`, e);
      }
    }
    cached = out;
    inFlight = null;
    return out;
  })();
  return inFlight;
}
