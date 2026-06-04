// 未接続時のランチャー画面。
// 左サイドバー (ブランド + Hosts/Settings ナビ + 接続ステータス) と、
// 右に「保存ホストのカード一覧」/「設定」ページを切り替えて表示する。
// カード押下でパスワード入力ダイアログ → 接続。接続中はランチャーを隠し canvas を全画面化。
//
// パスワードは保存しない方針 (config.rs)。カードには { label, host, port } のみ保持する。

import {
  deleteRecentHost,
  loadRecentHosts,
  loadSettings,
  saveRecentHost,
  saveSettings,
  type ClientSettings,
  type HostEntry,
} from "./ipc";

type ConnectCallback = (entry: HostEntry, password: string) => Promise<void>;
type ConnState = "idle" | "connecting" | "connected" | "error";

export type LauncherHandle = {
  show: () => void;
  hide: () => void;
  setConnecting: (connecting: boolean) => void;
  setConnState: (text: string, kind?: ConnState) => void;
  refreshHosts: () => Promise<void>;
  getSettings: () => ClientSettings;
};

const DEFAULT_PORT = 7000;
const DEFAULT_SETTINGS: ClientSettings = { default_fullscreen: false, show_stats: false };

/** ラベル/host から決定的に淡い色を作りアバター背景に使う。 */
function avatarColor(seed: string): string {
  let h = 0;
  for (let i = 0; i < seed.length; i++) h = (h * 31 + seed.charCodeAt(i)) % 360;
  return `hsl(${h}, 45%, 45%)`;
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c] ?? c);
}

