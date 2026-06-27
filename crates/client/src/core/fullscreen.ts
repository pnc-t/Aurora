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
  // 全画面没入中だけ OS 予約キー (Alt+Tab / Win / Esc 等) をページへ捕まえ、ホストへ転送する。
  if (fullscreen) void lockSystemKeys();
  else unlockSystemKeys();
  return fullscreen;
}

export async function exitFullscreen(): Promise<void> {
  if (!fullscreen) return;
  fullscreen = false;
  unlockSystemKeys();
  try {
    await getCurrentWindow().setFullscreen(false);
  } catch (e) {
    logger.warn("setFullscreen(false) failed", e);
  }
}

export function isFullscreen(): boolean {
  return fullscreen;
}

// Keyboard Lock API (実験的 API のため型を最小限自前定義)。全画面中のみ有効で、
// Alt+Tab / Win / Ctrl+Esc / Escape 等の OS 予約キーを OS ではなくページへ届ける。
// これにより input.ts がそれらをホストへ転送できる (preventDefault だけでは奪われる)。
interface KeyboardLockApi {
  lock(keyCodes?: string[]): Promise<void>;
  unlock(): void;
}

function keyboardLock(): KeyboardLockApi | null {
  const kb = (navigator as unknown as { keyboard?: KeyboardLockApi }).keyboard;
  return kb && typeof kb.lock === "function" ? kb : null;
}

async function lockSystemKeys(): Promise<void> {
  const kb = keyboardLock();
  if (!kb) return;
  // Keyboard Lock は「DOM Fullscreen API でページが全画面のとき」だけ有効。
  // Aurora の全画面は Tauri ネイティブウィンドウ全画面で、それだけでは
  // document.fullscreenElement が立たずロックが発火しない。そこでロックのために
  // documentElement にも DOM 全画面を要求する (ネイティブ全画面の窓内を満たすだけで見た目は不変)。
  try {
    if (!document.fullscreenElement) {
      await document.documentElement.requestFullscreen();
    }
  } catch (e) {
    logger.warn("requestFullscreen for keyboard lock failed", e);
    return;
  }
  try {
    // 引数なし = ロック可能な全キーを捕捉 (リモート没入時は全キー透過が目的)。
    await kb.lock();
  } catch (e) {
    // 未対応等で失敗しても致命的でない。ログのみで継続。
    logger.warn("keyboard.lock failed", e);
  }
}

function unlockSystemKeys(): void {
  const kb = keyboardLock();
  if (kb) {
    try {
      kb.unlock();
    } catch (e) {
      logger.warn("keyboard.unlock failed", e);
    }
  }
  // ロックのために立てた DOM 全画面を畳む (ネイティブ全画面とは独立)。
  if (document.fullscreenElement) {
    void document.exitFullscreen().catch((e) => logger.warn("exitFullscreen failed", e));
  }
}
