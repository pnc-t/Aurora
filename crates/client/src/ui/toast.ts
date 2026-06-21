// 一時通知トースト。右下スタックに数秒表示して自動消滅する。
// 恒久ステータス (接続ラベル) や統計 HUD とは分離し、重要な一時メッセージ
// (エラー / ディスプレイ切替 / ファイル結果 / 切断) が即上書きされないようにする。

import "./toast.css";

export type ToastKind = "info" | "success" | "error";

let container: HTMLElement | null = null;

export function initToast(root: HTMLElement): void {
  container = root;
}

export function showToast(message: string, level: ToastKind = "info", durationMs = 4000): void {
  if (!container) return;
  const el = document.createElement("div");
  el.className = `toast ${level}`;
  el.textContent = message;
  container.appendChild(el);

  // 次フレームで show を付けてフェードイン。
  requestAnimationFrame(() => el.classList.add("show"));

  window.setTimeout(() => {
    el.classList.remove("show");
    // トランジション終了後に DOM から除去。
    el.addEventListener("transitionend", () => el.remove(), { once: true });
    // 念のためのフォールバック除去。
    window.setTimeout(() => el.remove(), 400);
  }, durationMs);
}
