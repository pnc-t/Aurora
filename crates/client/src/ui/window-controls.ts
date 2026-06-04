// フレームレスウィンドウ用の自前ウィンドウ操作 (最小化 / 最大化トグル / 閉じる)。
// ネイティブのタイトルバーを消した (tauri.conf.json decorations:false) ため、
// min/max/close を UI 側に持つ。ドラッグ移動は data-tauri-drag-region 属性で行う。
// 権限: capabilities/default.json の allow-minimize / allow-toggle-maximize /
//       allow-close / allow-start-dragging。

import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { icon } from "./icons";
import "./window-controls.css";

// 未接続ランチャーの固定サイズ (tauri.conf.json の初期値と一致させる)。
const LAUNCHER_WIDTH = 920;
const LAUNCHER_HEIGHT = 600;

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
 * ウィンドウのリサイズ可否を切り替える。
 * ランチャー (未接続) は固定サイズ、接続中は四辺リサイズを許可する用途。
 * 権限: capabilities/default.json の core:window:allow-set-resizable。
 */
export async function setResizable(on: boolean): Promise<void> {
  try {
    await getCurrentWindow().setResizable(on);
  } catch (e) {
    console.warn("setResizable failed", e);
  }
}

/**
 * ランチャー (未接続) のウィンドウ状態へ戻す。
 * 接続中にユーザーがリサイズしたサイズが切断後も残らないよう、固定サイズに戻し
 * リサイズも無効化する。権限: core:window:allow-set-size / allow-set-resizable。
 */
export async function restoreLauncherWindow(): Promise<void> {
  try {
    const w = getCurrentWindow();
    await w.setResizable(false);
    await w.setSize(new LogicalSize(LAUNCHER_WIDTH, LAUNCHER_HEIGHT));
  } catch (e) {
    console.warn("restoreLauncherWindow failed", e);
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
