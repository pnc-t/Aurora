// 接続フォーム + ステータス表示 + ディスプレイ選択。
// 接続成功時に onSubmit コールバックを呼ぶ。HostInfo 受信で表示一覧を
// ドロップダウンに反映し、選択変更を onSelectDisplay で通知する。

import type { DisplayDto } from "./ipc";

type DisconnectCallback = () => void;
type FormCallback = (host: string, password: string) => Promise<void>;
type SelectDisplayCallback = (display: DisplayDto) => void;

let statusEl: HTMLElement | null = null;
let formEl: HTMLFormElement | null = null;
let displaySelectEl: HTMLSelectElement | null = null;
let onSelectDisplayCb: SelectDisplayCallback | null = null;
// HostInfo で受け取った表示一覧 (選択時の寸法参照に使う)。
let currentDisplays: DisplayDto[] = [];

export function mountConnectionUi(opts: {
  root: HTMLElement;
  onSubmit: FormCallback;
  onDisconnect: DisconnectCallback;
  onSelectDisplay: SelectDisplayCallback;
}): void {
  opts.root.innerHTML = `
    <form id="conn-form" style="display:flex;gap:8px;align-items:center;padding:8px;background:#222">
      <label>host <input id="host" type="text" placeholder="100.x.x.x or hostname" value="127.0.0.1" required style="width:200px"></label>
      <label>password <input id="password" type="password" required style="width:200px"></label>
      <button type="submit">connect</button>
      <button type="button" id="disconnect-btn">disconnect</button>
      <label id="display-label" style="display:none">display
        <select id="display-select"></select>
      </label>
      <span id="status" style="margin-left:auto;font-family:monospace">idle</span>
    </form>
  `;
  formEl = opts.root.querySelector<HTMLFormElement>("#conn-form");
  statusEl = opts.root.querySelector<HTMLElement>("#status");
  displaySelectEl = opts.root.querySelector<HTMLSelectElement>("#display-select");
  onSelectDisplayCb = opts.onSelectDisplay;
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

  displaySelectEl?.addEventListener("change", () => {
    const id = Number(displaySelectEl?.value);
    const d = currentDisplays.find((x) => x.id === id);
    if (d && onSelectDisplayCb) onSelectDisplayCb(d);
  });
}

export function setStatus(s: string): void {
  if (statusEl) statusEl.textContent = s;
}

export function setHostInfo(displays: DisplayDto[]): void {
  currentDisplays = displays;
  if (displays.length === 0) {
    setStatus("connected (no displays?)");
    return;
  }
  const d = displays[0];
  setStatus(`connected · ${d.name} ${d.width}x${d.height}`);

  // ドロップダウンを更新。表示が 1 枚だけなら隠す。
  const label = document.getElementById("display-label");
  if (displaySelectEl && label) {
    displaySelectEl.innerHTML = displays
      .map((x) => `<option value="${x.id}">${x.name} (${x.width}x${x.height})</option>`)
      .join("");
    label.style.display = displays.length > 1 ? "inline" : "none";
  }
}
