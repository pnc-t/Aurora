// 統計 HUD (Parsec/Moonlight 風)。右上に半透明で fps/bitrate/rtt/loss を表示。
// 既定はオフで、Ctrl+Alt+Shift+S のローカルホットキーで開閉する (main.ts が配線)。

import "./stats-overlay.css";
import type { CodecId } from "../core/codec";
import type { StatsPosition } from "../core/ipc";

type Stats = {
  fps: number;
  bitrate_kbps: number;
  rtt_us: number;
  loss_permille: number;
};

const CODEC_LABEL: Record<CodecId, string> = {
  h264: "H.264",
  hevc: "HEVC",
  av1: "AV1",
};

let hudEl: HTMLElement | null = null;
let visible = false;
let latest: Stats | null = null;
let latestCodec: CodecId | null = null;

export function initStatsOverlay(el: HTMLElement): void {
  hudEl = el;
  // 位置は settings 反映前の既定として右上にしておく (旧来の固定配置と同じ見え方)。
  el.classList.add("pos-tr");
}

/** 統計 HUD の表示位置 (画面の隅) を切り替える。mount 時 + 設定変更時に呼ぶ。 */
export function setStatsPosition(pos: StatsPosition): void {
  if (!hudEl) return;
  hudEl.classList.remove("pos-tl", "pos-tr", "pos-bl", "pos-br");
  hudEl.classList.add(`pos-${pos}`);
}

export function toggleStats(): void {
  visible = !visible;
  if (hudEl) hudEl.classList.toggle("visible", visible);
  render();
}

export function isStatsVisible(): boolean {
  return visible;
}

export function updateStats(s: Stats, codec?: CodecId): void {
  latest = s;
  if (codec !== undefined) latestCodec = codec;
  render();
}

/** 接続終了時に内容をクリア。 */
export function clearStats(): void {
  latest = null;
  latestCodec = null;
  render();
}

function render(): void {
  if (!hudEl || !visible) return;
  if (!latest) {
    hudEl.textContent = "waiting for stats…";
    return;
  }
  const { fps, bitrate_kbps, rtt_us, loss_permille } = latest;
  const rttMs = (rtt_us / 1000).toFixed(1);
  const mbps = (bitrate_kbps / 1000).toFixed(1);
  // 値の桁数が変わっても右端が揃うよう、ラベルは左詰め・値は右詰めの固定幅で組む
  // (#stats-hud は monospace + white-space:pre)。
  const rows: [string, string][] = [
    ["fps", String(fps)],
    ["rate", `${mbps} Mbps`],
    ["rtt", `${rttMs} ms`],
    ["loss", `${(loss_permille / 10).toFixed(1)} %`],
  ];
  if (latestCodec) rows.push(["codec", CODEC_LABEL[latestCodec]]);
  hudEl.textContent = rows.map(([k, v]) => `${k.padEnd(5)}${v.padStart(9)}`).join("\n");
}
