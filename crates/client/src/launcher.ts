// 未接続時のランチャー画面。
// 左サイドバー (ブランド + Hosts/Settings ナビ + 接続ステータス) と、
// 右に「保存ホストのカード一覧」/「設定」ページを切り替えて表示する。
// ＋追加はパスワード無しで即一覧へ保存。カード押下でパスワード入力 → 接続。
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
import { mountWindowControls } from "./window-controls";
import { icon } from "./icons";

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
const DEFAULT_SETTINGS: ClientSettings = {
  default_fullscreen: false,
  show_stats: false,
  default_capture: false,
  default_port: DEFAULT_PORT,
  toolbar_pinned: false,
  accent_color: "#3a82f7",
};

/** ラベル/host から決定的に淡い色を作りアバター背景に使う。 */
function avatarColor(seed: string): string {
  let h = 0;
  for (let i = 0; i < seed.length; i++) h = (h * 31 + seed.charCodeAt(i)) % 360;
  return `hsl(${h}, 45%, 45%)`;
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c] ?? c);
}

/** アクセント色を CSS 変数へ反映する (全 UI へ波及)。 */
function applyAccent(color: string): void {
  document.documentElement.style.setProperty("--accent", color);
}

export function mountLauncher(opts: { root: HTMLElement; onConnect: ConnectCallback }): LauncherHandle {
  opts.root.innerHTML = `
    <div class="launcher">
      <div class="titlebar" data-tauri-drag-region>
        <div class="window-controls" id="win-controls"></div>
      </div>
      <aside class="sidebar">
        <button class="nav-item active" data-page="hosts"><span class="ico">${icon("hosts")}</span> Hosts</button>
        <button class="nav-item" data-page="settings"><span class="ico">${icon("settings")}</span> Settings</button>
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
              <button class="icon-btn" id="add-host" title="ホストを追加">${icon("add")}</button>
              <button class="icon-btn" id="refresh-hosts" title="再読み込み">${icon("refresh")}</button>
            </div>
          </div>
          <div class="host-list" id="host-list"></div>
        </section>
        <section class="page" id="page-settings">
          <div class="page-head"><h2>Settings</h2></div>
          <div class="settings-body">
            <div class="setting-group">接続</div>
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
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">マウスキャプチャを自動で有効化</div>
                <div class="s-desc">接続時にポインタロック (相対マウス) を有効化。Esc で解除</div>
              </div>
              <label class="switch"><input type="checkbox" id="set-capture" /><span class="slider"></span></label>
            </div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">ツールバーを最初から表示</div>
                <div class="s-desc">接続後、上部の操作バーをピン留めして常時表示</div>
              </div>
              <label class="switch"><input type="checkbox" id="set-pinned" /><span class="slider"></span></label>
            </div>
            <div class="setting-group">外観・既定値</div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">アクセントカラー</div>
                <div class="s-desc">ボタンや選択中の表示に使う色</div>
              </div>
              <input type="color" id="set-accent" class="color-input" />
            </div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">新規ホストの既定ポート</div>
                <div class="s-desc">「＋追加」ダイアログの port 欄の初期値</div>
              </div>
              <input type="text" inputmode="numeric" id="set-port" class="num-input" />
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
  const captureToggle = $<HTMLInputElement>("#set-capture");
  const pinnedToggle = $<HTMLInputElement>("#set-pinned");
  const accentInput = $<HTMLInputElement>("#set-accent");
  const portInput = $<HTMLInputElement>("#set-port");

  const winControls = $("#win-controls");
  if (winControls) mountWindowControls(winControls);

  let settings: ClientSettings = { ...DEFAULT_SETTINGS };

  function persist(): void {
    void saveSettings(settings).catch(() => {});
  }

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

  /** ホスト追加/編集ダイアログ (label/host/port を保存。接続はしない)。entry=null で新規。 */
  function openHostDialog(entry: HostEntry | null): void {
    const isAdd = entry === null;
    const e: HostEntry = entry ?? { label: "", host: "", port: settings.default_port };
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>${isAdd ? "ホストを追加" : "ホストを編集"}</h3>
        <label>表示名 (任意)<input id="h-label" type="text" value="${escapeHtml(e.label)}" placeholder="home-pc" /></label>
        <label>host (Tailscale IP / hostname)<input id="h-host" type="text" value="${escapeHtml(e.host)}" placeholder="100.x.x.x" required /></label>
        <label>port<input id="h-port" type="text" inputmode="numeric" value="${e.port}" /></label>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="h-cancel">cancel</button>
          <button type="submit" class="btn-primary">${isAdd ? "add" : "save"}</button>
        </div>
      </form>
    `;
    backdrop.classList.add("visible");
    backdrop.querySelector<HTMLInputElement>("#h-host")?.focus();
    backdrop.querySelector("#h-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (ev) => {
      ev.preventDefault();
      const host = backdrop.querySelector<HTMLInputElement>("#h-host")?.value.trim() ?? "";
      const label = backdrop.querySelector<HTMLInputElement>("#h-label")?.value.trim() ?? "";
      const port = Number(backdrop.querySelector<HTMLInputElement>("#h-port")?.value) || DEFAULT_PORT;
      if (!host) return;
      void (async () => {
        // host を変えた編集なら旧エントリを消してから保存する (新 host で重複排除される)。
        const hostChanged = e.host.localeCompare(host, undefined, { sensitivity: "accent" }) !== 0;
        if (!isAdd && hostChanged) {
          await deleteRecentHost(e.host);
        }
        await saveRecentHost({ label: label || host, host, port });
        closeDialog();
        await handle.refreshHosts();
      })().catch(() => {});
    });
  }

  /** 接続ダイアログ (パスワードのみ)。カード押下から呼ぶ。 */
  function openConnectDialog(entry: HostEntry): void {
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>${escapeHtml(entry.label || entry.host)}</h3>
        <div class="s-desc" style="font-family:monospace">${escapeHtml(entry.host)}:${entry.port}</div>
        <label>password<input id="d-pass" type="password" required /></label>
        <div class="dlg-status" id="d-status"></div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="d-cancel">cancel</button>
          <button type="submit" class="btn-primary" id="d-ok">connect</button>
        </div>
      </form>
    `;
    backdrop.classList.add("visible");
    const passInput = backdrop.querySelector<HTMLInputElement>("#d-pass");
    const okBtn = backdrop.querySelector<HTMLButtonElement>("#d-ok");
    const statusEl = backdrop.querySelector<HTMLElement>("#d-status");
    passInput?.focus();
    backdrop.querySelector("#d-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (ev) => {
      ev.preventDefault();
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
            <button class="card-btn" data-act="edit" title="編集">${icon("edit")}</button>
            <button class="card-btn" data-act="del" title="削除">${icon("delete")}</button>
            <span class="chevron">${icon("chevron")}</span>
          </div>`;
      })
      .join("");

    hostList.querySelectorAll<HTMLElement>(".host-card").forEach((card) => {
      const idx = Number(card.dataset.idx);
      card.addEventListener("click", (e) => {
        // SVG 内クリックでも拾えるよう data-act を持つ最近接要素から判定する。
        const act = (e.target as HTMLElement).closest<HTMLElement>("[data-act]")?.dataset.act;
        if (act === "edit") {
          openHostDialog(hosts[idx]);
        } else if (act === "del") {
          void deleteRecentHost(hosts[idx].host).then(() => handle.refreshHosts());
        } else {
          openConnectDialog(hosts[idx]);
        }
      });
    });
  }

  // --- アクション配線 ---
  $("#add-host")?.addEventListener("click", () => openHostDialog(null));
  $("#refresh-hosts")?.addEventListener("click", () => void handle.refreshHosts());

  fullscreenToggle?.addEventListener("change", () => {
    settings = { ...settings, default_fullscreen: fullscreenToggle.checked };
    persist();
  });
  statsToggle?.addEventListener("change", () => {
    settings = { ...settings, show_stats: statsToggle.checked };
    persist();
  });
  captureToggle?.addEventListener("change", () => {
    settings = { ...settings, default_capture: captureToggle.checked };
    persist();
  });
  pinnedToggle?.addEventListener("change", () => {
    settings = { ...settings, toolbar_pinned: pinnedToggle.checked };
    persist();
  });
  accentInput?.addEventListener("input", () => {
    settings = { ...settings, accent_color: accentInput.value };
    applyAccent(accentInput.value);
    persist();
  });
  portInput?.addEventListener("change", () => {
    const port = Number(portInput.value) || DEFAULT_PORT;
    settings = { ...settings, default_port: port };
    portInput.value = String(port);
    persist();
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

  // 初期ロード: 設定を読み込んでコントロールへ反映 (アクセント色は即適用)。
  void (async () => {
    try {
      settings = await loadSettings();
    } catch {
      settings = { ...DEFAULT_SETTINGS };
    }
    if (fullscreenToggle) fullscreenToggle.checked = settings.default_fullscreen;
    if (statsToggle) statsToggle.checked = settings.show_stats;
    if (captureToggle) captureToggle.checked = settings.default_capture;
    if (pinnedToggle) pinnedToggle.checked = settings.toolbar_pinned;
    if (accentInput) accentInput.value = settings.accent_color;
    if (portInput) portInput.value = String(settings.default_port);
    applyAccent(settings.accent_color);
  })();
  void handle.refreshHosts();

  return handle;
}
