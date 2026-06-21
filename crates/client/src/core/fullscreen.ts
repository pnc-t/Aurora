// Tauri ウィンドウのフルスクリーン切替ラッパー。
// ネイティブタイトルバーを消してリモート画面に没入する。
// 権限: capabilities/default.json に core:window:allow-set-fullscreen が必要。

import { getCurrentWindow } from "@tauri-apps/api/window";
import { logger } from "./log";

let fullscreen = false;

export async function toggleFullscreen(): Promise<boolean> {
  fullscreen = !fullscreen;
  try {
    await getCurrentWindow().setFullscreen(fullscreen);
  } catch (e) {
    // 失敗したら状態を戻す。
    fullscreen = !fullscreen;
    logger.warn("setFullscreen failed", e);
  }
  return fullscreen;
}

export async function exitFullscreen(): Promise<void> {
  if (!fullscreen) return;
  fullscreen = false;
  try {
    await getCurrentWindow().setFullscreen(false);
  } catch (e) {
    logger.warn("setFullscreen(false) failed", e);
  }
}

export function isFullscreen(): boolean {
  return fullscreen;
}
