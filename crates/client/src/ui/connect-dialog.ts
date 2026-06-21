// capture ウィンドウ内の「+」から新しいタブ (接続) を追加するためのピッカー。
// 保存済みホスト (鍵マーク) はクリックで即接続 (password=null → Rust が DPAPI 復号)、
// 未保存ホストや手動入力はパスワード欄で接続する。launcher のフルカード UI は持ち込まず、
// タブ追加に必要な最小限 (一覧 + 手動入力) に絞る。
// ホスト由来文字列は textContent で描画する (XSS 注意)。

import { loadRecentHosts, loadSettings, savedPasswordHosts, type HostEntry } from "../core/ipc";
import { icon } from "./icons";
import "./connect-dialog.css";

type ConnectPickerOpts = {
  /** 接続実行。解決 = 接続成功 (タブ追加済み)。reject でダイアログにエラーを出す。
   *  password=null は保存済み (DPAPI) を使う。 */
  onConnect: (entry: HostEntry, password: string | null, save: boolean) => Promise<void>;
};

const DEFAULT_PORT = 7000;

export function openConnectPicker(root: HTMLElement, opts: ConnectPickerOpts): void {
  root.innerHTML = `
    <div class="picker">
      <div class="picker-head">
        <h3>接続を追加</h3>
        <button type="button" class="picker-x" id="pk-cancel" title="閉じる">${icon("close")}</button>
      </div>
      <div class="picker-list" id="pk-list"></div>
      <form class="picker-form" autocomplete="off">
        <div class="picker-sub">手動で接続</div>
        <div class="pk-row">
          <input id="pk-host" type="text" placeholder="host (100.x.x.x / hostname)" required />
          <input id="pk-port" type="text" inputmode="numeric" placeholder="port" class="pk-port" />
        </div>
        <input id="pk-pass" type="password" placeholder="password" />
        <label class="pk-check"><input id="pk-save" type="checkbox" /> パスワードを保存する</label>
        <div class="pk-status" id="pk-status"></div>
        <div class="pk-actions">
          <button type="submit" class="pk-connect">connect</button>
        </div>
      </form>
    </div>
  `;
  root.classList.add("visible");

  const list = root.querySelector<HTMLElement>("#pk-list");
  const hostInput = root.querySelector<HTMLInputElement>("#pk-host");
  const portInput = root.querySelector<HTMLInputElement>("#pk-port");
  const passInput = root.querySelector<HTMLInputElement>("#pk-pass");
  const saveInput = root.querySelector<HTMLInputElement>("#pk-save");
  const statusEl = root.querySelector<HTMLElement>("#pk-status");
  const connectBtn = root.querySelector<HTMLButtonElement>(".pk-connect");

  function close(): void {
    root.classList.remove("visible");
    root.innerHTML = "";
  }
  root.querySelector("#pk-cancel")?.addEventListener("click", close);
  root.addEventListener("click", (e) => {
    if (e.target === root) close();
  });

  function setError(msg: string): void {
    if (statusEl) {
      statusEl.textContent = msg;
      statusEl.classList.toggle("error", !!msg);
    }
  }

  function run(entry: HostEntry, password: string | null, save: boolean): void {
    setError("");
    if (connectBtn) {
      connectBtn.disabled = true;
      connectBtn.textContent = "connecting…";
    }
    void opts
      .onConnect(entry, password, save)
      .then(() => close())
      .catch((err) => {
        setError(`error: ${String(err)}`);
        if (connectBtn) {
          connectBtn.disabled = false;
          connectBtn.textContent = "connect";
        }
      });
  }

  // 手動フォーム送信。
  root.querySelector<HTMLFormElement>(".picker-form")?.addEventListener("submit", (ev) => {
    ev.preventDefault();
    const host = hostInput?.value.trim() ?? "";
    if (!host) return;
    const port = Number(portInput?.value) || DEFAULT_PORT;
    const password = passInput?.value ?? "";
    const save = saveInput?.checked ?? false;
    run({ label: host, host, port }, password || null, save);
  });

  // 保存済みホスト一覧を読み込んで描画 (保存済みは即接続、未保存は手動欄を埋める)。
  void (async () => {
    let hosts: HostEntry[] = [];
    let saved = new Set<string>();
    let defaultPort = DEFAULT_PORT;
    try {
      hosts = await loadRecentHosts();
    } catch {
      /* 補助機能。失敗しても手動入力は使える */
    }
    try {
      saved = new Set(await savedPasswordHosts());
    } catch {
      /* noop */
    }
    try {
      defaultPort = (await loadSettings()).default_port || DEFAULT_PORT;
    } catch {
      /* noop */
    }
    if (portInput && !portInput.value) portInput.value = String(defaultPort);
    if (!list) return;
    if (hosts.length === 0) {
      list.textContent = "";
      return;
    }
    for (const h of hosts) {
      const hasSaved = saved.has(h.host.trim().toLowerCase());
      const row = document.createElement("button");
      row.type = "button";
      row.className = "pk-host-row";
      const name = document.createElement("span");
      name.className = "pk-host-name";
      name.textContent = h.label || h.host;
      const addr = document.createElement("span");
      addr.className = "pk-host-addr";
      addr.textContent = `${h.host}:${h.port}`;
      row.append(name, addr);
      if (hasSaved) {
        const key = document.createElement("span");
        key.className = "pk-host-key";
        key.title = "パスワード保存済み (クリックで即接続)";
        key.innerHTML = icon("key");
        row.appendChild(key);
      }
      row.addEventListener("click", () => {
        if (hasSaved) {
          // 保存済み: 即接続 (password=null, 再保存不要)。
          run(h, null, false);
        } else {
          // 未保存: 手動欄を埋めてパスワード入力を促す。
          if (hostInput) hostInput.value = h.host;
          if (portInput) portInput.value = String(h.port);
          passInput?.focus();
        }
      });
      list.appendChild(row);
    }
  })();

  hostInput?.focus();
}
