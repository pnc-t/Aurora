// ファイル送信 UI (クライアント → ホスト)。
// ファイルを選んで「send file」を押すと、チャンク分割して制御チャネル経由で送る。
// 進捗は送信側で算出して表示する。完了 / エラーは host からの FileResult で更新する。

import { sendFileChunk, sendFileOffer } from "./ipc";

// TCP フレーム上限 (64KB) と bincode オーバーヘッドに収まるサイズ。
const CHUNK = 32 * 1024;

let progressEl: HTMLElement | null = null;
// 進行中転送の id (FileResult 照合用)。
let activeId: number | null = null;

export function mountFileTransfer(form: HTMLElement): void {
  const wrap = document.createElement("span");
  wrap.style.cssText = "display:flex;gap:6px;align-items:center;margin-left:12px";
  wrap.innerHTML = `
    <input id="file-input" type="file" style="max-width:180px;color:#eee" />
    <button type="button" id="send-file-btn">send file</button>
    <span id="file-progress" style="font-family:monospace;min-width:90px"></span>
  `;
  form.appendChild(wrap);

  progressEl = wrap.querySelector<HTMLElement>("#file-progress");
  const input = wrap.querySelector<HTMLInputElement>("#file-input");
  const btn = wrap.querySelector<HTMLButtonElement>("#send-file-btn");

  btn?.addEventListener("click", () => {
    const file = input?.files?.[0];
    if (!file) {
      setProgress("no file");
      return;
    }
    btn.disabled = true;
    void sendFile(file)
      .catch((err) => setProgress(`error: ${String(err)}`))
      .finally(() => {
        btn.disabled = false;
      });
  });
}

async function sendFile(file: File): Promise<void> {
  // 32bit に収まる転送 id。FileResult の照合に使う。
  const id = Date.now() >>> 0;
  activeId = id;
  setProgress("0%");

  await sendFileOffer(id, file.name, file.size);

  if (file.size === 0) {
    // 空ファイルは last=true の空チャンクで完了させる。
    await sendFileChunk(id, 0, [], true);
    setProgress("sent, waiting…");
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
    setProgress(`${Math.floor((offset / file.size) * 100)}%`);
  }
  setProgress("sent, waiting…");
}

/// host からの FileResult を反映する。
export function handleFileResult(id: number, ok: boolean, message: string): void {
  if (activeId !== null && id !== activeId) return;
  setProgress(ok ? `done: ${message}` : `failed: ${message}`);
  activeId = null;
}

function setProgress(s: string): void {
  if (progressEl) progressEl.textContent = s;
}
