// 未接続時の中央接続カード (AnyDesk 風)。
// host(履歴 datalist 補完) + password + connect。接続中はカードを隠し canvas を全画面に。
// ディスプレイ切替・disconnect は接続中ツールバー (toolbar.ts) 側へ移譲した。

import { loadRecentHosts } from "./ipc";

type FormCallback = (host: string, password: string) => Promise<void>;

export type ConnectionUiHandle = {
  show: () => void;
  hide: () => void;
  setStatus: (text: string, isError?: boolean) => void;
  setConnecting: (connecting: boolean) => void;
  refreshHosts: () => Promise<void>;
};

export function mountConnectionUi(opts: { root: HTMLElement; onSubmit: FormCallback }): ConnectionUiHandle {
  opts.root.innerHTML = `
    <form id="connect-card" autocomplete="off">
      <h1>DDD Remote Desktop</h1>
      <label>host (Tailscale IP / hostname)
        <input id="host" type="text" list="recent-hosts" placeholder="100.x.x.x or hostname" required />
        <datalist id="recent-hosts"></datalist>
      </label>
      <label>password
        <input id="password" type="password" required />
      </label>
      <button type="submit" id="connect-btn">connect</button>
      <div id="connect-status"></div>
    </form>
  `;

  const form = opts.root.querySelector<HTMLFormElement>("#connect-card");
  const hostInput = opts.root.querySelector<HTMLInputElement>("#host");
  const passwordInput = opts.root.querySelector<HTMLInputElement>("#password");
  const connectBtn = opts.root.querySelector<HTMLButtonElement>("#connect-btn");
  const statusEl = opts.root.querySelector<HTMLElement>("#connect-status");
  const datalist = opts.root.querySelector<HTMLDataListElement>("#recent-hosts");

  const handle: ConnectionUiHandle = {
    show(): void {
      opts.root.classList.remove("hidden");
      // 再表示時はパスワードを消し、host にフォーカス。
      if (passwordInput) passwordInput.value = "";
      hostInput?.focus();
    },
    hide(): void {
      opts.root.classList.add("hidden");
    },
    setStatus(text: string, isError = false): void {
      if (!statusEl) return;
      statusEl.textContent = text;
      statusEl.classList.toggle("error", isError);
    },
    setConnecting(connecting: boolean): void {
      if (connectBtn) {
        connectBtn.disabled = connecting;
        connectBtn.textContent = connecting ? "connecting…" : "connect";
      }
    },
    async refreshHosts(): Promise<void> {
      if (!datalist) return;
      try {
        const hosts = await loadRecentHosts();
        datalist.innerHTML = hosts.map((h) => `<option value="${h}"></option>`).join("");
        // 直近の host を既定で入れておく (未入力時のみ)。
        if (hostInput && !hostInput.value && hosts.length > 0) hostInput.value = hosts[0];
      } catch {
        // 履歴は補助機能なので失敗しても接続自体は妨げない。
      }
    },
  };

  form?.addEventListener("submit", (e) => {
    e.preventDefault();
    const host = hostInput?.value.trim() ?? "";
    const password = passwordInput?.value ?? "";
    if (!host || !password) return;
    handle.setStatus(`connecting to ${host}…`);
    handle.setConnecting(true);
    void opts.onSubmit(host, password).catch((err) => {
      handle.setStatus(`error: ${String(err)}`, true);
      handle.setConnecting(false);
    });
  });

  void handle.refreshHosts();
  return handle;
}
