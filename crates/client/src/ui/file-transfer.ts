// ファイル転送 UI。
// - アップロード (クライアント → ホスト): キャプチャ画面へのドラッグ&ドロップで行う (main.ts)。
//   ここには操作ヒントと進捗表示を置く。
// - ダウンロード (ホスト → クライアント): ホスト上のパスを指定して取得。
//   受信ファイルはクライアント側の %USERPROFILE%\Aurora\downloads に自動保存される。
// 共有フォルダ (ProjFS ドライブ投影) の設定は launcher の設定画面に移設した (接続のたびに
// 自動適用されるため、転送パネルには持たない)。
// ダウンロードの完了 / エラーは host からの FileResult (転送 id で照合)、進行中の割合は
// FileProgress (done/total バイト) で更新する。

import { requestFile } from "../core/ipc";
import { logger } from "../core/log";
import "./file-transfer.css";

// 過去に取得したホスト上のパス (datalist 補完用)。ローカルの UI 選好なので localStorage に持つ。
const PATH_HISTORY_KEY = "aurora.download.paths";
const PATH_HISTORY_MAX = 5;

function loadPathHistory(): string[] {
  try {
    const raw = localStorage.getItem(PATH_HISTORY_KEY);
    const arr: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(arr) ? arr.filter((x): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}

function savePathHistory(path: string): void {
  const next = [path, ...loadPathHistory().filter((p) => p !== path)].slice(0, PATH_HISTORY_MAX);
  try {
    localStorage.setItem(PATH_HISTORY_KEY, JSON.stringify(next));
  } catch (e) {
    logger.warn("save download path history failed", e);
  }
}

let downloadProgressEl: HTMLElement | null = null;
let downloadBarEl: HTMLElement | null = null;
let uploadProgressEl: HTMLElement | null = null;
let uploadBarEl: HTMLElement | null = null;
let pathListEl: HTMLDataListElement | null = null;
// ダウンロードボタン (結果受信で再有効化するためモジュールに保持)。
let downloadBtn: HTMLButtonElement | null = null;
// 進行中ダウンロードの id と要求パス (FileResult 照合・成功時の履歴保存用)。
let activeDownloadId: number | null = null;
let activeDownloadPath = "";

function renderPathHistory(): void {
  if (!pathListEl) return;
  pathListEl.replaceChildren(
    ...loadPathHistory().map((p) => {
      const opt = document.createElement("option");
      opt.value = p;
      return opt;
    }),
  );
}

export function mountFileTransfer(panel: HTMLElement): void {
  panel.innerHTML = `
    <div class="section-title">upload to host</div>
    <div class="row">
      <span class="hint">ファイルをキャプチャ画面にドラッグ&ドロップ</span>
    </div>
    <div class="row transfer-bar-row" id="upload-bar-row" hidden>
      <div class="transfer-bar"><div class="transfer-bar-fill" id="upload-bar"></div></div>
      <span id="upload-progress" class="progress"></span>
    </div>
    <div class="section-title">download from host</div>
    <div class="row">
      <input id="download-path" type="text" list="download-path-list"
             placeholder="例: C:\\Users\\name\\file.zip (ホスト上のパス)" />
      <datalist id="download-path-list"></datalist>
      <button type="button" id="download-file-btn">get</button>
      <span id="download-progress" class="progress"></span>
    </div>
    <div class="row transfer-bar-row" id="download-bar-row" hidden>
      <div class="transfer-bar"><div class="transfer-bar-fill" id="download-bar"></div></div>
    </div>
    <div class="row"><span class="hint">受信ファイルは %USERPROFILE%\\Aurora\\downloads に保存されます</span></div>
  `;

  downloadProgressEl = panel.querySelector<HTMLElement>("#download-progress");
  downloadBarEl = panel.querySelector<HTMLElement>("#download-bar");
  uploadProgressEl = panel.querySelector<HTMLElement>("#upload-progress");
  uploadBarEl = panel.querySelector<HTMLElement>("#upload-bar");
  pathListEl = panel.querySelector<HTMLDataListElement>("#download-path-list");
  downloadBtn = panel.querySelector<HTMLButtonElement>("#download-file-btn");
  const pathInput = panel.querySelector<HTMLInputElement>("#download-path");
  renderPathHistory();

  downloadBtn?.addEventListener("click", () => {
    const path = pathInput?.value?.trim();
    if (!path) {
      setDownloadProgress("パスを入力");
      return;
    }
    const id = Date.now() >>> 0;
    activeDownloadId = id;
    activeDownloadPath = path;
    if (downloadBtn) downloadBtn.disabled = true; // 結果待ちの二重要求を防ぐ
    setDownloadProgress("要求中…");
    setBar("download", 0);
    void requestFile(id, path).catch((err) => {
      activeDownloadId = null;
      if (downloadBtn) downloadBtn.disabled = false;
      hideBar("download");
      setDownloadProgress(`エラー: ${String(err)}`);
    });
  });
}

function setBar(kind: "download" | "upload", ratio: number): void {
  const row = document.getElementById(`${kind}-bar-row`);
  const fill = kind === "download" ? downloadBarEl : uploadBarEl;
  if (row) row.hidden = false;
  if (fill) fill.style.width = `${Math.round(Math.max(0, Math.min(1, ratio)) * 100)}%`;
}

function hideBar(kind: "download" | "upload"): void {
  const row = document.getElementById(`${kind}-bar-row`);
  if (row) row.hidden = true;
}

/**
 * host からの FileResult を反映する (ダウンロード)。アップロード (D&D) の結果は main.ts が
 * トーストで通知するため、ここではダウンロードの照合とアップロード進捗バーの後始末のみ行う。
 */
export function handleFileResult(id: number, ok: boolean, message: string): void {
  if (id === activeDownloadId) {
    setDownloadProgress(ok ? `保存: ${message}` : `失敗: ${message}`);
    hideBar("download");
    if (ok && activeDownloadPath) {
      savePathHistory(activeDownloadPath);
      renderPathHistory();
    }
    activeDownloadId = null;
    activeDownloadPath = "";
    if (downloadBtn) downloadBtn.disabled = false; // 次の要求を受け付ける
  } else {
    // ダウンロード以外の id はアップロード。完了/失敗でバーを畳む。
    hideBar("upload");
    if (uploadProgressEl) uploadProgressEl.textContent = "";
  }
}

/** host からの FileProgress (done/total バイト) を進捗バーへ反映する。 */
export function handleFileProgress(id: number, done: number, total: number): void {
  const ratio = total > 0 ? done / total : 0;
  const pct = `${Math.round(ratio * 100)}%`;
  if (id === activeDownloadId) {
    setBar("download", ratio);
    setDownloadProgress(`受信中… ${pct}`);
  } else {
    setBar("upload", ratio);
    if (uploadProgressEl) uploadProgressEl.textContent = pct;
  }
}

function setDownloadProgress(s: string): void {
  if (downloadProgressEl) downloadProgressEl.textContent = s;
}
