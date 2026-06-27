// 一時通知トースト。右下スタックに数秒表示して自動消滅する。
// 恒久ステータス (接続ラベル) や統計 HUD とは分離し、重要な一時メッセージ
// (エラー / ディスプレイ切替 / ファイル結果 / 切断) が即上書きされないようにする。

import "./toast.css";

export type ToastKind = "info" | "success" | "error";

let container: HTMLElement | null = null;

export function initToast(root: HTMLElement): void {
  container = root;
  // 一時通知をスクリーンリーダーへ伝える (エラー等を見落とさせない)。
  // polite: 進行中の読み上げを中断せず、合間に読み上げる。
  root.setAttribute("role", "status");
  root.setAttribute("aria-live", "polite");
}

// 同時に積むトーストの上限。連続発生でスタックが画面外まで伸びるのを防ぐ。
const MAX_TOASTS = 4;

export function showToast(message: string, level: ToastKind = "info", durationMs = 4000): void {
  if (!container) return;
  // 上限を超えるぶんは古いものから消す。
  while (container.children.length >= MAX_TOASTS) container.firstElementChild?.remove();

  const el = document.createElement("div");
  el.className = `toast ${level}`;
  el.textContent = message;
  container.appendChild(el);

  // 次フレームで show を付けてフェードイン。
  requestAnimationFrame(() => el.classList.add("show"));

  let dismissed = false;
  const dismiss = (): void => {
    if (dismissed) return;
    dismissed = true;
    window.clearTimeout(timer);
    el.classList.remove("show");
    // トランジション終了後に DOM から除去。
    el.addEventListener("transitionend", () => el.remove(), { once: true });
    // 念のためのフォールバック除去。
    window.setTimeout(() => el.remove(), 400);
  };
  // クリックで即座に消せる (邪魔なときに待たなくてよい)。
  el.addEventListener("click", dismiss);
  const timer = window.setTimeout(dismiss, durationMs);
}
