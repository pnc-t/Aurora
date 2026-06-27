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
// ダウンロードボタン (結果受信で再有効化するためモジュールに保持)。
let downloadBtn: HTMLButtonElement | null = null;
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
      <input id="download-path" type="text" placeholder="例: C:\\Users\\name\\file.zip (ホスト上のパス)" />
      <button type="button" id="download-file-btn">get</button>
      <span id="download-progress" class="progress"></span>
    </div>
    <div class="row"><span class="hint">受信ファイルは %USERPROFILE%\\Aurora\\downloads に保存されます</span></div>
    <div class="section-title">share folder (ProjFS)</div>
    <div class="row">
      <input id="share-root" type="text" placeholder="例: C:\\Users\\name\\Shared (ホスト上のフォルダ)" />
      <button type="button" id="share-root-btn">share</button>
      <span id="share-root-status" class="progress"></span>
    </div>
  `;

  downloadProgressEl = panel.querySelector<HTMLElement>("#download-progress");
  downloadBtn = panel.querySelector<HTMLButtonElement>("#download-file-btn");
  const pathInput = panel.querySelector<HTMLInputElement>("#download-path");
  const shareInput = panel.querySelector<HTMLInputElement>("#share-root");
  const shareBtn = panel.querySelector<HTMLButtonElement>("#share-root-btn");
  const shareStatus = panel.querySelector<HTMLElement>("#share-root-status");

  shareBtn?.addEventListener("click", () => {
    const path = shareInput?.value?.trim();
    if (!path) {
      if (shareStatus) shareStatus.textContent = "パスを入力";
      return;
    }
    shareBtn.disabled = true;
    if (shareStatus) shareStatus.textContent = "共有中…";
    void setShareRoot(path)
      .then(() => {
        if (shareStatus) shareStatus.textContent = "共有しました";
      })
      .catch((err) => {
        if (shareStatus) shareStatus.textContent = `エラー: ${String(err)}`;
      })
      .finally(() => {
        shareBtn.disabled = false;
      });
  });

  downloadBtn?.addEventListener("click", () => {
    const path = pathInput?.value?.trim();
    if (!path) {
      setDownloadProgress("パスを入力");
      return;
    }
    const id = Date.now() >>> 0;
    activeDownloadId = id;
    if (downloadBtn) downloadBtn.disabled = true; // 結果待ちの二重要求を防ぐ
    setDownloadProgress("要求中…");
    void requestFile(id, path).catch((err) => {
      activeDownloadId = null;
      if (downloadBtn) downloadBtn.disabled = false;
      setDownloadProgress(`エラー: ${String(err)}`);
    });
  });
}

/// host からの FileResult を反映する (ダウンロード)。アップロード (D&D) は main.ts が
/// トーストで通知するため、ここではダウンロードのみ照合する。
export function handleFileResult(id: number, ok: boolean, message: string): void {
  if (id === activeDownloadId) {
    setDownloadProgress(ok ? `保存: ${message}` : `失敗: ${message}`);
    activeDownloadId = null;
    if (downloadBtn) downloadBtn.disabled = false; // 次の要求を受け付ける
  }
}

function setDownloadProgress(s: string): void {
  if (downloadProgressEl) downloadProgressEl.textContent = s;
}
