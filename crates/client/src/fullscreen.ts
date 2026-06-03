// Tauri ウィンドウのフルスクリーン切替ラッパー。
// ネイティブタイトルバーを消してリモート画面に没入する。
// 権限: capabilities/default.json に core:window:allow-set-fullscreen が必要。

import { getCurrentWindow } from "@tauri-apps/api/window";

let fullscreen = false;

export async function toggleFullscreen(): Promise<boolean> {
  fullscreen = !fullscreen;
  try {
    await getCurrentWindow().setFullscreen(fullscreen);
  } catch (e) {
    // 失敗したら状態を戻す。
    fullscreen = !fullscreen;
    console.warn("setFullscreen failed", e);
  }
  return fullscreen;
}

export async function exitFullscreen(): Promise<void> {
  if (!fullscreen) return;
  fullscreen = false;
  try {
    await getCurrentWindow().setFullscreen(false);
  } catch (e) {
    console.warn("setFullscreen(false) failed", e);
  }
}

export function isFullscreen(): boolean {
  return fullscreen;
}
