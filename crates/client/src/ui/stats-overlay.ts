// 統計 HUD (Parsec/Moonlight 風)。右上に半透明で fps/bitrate/rtt/loss を表示。
// 既定はオフで、Ctrl+Alt+Shift+S のローカルホットキーで開閉する (main.ts が配線)。

import "./stats-overlay.css";

type Stats = {
  fps: number;
  bitrate_kbps: number;
  rtt_us: number;
  loss_permille: number;
};

let hudEl: HTMLElement | null = null;
let visible = false;
let latest: Stats | null = null;

export function initStatsOverlay(el: HTMLElement): void {
  hudEl = el;
}

export function toggleStats(): void {
  visible = !visible;
  if (hudEl) hudEl.classList.toggle("visible", visible);
  render();
}

export function isStatsVisible(): boolean {
  return visible;
}

export function updateStats(s: Stats): void {
  latest = s;
  render();
}

/** 接続終了時に内容をクリア。 */
export function clearStats(): void {
  latest = null;
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
  hudEl.textContent =
    `fps   ${fps}\n` +
    `rate  ${mbps} Mbps\n` +
    `rtt   ${rttMs} ms\n` +
    `loss  ${(loss_permille / 10).toFixed(1)} %`;
}
