// 静かな再接続の常駐バッジ。切断直後〜数回の再接続試行では全面オーバーレイを出さず
// (decoder が保持する最後の映像フレームを canvas に残したまま)、この小さなバッジだけで
// 「再接続中」を知らせる。試行回数が閾値を超えたら session.ts 側が全面オーバーレイへ切り替える
// (このバッジは clear される)。

import "./reconnect-badge.css";

let el: HTMLElement | null = null;

export function initReconnectBadge(root: HTMLElement): void {
  el = root;
}

export function showReconnectBadge(text: string): void {
  if (!el) return;
  el.textContent = text;
  el.classList.add("visible");
}

export function hideReconnectBadge(): void {
  if (!el) return;
  el.classList.remove("visible");
  el.replaceChildren();
}