export function mountLauncher(opts: { root: HTMLElement; onConnect: ConnectCallback }): LauncherHandle {
  opts.root.innerHTML = `
    <div class="launcher">
      <aside class="sidebar">
        <div class="brand"><div class="logo">D</div><div class="name">DDD</div></div>
        <button class="nav-item active" data-page="hosts"><span class="ico">🖥</span> Hosts</button>
        <button class="nav-item" data-page="settings"><span class="ico">⚙</span> Settings</button>
        <div class="sidebar-spacer"></div>
        <div class="status-card" id="status-card">
          <span class="dot"></span><span class="txt" id="status-txt">未接続</span>
        </div>
      </aside>
      <main class="content">
        <section class="page active" id="page-hosts">
          <div class="page-head">
            <h2>Hosts</h2>
            <div class="actions">
              <button class="icon-btn" id="add-host" title="ホストを追加">＋</button>
              <button class="icon-btn" id="refresh-hosts" title="再読み込み">⟳</button>
            </div>
          </div>
          <div class="host-list" id="host-list"></div>
        </section>
        <section class="page" id="page-settings">
          <div class="page-head"><h2>Settings</h2></div>
          <div class="settings-body">
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">接続時に自動フルスクリーン</div>
                <div class="s-desc">接続が成立したら全画面表示にする</div>
              </div>
              <label class="switch"><input type="checkbox" id="set-fullscreen" /><span class="slider"></span></label>
            </div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">統計 HUD を表示</div>
                <div class="s-desc">接続時に fps / ビットレート / RTT / loss を右上に表示</div>
              </div>
              <label class="switch"><input type="checkbox" id="set-stats" /><span class="slider"></span></label>
            </div>
          </div>
        </section>
      </main>
      <div id="dialog-backdrop"></div>
    </div>
  `;

  const $ = <T extends HTMLElement>(sel: string) => opts.root.querySelector<T>(sel);
  const statusCard = $("#status-card") as HTMLElement;
  const statusTxt = $("#status-txt") as HTMLElement;
  const hostList = $("#host-list") as HTMLElement;
  const backdrop = $("#dialog-backdrop") as HTMLElement;
  const fullscreenToggle = $<HTMLInputElement>("#set-fullscreen");
  const statsToggle = $<HTMLInputElement>("#set-stats");

  let settings: ClientSettings = { ...DEFAULT_SETTINGS };

  // --- ページ切替 ---
  opts.root.querySelectorAll<HTMLButtonElement>(".nav-item").forEach((btn) => {
    btn.addEventListener("click", () => {
      const page = btn.dataset.page;
      opts.root.querySelectorAll(".nav-item").forEach((b) => b.classList.toggle("active", b === btn));
      opts.root.querySelectorAll(".page").forEach((p) => {
        p.classList.toggle("active", p.id === `page-${page}`);
      });
    });
  });

  // --- ダイアログ ---
  function closeDialog(): void {
    backdrop.classList.remove("visible");
    backdrop.innerHTML = "";
  }
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeDialog();
  });

  /** 接続ダイアログ。prefill 有り=パスワードのみ、無し=ホスト新規追加 (全項目)。 */
  function openConnectDialog(prefill: HostEntry | null): void {
    const isAdd = prefill === null;
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>${isAdd ? "ホストを追加して接続" : escapeHtml(prefill.label || prefill.host)}</h3>
        ${
          isAdd
            ? `<label>表示名 (任意)<input id="d-label" type="text" placeholder="home-pc" /></label>
               <label>host (Tailscale IP / hostname)<input id="d-host" type="text" placeholder="100.x.x.x" required /></label>
               <label>port<input id="d-port" type="number" min="1" max="65535" value="${DEFAULT_PORT}" /></label>`
            : `<div class="s-desc" style="font-family:monospace">${escapeHtml(prefill.host)}:${prefill.port}</div>`
        }
        <label>password<input id="d-pass" type="password" required /></label>
        <div class="dlg-status" id="d-status"></div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="d-cancel">cancel</button>
          <button type="submit" class="btn-primary" id="d-ok">connect</button>
        </div>
      </form>
    `;
    backdrop.classList.add("visible");
    const form = backdrop.querySelector<HTMLFormElement>(".dialog");
    const passInput = backdrop.querySelector<HTMLInputElement>("#d-pass");
    const okBtn = backdrop.querySelector<HTMLButtonElement>("#d-ok");
    const statusEl = backdrop.querySelector<HTMLElement>("#d-status");
    backdrop.querySelector("#d-cancel")?.addEventListener("click", closeDialog);
    (isAdd ? backdrop.querySelector<HTMLInputElement>("#d-host") : passInput)?.focus();

    form?.addEventListener("submit", (e) => {
      e.preventDefault();
      let entry: HostEntry;
      if (isAdd) {
        const host = backdrop.querySelector<HTMLInputElement>("#d-host")?.value.trim() ?? "";
        const label = backdrop.querySelector<HTMLInputElement>("#d-label")?.value.trim() ?? "";
        const port = Number(backdrop.querySelector<HTMLInputElement>("#d-port")?.value) || DEFAULT_PORT;
        if (!host) return;
        entry = { label: label || host, host, port };
      } else {
        entry = prefill;
      }
      const password = passInput?.value ?? "";
      if (!password) return;
      if (okBtn) {
        okBtn.disabled = true;
        okBtn.textContent = "connecting…";
      }
      if (statusEl) statusEl.textContent = `connecting to ${entry.host}…`;
      void opts.onConnect(entry, password).catch((err) => {
        if (statusEl) {
          statusEl.textContent = `error: ${String(err)}`;
          statusEl.classList.add("error");
        }
        if (okBtn) {
          okBtn.disabled = false;
          okBtn.textContent = "connect";
        }
      });
    });
  }

  /** ホスト編集ダイアログ (label/host/port を保存。接続はしない)。 */
  function openEditDialog(entry: HostEntry): void {
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>ホストを編集</h3>
        <label>表示名<input id="e-label" type="text" value="${escapeHtml(entry.label)}" /></label>
        <label>host<input id="e-host" type="text" value="${escapeHtml(entry.host)}" required /></label>
        <label>port<input id="e-port" type="number" min="1" max="65535" value="${entry.port}" /></label>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="e-cancel">cancel</button>
          <button type="submit" class="btn-primary">save</button>
        </div>
      </form>
    `;
    backdrop.classList.add("visible");
    backdrop.querySelector("#e-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (e) => {
      e.preventDefault();
      const host = backdrop.querySelector<HTMLInputElement>("#e-host")?.value.trim() ?? "";
      const label = backdrop.querySelector<HTMLInputElement>("#e-label")?.value.trim() ?? "";
      const port = Number(backdrop.querySelector<HTMLInputElement>("#e-port")?.value) || DEFAULT_PORT;
      if (!host) return;
      // host が変わった場合は旧 host の履歴を消してから新エントリを保存する。
      const oldHost = entry.host;
      void (async () => {
        if (!oldHost.localeCompare(host, undefined, { sensitivity: "accent" })) {
          // 同一 host: save が host で重複排除するのでそのまま上書きされる。
        } else {
          await deleteRecentHost(oldHost);
        }
        await saveRecentHost({ label: label || host, host, port });
        closeDialog();
        await handle.refreshHosts();
      })().catch(() => {});
    });
  }

  // --- ホスト一覧の描画 ---
  function renderHosts(hosts: HostEntry[]): void {
    if (hosts.length === 0) {
      hostList.innerHTML = `<div class="empty-hint">保存済みのホストはありません。<br>右上の ＋ から追加してください。</div>`;
      return;
    }
    hostList.innerHTML = hosts
      .map((h, i) => {
        const name = h.label || h.host;
        const initial = escapeHtml((name[0] ?? "?").toUpperCase());
        return `
          <div class="host-card" data-idx="${i}">
            <div class="avatar" style="background:${avatarColor(name)}">${initial}</div>
            <div class="meta">
              <div class="label">${escapeHtml(name)}</div>
              <div class="addr">${escapeHtml(h.host)}:${h.port}</div>
            </div>
            <button class="card-btn" data-act="edit" data-idx="${i}" title="編集">✎</button>
            <button class="card-btn" data-act="del" data-idx="${i}" title="削除">🗑</button>
            <span class="chevron">›</span>
          </div>`;
      })
      .join("");

    hostList.querySelectorAll<HTMLElement>(".host-card").forEach((card) => {
      const idx = Number(card.dataset.idx);
      card.addEventListener("click", (e) => {
        const target = e.target as HTMLElement;
        const act = target.dataset.act;
        if (act === "edit") {
          openEditDialog(hosts[idx]);
        } else if (act === "del") {
          void deleteRecentHost(hosts[idx].host).then(() => handle.refreshHosts());
        } else {
          openConnectDialog(hosts[idx]);
        }
      });
    });
  }

  // --- アクション配線 ---
  $("#add-host")?.addEventListener("click", () => openConnectDialog(null));
  $("#refresh-hosts")?.addEventListener("click", () => void handle.refreshHosts());

  fullscreenToggle?.addEventListener("change", () => {
    settings = { ...settings, default_fullscreen: fullscreenToggle.checked };
    void saveSettings(settings).catch(() => {});
  });
  statsToggle?.addEventListener("change", () => {
    settings = { ...settings, show_stats: statsToggle.checked };
    void saveSettings(settings).catch(() => {});
  });

  const handle: LauncherHandle = {
    show(): void {
      opts.root.classList.remove("hidden");
    },
    hide(): void {
      opts.root.classList.add("hidden");
      closeDialog();
    },
    setConnecting(connecting: boolean): void {
      if (connecting) handle.setConnState("connecting…", "connecting");
    },
    setConnState(text: string, kind: ConnState = "idle"): void {
      statusTxt.textContent = text;
      statusCard.classList.remove("connecting", "connected", "error");
      if (kind !== "idle") statusCard.classList.add(kind);
    },
    async refreshHosts(): Promise<void> {
      try {
        renderHosts(await loadRecentHosts());
      } catch {
        // 履歴は補助機能。失敗しても致命的ではない。
      }
    },
    getSettings(): ClientSettings {
      return settings;
    },
  };

  // 初期ロード: 設定とホスト一覧。
  void (async () => {
    try {
      settings = await loadSettings();
    } catch {
      settings = { ...DEFAULT_SETTINGS };
    }
    if (fullscreenToggle) fullscreenToggle.checked = settings.default_fullscreen;
    if (statsToggle) statsToggle.checked = settings.show_stats;
  })();
  void handle.refreshHosts();

  return handle;
}
