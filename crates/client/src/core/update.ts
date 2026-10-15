// GitHub Releases 経由のアプリ更新チェック/適用。tauri-plugin-updater の薄いラッパー。

import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { invoke } from "@tauri-apps/api/core";
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
type VerifiedUpdate = { version: string; url: string; signature: string };

/**
 * プラグインが取得した更新内容が、署名検証済み manifest (latest.json.sig) と一致するか。
 * プラグインは setup.exe の署名しか見ないため、version を騙って旧正規版を指す manifest への
 * 差し替え (ダウングレード) を防ぐ。検証できなければ fail-closed で適用しない。
 */
async function matchesVerifiedManifest(update: Update): Promise<boolean> {
  try {
    const v = await invoke<VerifiedUpdate>("verify_update_manifest");
    const platforms = update.rawJson["platforms"] as Record<string, { url?: string; signature?: string }> | undefined;
    const p = platforms?.["windows-x86_64"];
    return update.version === v.version && p?.url === v.url && p?.signature === v.signature;
  } catch (e) {
    logger.warn(`update manifest verification failed: ${String(e)}`);
    return false;
  }
}

export async function checkForUpdate(): Promise<UpdateStatus> {
  try {
    const found = await check();
    const update = found && (await matchesVerifiedManifest(found)) ? found : null;
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
    if (!u || !(await matchesVerifiedManifest(u))) return;
    pending = u;
  }
  await pending.downloadAndInstall();
  await relaunch();
}
