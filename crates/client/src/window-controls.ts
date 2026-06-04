// フレームレスウィンドウ用の自前ウィンドウ操作 (最小化 / 最大化トグル / 閉じる)。
// ネイティブのタイトルバーを消した (tauri.conf.json decorations:false) ため、
// min/max/close を UI 側に持つ。ドラッグ移動は data-tauri-drag-region 属性で行う。
// 権限: capabilities/default.json の allow-minimize / allow-toggle-maximize /
//       allow-close / allow-start-dragging。

import { getCurrentWindow } from "@tauri-apps/api/window";
import { icon } from "./icons";

export async function minimizeWindow(): Promise<void> {
  try {
    await getCurrentWindow().minimize();
  } catch (e) {
    console.warn("minimize failed", e);
  }
}

export async function closeWindow(): Promise<void> {
  try {
    await getCurrentWindow().close();
  } catch (e) {
    console.warn("close failed", e);
  }
}

/**
 * 最小化 / 閉じる ボタンを container 内に生成して配線する。
 * ウィンドウは固定サイズのため最大化は持たない (リモート接続中のフルスクリーンは
 * ツールバーの専用ボタンで行う)。
 */
export function mountWindowControls(container: HTMLElement): void {
  container.innerHTML = `
    <button class="winctl" data-act="min" title="最小化" tabindex="-1">${icon("minimize")}</button>
    <button class="winctl close" data-act="close" title="閉じる" tabindex="-1">${icon("close")}</button>
  `;
  container.querySelector('[data-act="min"]')?.addEventListener("click", () => void minimizeWindow());
  container.querySelector('[data-act="close"]')?.addEventListener("click", () => void closeWindow());
}
