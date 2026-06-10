// ファイル転送 UI。
// - アップロード (クライアント → ホスト): キャプチャ画面へのドラッグ&ドロップで行う (main.ts)。
//   ここには操作ヒントだけを置く。
// - ダウンロード (ホスト → クライアント): ホスト上のパスを指定して取得。
//   受信ファイルはクライアント側の %USERPROFILE%\Aurora\downloads に自動保存される。
// - 共有フォルダ (ProjFS): ホストの Explorer に投影するフォルダを設定する。
// ダウンロードの完了 / エラーは host からの FileResult (転送 id で照合) で更新する。

import { requestFile, setShareRoot } from "../core/ipc";
import "./file-transfer.css";

let downloadProgressEl: HTMLElement | null = null;
// 進行中ダウンロードの id (FileResult 照合用)。
let activeDownloadId: number | null = null;

export function mountFileTransfer(panel: HTMLElement): void {
  panel.innerHTML = `
    <div class="section-title">upload to host</div>
    <div class="row">
      <span class="hint">ファイルをキャプチャ画面にドラッグ&ドロップ</span>
    </div>
    <div class="section-title">download from host</div>
    <div class="row">
      <input id="download-path" type="text" placeholder="host file path" />
      <button type="button" id="download-file-btn">get</button>
      <span id="download-progress" class="progress"></span>
    </div>
    <div class="section-title">share folder (ProjFS)</div>
    <div class="row">
      <input id="share-root" type="text" placeholder="share folder path" />
      <button type="button" id="share-root-btn">share</button>
      <span id="share-root-status" class="progress"></span>
    </div>
  `;

  downloadProgressEl = panel.querySelector<HTMLElement>("#download-progress");
  const pathInput = panel.querySelector<HTMLInputElement>("#download-path");
  const dlBtn = panel.querySelector<HTMLButtonElement>("#download-file-btn");
  const shareInput = panel.querySelector<HTMLInputElement>("#share-root");
  const shareBtn = panel.querySelector<HTMLButtonElement>("#share-root-btn");
  const shareStatus = panel.querySelector<HTMLElement>("#share-root-status");

  shareBtn?.addEventListener("click", () => {
    const path = shareInput?.value?.trim();
    if (!path) {
      if (shareStatus) shareStatus.textContent = "no path";
      return;
    }
    void setShareRoot(path)
      .then(() => {
        if (shareStatus) shareStatus.textContent = "shared";
      })
      .catch((err) => {
        if (shareStatus) shareStatus.textContent = `error: ${String(err)}`;
      });
  });

  dlBtn?.addEventListener("click", () => {
    const path = pathInput?.value?.trim();
    if (!path) {
      setDownloadProgress("no path");
      return;
    }
    const id = Date.now() >>> 0;
    activeDownloadId = id;
    setDownloadProgress("requesting…");
    void requestFile(id, path).catch((err) => setDownloadProgress(`error: ${String(err)}`));
  });
}

/// host からの FileResult を反映する (ダウンロード)。アップロード (D&D) は main.ts が
/// トーストで通知するため、ここではダウンロードのみ照合する。
export function handleFileResult(id: number, ok: boolean, message: string): void {
  if (id === activeDownloadId) {
    setDownloadProgress(ok ? `saved: ${message}` : `failed: ${message}`);
    activeDownloadId = null;
  }
}

function setDownloadProgress(s: string): void {
  if (downloadProgressEl) downloadProgressEl.textContent = s;
}
