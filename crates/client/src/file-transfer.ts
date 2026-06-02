// ファイル転送 UI。
// - アップロード (クライアント → ホスト): ファイルを選んでチャンク送信。
// - ダウンロード (ホスト → クライアント): ホスト上のパスを指定して取得。
//   受信ファイルはクライアント側の ddd-downloads/ に自動保存される。
// 完了 / エラーは host からの FileResult (転送 id で照合) で更新する。

import { requestFile, sendFileChunk, sendFileOffer, setShareRoot } from "./ipc";

// TCP フレーム上限 (64KB) と bincode オーバーヘッドに収まるサイズ。
const CHUNK = 32 * 1024;

let uploadProgressEl: HTMLElement | null = null;
let downloadProgressEl: HTMLElement | null = null;
// 進行中転送の id (FileResult 照合用)。
let activeUploadId: number | null = null;
let activeDownloadId: number | null = null;

export function mountFileTransfer(form: HTMLElement): void {
  const wrap = document.createElement("span");
  wrap.style.cssText = "display:flex;gap:6px;align-items:center;margin-left:12px";
  wrap.innerHTML = `
    <input id="file-input" type="file" style="max-width:160px;color:#eee" />
    <button type="button" id="send-file-btn">send</button>
    <span id="file-progress" style="font-family:monospace;min-width:80px"></span>
    <input id="download-path" type="text" placeholder="host file path" style="width:160px" />
    <button type="button" id="download-file-btn">download</button>
    <span id="download-progress" style="font-family:monospace;min-width:80px"></span>
    <input id="share-root" type="text" placeholder="share folder (ProjFS)" style="width:180px" />
    <button type="button" id="share-root-btn">share</button>
    <span id="share-root-status" style="font-family:monospace;min-width:80px"></span>
  `;
  form.appendChild(wrap);

  uploadProgressEl = wrap.querySelector<HTMLElement>("#file-progress");
  downloadProgressEl = wrap.querySelector<HTMLElement>("#download-progress");
  const input = wrap.querySelector<HTMLInputElement>("#file-input");
  const sendBtn = wrap.querySelector<HTMLButtonElement>("#send-file-btn");
  const pathInput = wrap.querySelector<HTMLInputElement>("#download-path");
  const dlBtn = wrap.querySelector<HTMLButtonElement>("#download-file-btn");
  const shareInput = wrap.querySelector<HTMLInputElement>("#share-root");
  const shareBtn = wrap.querySelector<HTMLButtonElement>("#share-root-btn");
  const shareStatus = wrap.querySelector<HTMLElement>("#share-root-status");

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

  sendBtn?.addEventListener("click", () => {
    const file = input?.files?.[0];
    if (!file) {
      setUploadProgress("no file");
      return;
    }
    sendBtn.disabled = true;
    void sendFile(file)
      .catch((err) => setUploadProgress(`error: ${String(err)}`))
      .finally(() => {
        sendBtn.disabled = false;
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

async function sendFile(file: File): Promise<void> {
  // 32bit に収まる転送 id。FileResult の照合に使う。
  const id = Date.now() >>> 0;
  activeUploadId = id;
  setUploadProgress("0%");

  await sendFileOffer(id, file.name, file.size);

  if (file.size === 0) {
    // 空ファイルは last=true の空チャンクで完了させる。
    await sendFileChunk(id, 0, [], true);
    setUploadProgress("sent, waiting…");
    return;
  }

  let offset = 0;
  while (offset < file.size) {
    const slice = file.slice(offset, offset + CHUNK);
    const bytes = new Uint8Array(await slice.arrayBuffer());
    const last = offset + bytes.length >= file.size;
    // invoke 引数は JSON 化されるため number[] で渡す。
    await sendFileChunk(id, offset, Array.from(bytes), last);
    offset += bytes.length;
    setUploadProgress(`${Math.floor((offset / file.size) * 100)}%`);
  }
  setUploadProgress("sent, waiting…");
}

/// host からの FileResult を反映する (アップロード / ダウンロード両方)。
export function handleFileResult(id: number, ok: boolean, message: string): void {
  if (id === activeDownloadId) {
    setDownloadProgress(ok ? `saved: ${message}` : `failed: ${message}`);
    activeDownloadId = null;
    return;
  }
  if (activeUploadId === null || id === activeUploadId) {
    setUploadProgress(ok ? `done: ${message}` : `failed: ${message}`);
    activeUploadId = null;
  }
}

function setUploadProgress(s: string): void {
  if (uploadProgressEl) uploadProgressEl.textContent = s;
}

function setDownloadProgress(s: string): void {
  if (downloadProgressEl) downloadProgressEl.textContent = s;
}
