// capture ウィンドウの全画面オーバーレイ群。
// - 中央ステータス (#conn-status): 接続中スピナー / エラー表示 + 操作ボタン (閉じる/再接続)
// - ファイル D&D のドロップヒント
// - 「n 件の接続を閉じます」等の軽量確認ダイアログ
// テキストの reason はホスト由来を含みうるため textContent で組む (XSS 注意)。

import type { StatusKind } from "../core/session";
import "./capture-overlay.css";

let statusEl: HTMLElement | null = null;

export function initCaptureOverlay(el: HTMLElement): void {
  statusEl = el;
  statusEl.className = "conn-overlay";
}

/** ステータスオーバーレイのボタン操作。未指定のボタンは出さない。 */
export type StatusActions = {
  /** connecting 中 = キャンセル、error 後 = 閉じる。 */
  onClose?: () => void;
  /** エラー画面の「再接続」(再試行カウンタをリセットして即時再接続)。 */
  onReconnect?: () => void;
};

function makeOverlayButton(label: string, primary: boolean, onClick: () => void): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = primary ? "conn-btn primary" : "conn-btn";
  btn.textContent = label;
  btn.addEventListener("click", onClick);
  return btn;
}

/** 中央ステータスオーバーレイを描画する ("hidden" で消す)。 */
export function renderStatusOverlay(kind: StatusKind, text: string, actions: StatusActions): void {
  if (!statusEl) return;
  if (kind === "hidden") {
    statusEl.classList.remove("visible");
    statusEl.replaceChildren();
    return;
  }
  const card = document.createElement("div");
  card.className = "conn-card";
  if (kind === "connecting") {
    const spinner = document.createElement("div");
    spinner.className = "conn-spinner";
    card.appendChild(spinner);
  }
  const msg = document.createElement("div");
  msg.className = kind === "error" ? "conn-msg error" : "conn-msg";
  msg.textContent = text;
  card.appendChild(msg);
  const row = document.createElement("div");
  row.className = "conn-actions";
  if (kind === "error" && actions.onReconnect) {
    row.appendChild(makeOverlayButton("再接続", true, actions.onReconnect));
  }
  if (actions.onClose) {
    // connecting 中 (準備中/再接続中含む) はキャンセル操作、error 後は文字通り閉じる操作として文言を出し分ける。
    row.appendChild(makeOverlayButton(kind === "error" ? "閉じる" : "キャンセル", false, actions.onClose));
  }
  if (row.childElementCount > 0) card.appendChild(row);
  statusEl.replaceChildren(card);
  statusEl.classList.add("visible");
}

// --- ファイル D&D のドロップヒント ---
let dropHint: HTMLElement | null = null;

export function showDropHint(on: boolean): void {
  if (!dropHint) {
    dropHint = document.createElement("div");
    dropHint.id = "drop-hint";
    dropHint.textContent = "ドロップしてホストへ転送";
    document.body.appendChild(dropHint);
  }
  dropHint.classList.toggle("visible", on);
}

// --- 軽量確認ダイアログ (ウィンドウを閉じる確認など) ---
let confirmEl: HTMLElement | null = null;

export function openConfirmOverlay(message: string, okLabel: string, onOk: () => void): void {
  confirmEl?.remove();
  const overlay = document.createElement("div");
  overlay.className = "conn-overlay visible";
  const card = document.createElement("div");
  card.className = "conn-card";
  const msg = document.createElement("div");
  msg.className = "conn-msg";
  msg.textContent = message;
  const row = document.createElement("div");
  row.className = "conn-actions";
  const close = (): void => {
    overlay.remove();
    if (confirmEl === overlay) confirmEl = null;
  };
  const okBtn = makeOverlayButton(okLabel, true, () => {
    close();
    onOk();
  });
  const cancelBtn = makeOverlayButton("キャンセル", false, close);
  row.append(cancelBtn, okBtn);
  card.append(msg, row);
  overlay.appendChild(card);
  document.body.appendChild(overlay);
  confirmEl = overlay;
  // 誤 Enter で確定しないよう既定フォーカスはキャンセル側 (launcher の確認ダイアログと同方針)。
  cancelBtn.focus();
}
