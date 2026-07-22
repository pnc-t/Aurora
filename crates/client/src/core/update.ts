// GitHub Releases 経由のアプリ更新チェック/適用。tauri-plugin-updater の薄いラッパー。

import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { logger } from "./log";

/** 更新チェック結果。available なら新版あり。 */
export type UpdateStatus =
  | { kind: "available"; version: string; notes: string }
  | { kind: "none" };

// check() が返す Update ハンドルをモジュール内に保持し、インストール時に再利用する
// (再 check の往復を省く)。
let pending: Update | null = null;

/**
 * GitHub Releases の latest.json をチェックする。新版があれば version/notes を返す。
 * ネットワーク不通・dev 実行 (エンドポイント未到達) 等の失敗は none 扱いにする
 * (更新は必須機能ではないため、失敗で UI をブロックしない)。
 */
export async function checkForUpdate(): Promise<UpdateStatus> {
  try {
    const update = await check();
    if (update) {
      pending = update;
      return { kind: "available", version: update.version, notes: update.body ?? "" };
    }
    pending = null;
    return { kind: "none" };
  } catch (e) {
    logger.warn(`update check failed: ${String(e)}`);
    return { kind: "none" };
  }
}

/**
 * 保留中の更新をダウンロード→インストールし、アプリを再起動する。
 * pending が無ければ再チェックしてから実行する。
 */
export async function installPendingUpdate(): Promise<void> {
  if (!pending) {
    const u = await check();
    if (!u) return;
    pending = u;
  }
  await pending.downloadAndInstall();
  await relaunch();
}
