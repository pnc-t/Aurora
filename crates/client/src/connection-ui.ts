// 接続フォーム + ステータス表示。
// 接続成功時に onConnect コールバックを呼び、HostInfo の display サイズを通知する。

import type { DisplayDto } from "./ipc";

type DisconnectCallback = () => void;
type FormCallback = (host: string, password: string) => Promise<void>;

let statusEl: HTMLElement | null = null;
let formEl: HTMLFormElement | null = null;

export function mountConnectionUi(opts: {
  root: HTMLElement;
  onSubmit: FormCallback;
  onDisconnect: DisconnectCallback;
}): void {
  opts.root.innerHTML = `
    <form id="conn-form" style="display:flex;gap:8px;align-items:center;padding:8px;background:#222">
      <label>host <input id="host" type="text" placeholder="100.x.x.x or hostname" value="127.0.0.1" required style="width:200px"></label>
      <label>password <input id="password" type="password" required style="width:200px"></label>
      <button type="submit">connect</button>
      <button type="button" id="disconnect-btn">disconnect</button>
      <span id="status" style="margin-left:auto;font-family:monospace">idle</span>
    </form>
  `;
  formEl = opts.root.querySelector<HTMLFormElement>("#conn-form");
  statusEl = opts.root.querySelector<HTMLElement>("#status");
  const disconnectBtn = opts.root.querySelector<HTMLButtonElement>("#disconnect-btn");

  formEl?.addEventListener("submit", (e) => {
    e.preventDefault();
    const host = (opts.root.querySelector<HTMLInputElement>("#host"))?.value ?? "";
    const password = (opts.root.querySelector<HTMLInputElement>("#password"))?.value ?? "";
    setStatus(`connecting to ${host}...`);
    void opts.onSubmit(host, password).catch((err) => {
      setStatus(`error: ${String(err)}`);
    });
  });
  disconnectBtn?.addEventListener("click", () => {
    opts.onDisconnect();
  });
}

export function setStatus(s: string): void {
  if (statusEl) statusEl.textContent = s;
}

export function setHostInfo(displays: DisplayDto[]): void {
  if (displays.length === 0) {
    setStatus("connected (no displays?)");
    return;
  }
  const d = displays[0];
  setStatus(`connected · ${d.name} ${d.width}x${d.height}`);
}

