// 未接続時のランチャー画面。
// 左サイドバー (ブランド + Hosts/Settings ナビ + 接続ステータス) と、
// 右に「保存ホストのカード一覧」/「設定」ページを切り替えて表示する。
// ＋追加はパスワード無しで即一覧へ保存。カード押下で接続。
//
// パスワードは接続成功時に DPAPI で暗号化保存される (Rust 側 secret.rs)。保存済みの
// host は鍵マーク付きで、カード押下時にダイアログを出さず即接続する (平文はフロントに
// 載せず Rust が復号して使う)。未保存の host は従来どおりパスワード入力ダイアログ。
// recent_hosts.json (config.rs) には引き続き { label, host, port } のみ保持する。

import {
  connectedHosts,
  disconnectHost,
  deleteRecentHost,
  deleteHostPassword,
  focusHostWindow,
  loadRecentHosts,
  loadSettings,
  probeHosts,
  saveRecentHost,
  savedPasswordHosts,
  setRecentHosts,
  saveSettings,
  openLogsFolder,
  exportLogsZip,
  type ClientSettings,
  type HostEntry,
  type HostOverrides,
  type ThemeId,
  type ToolbarPosition,
} from "../core/ipc";
import { logger } from "../core/log";
import { emit, listen } from "@tauri-apps/api/event";
import {
  findPreset,
  matchPreset,
  presetOptionsHtml,
  VIDEO_PRESETS,
  CUSTOM_PRESET_ID,
} from "../core/video-presets";
import { mountWindowControls } from "./window-controls";
import { applyAriaLabels, trapFocus } from "../core/a11y";
import { SHORTCUTS } from "../core/shortcuts";
import { icon } from "./icons";
import "./launcher.css";

// password に null を渡すと Rust 側が保存済みパスワード (DPAPI) を使って接続する。
// newWindow=true は接続中ホストへ「新しいウィンドウで接続」した場合 (通常接続は false)。
type ConnectCallback = (
  entry: HostEntry,
  password: string | null,
  save: boolean,
  newWindow: boolean,
) => Promise<void>;
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
  toolbar_position: "top",
  tab_position: "top",
  accent_color: "#3a82f7",
  theme: "dark",
  fps: 60,
  max_mbps: 8,
  auto_reconnect: true,
  use_virtual_display: false,
  share_files: false,
};

/** ラベル/host から決定的に色を作りアバター背景に使う。
 *  白いイニシャル文字とのコントラストを確保するため明度は低め (38%) に固定する
 *  (黄/緑系の hue でも白文字が読める)。 */
function avatarColor(seed: string): string {
  let h = 0;
  for (let i = 0; i < seed.length; i++) h = (h * 31 + seed.charCodeAt(i)) % 360;
  return `hsl(${h}, 52%, 38%)`;
}

/** 設定 JSON に埋め込む小さなアバター。選択画像を中央クロップで 64px 正方の PNG data URL にする。
 *  ファイルコピー/asset protocol を使わず data URL でそのまま保存・描画できるため軽い。 */
async function imageToIconDataUrl(file: File): Promise<string> {
  const SIZE = 64;
  const bitmap = await createImageBitmap(file);
  try {
    const canvas = document.createElement("canvas");
    canvas.width = SIZE;
    canvas.height = SIZE;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("canvas 2d がありません");
    // アスペクト比維持で正方形へ中央クロップ。
    const side = Math.min(bitmap.width, bitmap.height);
    const sx = (bitmap.width - side) / 2;
    const sy = (bitmap.height - side) / 2;
    ctx.drawImage(bitmap, sx, sy, side, side, 0, 0, SIZE, SIZE);
    return canvas.toDataURL("image/png");
  } finally {
    bitmap.close();
  }
}

/** data URL の画像アバターか判定する (描画分岐・保存値の検証に使う)。 */
function hasImageIcon(icon: string | null | undefined): icon is string {
  return !!icon && icon.startsWith("data:image/");
}

function escapeHtml(s: string): string {
  return s.replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] ?? c,
  );
}

/** アクセント色を CSS 変数へ反映する (全 UI へ波及)。 */
function applyAccent(color: string): void {
  document.documentElement.style.setProperty("--accent", color);
}

/**
 * 配色テーマを <html data-theme> へ反映する (styles/base.css が配色を切り替える)。
 * どのテーマでもユーザー設定のアクセント色を効かせる (legacy のグレー調も含む)。
 */
function applyTheme(theme: ThemeId, accent: string): void {
  document.documentElement.dataset.theme = theme;
  applyAccent(accent);
}

export function mountLauncher(opts: { root: HTMLElement; onConnect: ConnectCallback }): LauncherHandle {
  // ショートカット一覧の行は単一定義 (core/shortcuts.ts) から生成する。
  // label/desc は静的定数なので HTML へ直接埋め込んでよい (ホスト由来データではない)。
  const shortcutRows = SHORTCUTS.map(
    (s) => `
            <div class="setting-row">
              <div class="s-meta"><div class="s-title">${s.desc}</div></div>
              <span class="kbd">${s.label}</span>
            </div>`,
  ).join("");

  opts.root.innerHTML = `
    <div class="launcher">
      <div class="titlebar" data-tauri-drag-region>
        <div class="window-controls" id="win-controls"></div>
      </div>
      <aside class="sidebar">
        <button class="nav-item active" data-page="hosts" title="Hosts"><span class="ico">${icon("hosts")}</span></button>
        <button class="nav-item" data-page="settings" title="Settings"><span class="ico">${icon("settings")}</span></button>
        <div class="sidebar-spacer"></div>
      </aside>
      <main class="content">
        <section class="page active" id="page-hosts">
          <div class="page-head">
            <h2>Hosts</h2>
            <div class="actions">
              <button class="icon-btn" id="probe-refresh" title="オンライン状態を更新">${icon("refresh")}</button>
              <button class="icon-btn" id="add-host" title="ホストを追加">${icon("add")}</button>
            </div>
          </div>
          <div class="host-list" id="host-list"></div>
        </section>
        <section class="page" id="page-settings">
          <div class="page-head"><h2>Settings</h2></div>
          <div class="settings-body">
            <div class="setting-group">映像 (接続時にホストへ適用)</div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">画質プリセット</div>
                <div class="s-desc">fps とビットレートをまとめて設定。手動で変えるとカスタムになる</div>
              </div>
              <select id="set-preset" class="select-input">${presetOptionsHtml()}</select>
            </div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">送出 fps 上限</div>
                <div class="s-desc">0 で無制限。高リフレッシュレートで遅延が増えるのを防ぐ</div>
              </div>
              <input type="text" inputmode="numeric" id="set-fps" class="num-input" />
            </div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">ビットレート上限 (Mbps)</div>
                <div class="s-desc">この値を上限に、輻輳時は自動で下げる (適応制御)</div>
              </div>
              <input type="text" inputmode="numeric" id="set-mbps" class="num-input" />
            </div>
            <div class="setting-group">接続</div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">切断時に自動で再接続</div>
                <div class="s-desc">通信断などで切れたとき、間隔を空けて自動的に接続し直す</div>
              </div>
              <label class="switch"><input type="checkbox" id="set-reconnect" /><span class="slider"></span></label>
            </div>
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
                <div class="s-title">仮想ディスプレイを使う (RDP 風)</div>
                <div class="s-desc">ホストに仮想ディスプレイがあれば、それをキャプチャしウィンドウに解像度を合わせる (黒帯解消)。物理画面とは別の空のデスクトップになる点に注意</div>
              </div>
              <label class="switch"><input type="checkbox" id="set-virtual" /><span class="slider"></span></label>
            </div>
            <div class="setting-group">ツールバー</div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">配置</div>
                <div class="s-desc">操作バーを画面のどの端に置くか。左右に置くと縦並びのサイドバーになる</div>
              </div>
              <select id="set-tb-position" class="select-input">
                <option value="top">上</option>
                <option value="bottom">下</option>
                <option value="left">左</option>
                <option value="right">右</option>
              </select>
            </div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">最初からピン留め (常時表示)</div>
                <div class="s-desc">OFF だと接続時は格納され、配置した端にカーソルを合わせると出てくる</div>
              </div>
              <label class="switch"><input type="checkbox" id="set-tb-pinned" /><span class="slider"></span></label>
            </div>
            <div class="setting-group">タブ</div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">配置</div>
                <div class="s-desc">接続先タブを画面のどの端に置くか。左右に置くと縦並びのサイドタブになる</div>
              </div>
              <select id="set-tab-position" class="select-input">
                <option value="top">上</option>
                <option value="bottom">下</option>
                <option value="left">左</option>
                <option value="right">右</option>
              </select>
            </div>
            <div class="setting-group">外観・既定値</div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">テーマ</div>
                <div class="s-desc">配色。auto は OS のダーク/ライト設定に追従。legacy はグレーのクラシック調</div>
              </div>
              <select id="set-theme" class="select-input">
                <option value="auto">システム連動</option>
                <option value="dark">ダーク</option>
                <option value="light">ライト</option>
                <option value="legacy">レガシー</option>
              </select>
            </div>
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
            <div class="setting-group">ショートカット (接続中のローカル操作)</div>
            ${shortcutRows}
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">マウスキャプチャ解除</div>
                <div class="s-desc">キャプチャ中にポインタロックを抜ける</div>
              </div>
              <span class="kbd">Esc</span>
            </div>
            <div class="setting-group">ファイル共有</div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">この PC をドライブとして共有</div>
                <div class="s-desc">接続中、<b>この PC の This PC 全体 (全ドライブ)</b> がホスト側に仮想ドライブとして現れ、ホストの Explorer から閲覧・コピー・書き込みできる。安全のため、ホストに<b>登録済みの端末</b>のときだけドライブが出る</div>
              </div>
              <label class="switch"><input type="checkbox" id="set-share-files" /><span class="slider"></span></label>
            </div>
            <div class="setting-group">診断</div>
            <div class="setting-row">
              <div class="s-meta">
                <div class="s-title">ログ</div>
                <div class="s-desc">不具合の調査用。フォルダを開くか、まとめて ZIP に書き出して共有できる</div>
              </div>
              <div class="s-actions">
                <button class="btn-secondary" id="open-logs">フォルダを開く</button>
                <button class="btn-secondary" id="export-logs">ZIP 出力</button>
              </div>
            </div>
          </div>
        </section>
      </main>
      <footer class="statusbar" id="statusbar">
        <span class="sb-dot"></span>
        <span class="sb-text" id="statusbar-text">未接続</span>
      </footer>
      <div id="card-menu" class="card-menu"></div>
      <div id="dialog-backdrop"></div>
    </div>
  `;

  const $ = <T extends HTMLElement>(sel: string) => opts.root.querySelector<T>(sel);
  const statusBar = $("#statusbar") as HTMLElement;
  const statusText = $("#statusbar-text") as HTMLElement;
  const hostList = $("#host-list") as HTMLElement;
  const backdrop = $("#dialog-backdrop") as HTMLElement;
  const cardMenu = $("#card-menu") as HTMLElement;
  // パスワード保存済みの host(小文字) 集合。refreshHosts で更新し、カードの
  // 鍵マーク表示・即接続・「忘れる」メニューの出し分けに使う。
  let savedSet = new Set<string>();
  const hasSaved = (host: string): boolean => savedSet.has(host.trim().toLowerCase());
  // 現在接続中の host(小文字) 集合。`aurora://connections` イベント (Rust 発) で更新し、
  // カードの「接続中」表示・切断ボタンの出し分けに使う。
  let connectedSet = new Set<string>();
  const isConnected = (host: string): boolean => connectedSet.has(host.trim().toLowerCase());
  const fullscreenToggle = $<HTMLInputElement>("#set-fullscreen");
  const statsToggle = $<HTMLInputElement>("#set-stats");
  const captureToggle = $<HTMLInputElement>("#set-capture");
  const reconnectToggle = $<HTMLInputElement>("#set-reconnect");
  const virtualToggle = $<HTMLInputElement>("#set-virtual");
  const tbPositionSelect = $<HTMLSelectElement>("#set-tb-position");
  const tabPositionSelect = $<HTMLSelectElement>("#set-tab-position");
  const tbPinnedToggle = $<HTMLInputElement>("#set-tb-pinned");
  const accentInput = $<HTMLInputElement>("#set-accent");
  const themeSelect = $<HTMLSelectElement>("#set-theme");
  const portInput = $<HTMLInputElement>("#set-port");
  const presetSelect = $<HTMLSelectElement>("#set-preset");
  const fpsInput = $<HTMLInputElement>("#set-fps");
  const mbpsInput = $<HTMLInputElement>("#set-mbps");
  const shareFilesToggle = $<HTMLInputElement>("#set-share-files");

  const winControls = $("#win-controls");
  if (winControls) mountWindowControls(winControls);

  // ナビ (Hosts/Settings) や ＋追加 などアイコンのみボタンに aria-label を補う。
  applyAriaLabels(opts.root);

  // 診断: ログフォルダを開く / ログを ZIP 出力。
  $("#open-logs")?.addEventListener("click", () => {
    void openLogsFolder().catch((e) => logger.warn("open logs folder failed", e));
  });
  // 一時メッセージ (ログ出力完了など) が接続状態表示を占有し続けないよう自動で戻すタイマー。
  let statusRevertTimer: number | null = null;
  $("#export-logs")?.addEventListener("click", () => {
    void exportLogsZip()
      .then((path) => {
        statusText.textContent = `ログを出力しました: ${path}`;
        if (statusRevertTimer !== null) clearTimeout(statusRevertTimer);
        statusRevertTimer = window.setTimeout(() => {
          statusRevertTimer = null;
          updateStatusBar();
        }, 5000);
      })
      .catch((e) => logger.warn("export logs failed", e));
  });

  let settings: ClientSettings = { ...DEFAULT_SETTINGS };

  function persist(): void {
    void saveSettings(settings).catch((e) => logger.warn("save settings failed", e));
    // capture 窓の設定コピー (再接続時の映像パラメータ等が参照) にも即反映させる。
    void emit("aurora://settings-changed", settings);
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
  // 開いているダイアログのフォーカストラップ解除関数 (開くたびに張り、閉じると外す)。
  let releaseTrap: (() => void) | null = null;
  function openBackdrop(): void {
    backdrop.classList.add("visible");
    applyAriaLabels(backdrop);
    releaseTrap?.();
    releaseTrap = trapFocus(backdrop);
  }
  function closeDialog(): void {
    releaseTrap?.();
    releaseTrap = null;
    backdrop.classList.remove("visible");
    backdrop.innerHTML = "";
  }
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeDialog();
  });
  // Esc で開いているダイアログを閉じる (カードメニューは別途下で閉じる)。
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && backdrop.classList.contains("visible")) closeDialog();
  });

  /** 削除などの確認ダイアログ。OK で onConfirm を実行する。 */
  function openConfirmDialog(message: string, onConfirm: () => Promise<void>): void {
    backdrop.innerHTML = `
      <div class="dialog">
        <h3>削除の確認</h3>
        <div class="s-desc">${message}</div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="c-cancel">cancel</button>
          <button type="button" class="btn-danger" id="c-ok">削除</button>
        </div>
      </div>
    `;
    openBackdrop();
    backdrop.querySelector("#c-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector("#c-ok")?.addEventListener("click", () => {
      void onConfirm().finally(closeDialog);
    });
  }

  // --- カードの ⋯ メニュー ---
  function closeCardMenu(): void {
    cardMenu.classList.remove("visible");
    cardMenu.innerHTML = "";
  }
  /** ⋯ ボタン (anchor) の下にメニューを開く (編集は直接ボタンへ移した)。
   *  パスワード保存済みなら「忘れる」項目も出す。 */
  function openCardMenu(anchor: HTMLElement, entry: HostEntry): void {
    const newWinItem = isConnected(entry.host)
      ? `<button class="menu-item" data-m="newwin">${icon("add")} 新しいウィンドウで接続</button>`
      : "";
    const forgetItem = hasSaved(entry.host)
      ? `<button class="menu-item" data-m="forget">${icon("key")} パスワードを忘れる</button>`
      : "";
    cardMenu.innerHTML = `
      ${newWinItem}
      ${forgetItem}
      <button class="menu-item danger" data-m="del">${icon("delete")} 削除</button>
    `;
    cardMenu.classList.add("visible");
    // 表示してから幅を測り、ボタン右端に揃えて配置する。
    const r = anchor.getBoundingClientRect();
    cardMenu.style.top = `${r.bottom + 4}px`;
    cardMenu.style.left = `${Math.max(8, r.right - cardMenu.offsetWidth)}px`;
    cardMenu.querySelector('[data-m="newwin"]')?.addEventListener("click", () => {
      closeCardMenu();
      if (hasSaved(entry.host)) {
        handle.setConnecting(true);
        // 保存済み: 即接続 (password=null は Rust が DPAPI 復号)。newWindow=true で必ず新窓。
        void opts
          .onConnect(entry, null, false, true)
          .then(() => updateStatusBar())
          .catch((e) => {
            logger.warn("open new window failed", e);
            updateStatusBar();
            openConnectDialog(entry, true);
          });
      } else {
        openConnectDialog(entry, true);
      }
    });
    cardMenu.querySelector('[data-m="forget"]')?.addEventListener("click", () => {
      closeCardMenu();
      void (async () => {
        await deleteHostPassword(entry.host);
        await handle.refreshHosts();
      })().catch((e) => logger.warn("forget host password failed", e));
    });
    cardMenu.querySelector('[data-m="del"]')?.addEventListener("click", () => {
      closeCardMenu();
      const name = entry.label || entry.host;
      openConfirmDialog(`「${escapeHtml(name)}」を一覧から削除しますか?`, async () => {
        // 一覧から消すときは保存済みパスワードも一緒に消す (孤立を防ぐ)。
        await deleteHostPassword(entry.host);
        await deleteRecentHost(entry.host);
        await handle.refreshHosts();
      });
    });
  }
  // メニュー外クリック / スクロール / Esc で閉じる。
  document.addEventListener("click", () => closeCardMenu());
  hostList.addEventListener("scroll", () => closeCardMenu());
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeCardMenu();
  });

  /** ホスト追加/編集ダイアログ (label/host/port を保存。接続はしない)。entry=null で新規。 */
  function openHostDialog(entry: HostEntry | null): void {
    const isAdd = entry === null;
    const e: HostEntry = entry ?? { label: "", host: "", port: settings.default_port };
    const o: HostOverrides = e.overrides ?? {};
    const hasQuality = o.fps != null && o.max_mbps != null;
    const qualityId = hasQuality ? matchPreset(o.fps as number, o.max_mbps as number) : "default";
    // 手で JSON を編集した等でプリセット外の値が入っている場合は現値ラベルで残す (保存時は維持)。
    const customOpt =
      qualityId === CUSTOM_PRESET_ID
        ? `<option value="${CUSTOM_PRESET_ID}">カスタム (${o.fps}fps / ${o.max_mbps}Mbps)</option>`
        : "";
    const presetOpts =
      `<option value="default">既定に従う</option>` +
      VIDEO_PRESETS.map((p) => `<option value="${p.id}">${p.label}</option>`).join("") +
      customOpt;
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>${isAdd ? "ホストを追加" : "ホストを編集"}</h3>
        <div class="icon-picker">
          <div class="icon-preview" id="h-icon-preview"></div>
          <div class="icon-picker-actions">
            <button type="button" class="btn-secondary" id="h-icon-pick">画像を選択</button>
            <button type="button" class="btn-ghost" id="h-icon-clear">解除</button>
          </div>
          <input type="file" id="h-icon-file" accept="image/*" hidden />
        </div>
        <label>表示名 (任意)<input id="h-label" type="text" value="${escapeHtml(e.label)}" placeholder="home-pc" /></label>
        <label>host (Tailscale IP / hostname)<input id="h-host" type="text" value="${escapeHtml(e.host)}" placeholder="100.x.x.x" required /></label>
        <label>port<input id="h-port" type="text" inputmode="numeric" value="${e.port}" /></label>
        <div class="dlg-sub">このホストだけの設定 (既定を上書き)</div>
        <label>仮想ディスプレイ
          <select id="h-virtual" class="select-input">
            <option value="default">既定に従う</option>
            <option value="on">使う</option>
            <option value="off">使わない</option>
          </select>
        </label>
        <label>画質プリセット<select id="h-preset" class="select-input">${presetOpts}</select></label>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="h-cancel">cancel</button>
          <button type="submit" class="btn-primary">${isAdd ? "add" : "save"}</button>
        </div>
      </form>
    `;
    openBackdrop();

    const virtualSel = backdrop.querySelector<HTMLSelectElement>("#h-virtual");
    const presetSel = backdrop.querySelector<HTMLSelectElement>("#h-preset");
    if (virtualSel)
      virtualSel.value = o.use_virtual_display == null ? "default" : o.use_virtual_display ? "on" : "off";
    if (presetSel) presetSel.value = qualityId;

    // --- アイコン (画像) 選択 ---
    // 画像未設定なら label/host 由来の頭文字アバターをプレビューに出し、選択中は画像を出す。
    let currentIcon = e.icon ?? "";
    const labelInput = backdrop.querySelector<HTMLInputElement>("#h-label");
    const hostInput = backdrop.querySelector<HTMLInputElement>("#h-host");
    const preview = backdrop.querySelector<HTMLElement>("#h-icon-preview");
    const fileInput = backdrop.querySelector<HTMLInputElement>("#h-icon-file");
    const clearBtn = backdrop.querySelector<HTMLButtonElement>("#h-icon-clear");
    function renderIconPreview(): void {
      if (!preview) return;
      if (hasImageIcon(currentIcon)) {
        preview.style.background = "";
        const img = document.createElement("img");
        img.src = currentIcon; // 自前生成の data URL
        img.alt = "";
        img.draggable = false;
        preview.replaceChildren(img);
      } else {
        const nm = labelInput?.value.trim() || hostInput?.value.trim() || "?";
        preview.style.background = avatarColor(nm);
        preview.textContent = (nm[0] ?? "?").toUpperCase();
      }
      if (clearBtn) clearBtn.style.display = currentIcon ? "" : "none";
    }
    renderIconPreview();
    // 画像未設定のときは頭文字が label/host に追従するよう再描画。
    labelInput?.addEventListener("input", () => {
      if (!hasImageIcon(currentIcon)) renderIconPreview();
    });
    hostInput?.addEventListener("input", () => {
      if (!hasImageIcon(currentIcon)) renderIconPreview();
    });
    backdrop.querySelector("#h-icon-pick")?.addEventListener("click", () => fileInput?.click());
    fileInput?.addEventListener("change", () => {
      const file = fileInput.files?.[0];
      fileInput.value = ""; // 同じファイルを選び直しても change が出るようリセット
      if (!file) return;
      void imageToIconDataUrl(file)
        .then((url) => {
          currentIcon = url;
          renderIconPreview();
        })
        .catch((err) => logger.warn("icon load failed", err));
    });
    clearBtn?.addEventListener("click", () => {
      currentIcon = "";
      renderIconPreview();
    });

    backdrop.querySelector<HTMLInputElement>("#h-host")?.focus();
    backdrop.querySelector("#h-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (ev) => {
      ev.preventDefault();
      const host = backdrop.querySelector<HTMLInputElement>("#h-host")?.value.trim() ?? "";
      const label = backdrop.querySelector<HTMLInputElement>("#h-label")?.value.trim() ?? "";
      const port = Number(backdrop.querySelector<HTMLInputElement>("#h-port")?.value) || DEFAULT_PORT;
      if (!host) return;
      const ov: HostOverrides = {};
      if (virtualSel?.value === "on") ov.use_virtual_display = true;
      else if (virtualSel?.value === "off") ov.use_virtual_display = false;
      const pv = presetSel?.value ?? "default";
      if (pv === CUSTOM_PRESET_ID) {
        // プリセット外の既存値をそのまま維持
        ov.fps = o.fps;
        ov.max_mbps = o.max_mbps;
      } else {
        const p = findPreset(pv);
        if (p) {
          ov.fps = p.fps;
          ov.max_mbps = p.mbps;
        }
      }
      void (async () => {
        // host を変えた編集なら旧エントリを消してから保存する (新 host で重複排除される)。
        const hostChanged = e.host.localeCompare(host, undefined, { sensitivity: "accent" }) !== 0;
        if (!isAdd && hostChanged) {
          await deleteRecentHost(e.host);
        }
        // overrides は空オブジェクトも送る: Rust 側が「既定に従う」への明示リセットとして
        // None へ正規化する (undefined だと「情報なし」扱いで既存値が残ってしまう)。
        await saveRecentHost({ label: label || host, host, port, icon: currentIcon, overrides: ov });
        closeDialog();
        await handle.refreshHosts();
      })().catch((e) => logger.warn("save host failed", e));
    });
  }

  /** 接続ダイアログ (パスワードのみ)。カード押下から呼ぶ。newWindow は接続中ホストへの追加窓接続時に true。 */
  function openConnectDialog(entry: HostEntry, newWindow = false): void {
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>${escapeHtml(entry.label || entry.host)}</h3>
        <div class="s-desc" style="font-family:monospace">${escapeHtml(entry.host)}:${entry.port}</div>
        <label>password
          <span class="pass-wrap">
            <input id="d-pass" type="password" required />
            <button type="button" class="pass-eye" id="d-eye" title="パスワードを表示">${icon("eye")}</button>
          </span>
        </label>
        <label class="dlg-check"><input id="d-save" type="checkbox" checked /> このパスワードを保存する</label>
        <div class="dlg-status" id="d-status"></div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="d-cancel">cancel</button>
          <button type="submit" class="btn-primary" id="d-ok">connect</button>
        </div>
      </form>
    `;
    openBackdrop();
    const passInput = backdrop.querySelector<HTMLInputElement>("#d-pass");
    const okBtn = backdrop.querySelector<HTMLButtonElement>("#d-ok");
    const statusEl = backdrop.querySelector<HTMLElement>("#d-status");
    const eyeBtn = backdrop.querySelector<HTMLButtonElement>("#d-eye");
    eyeBtn?.addEventListener("click", () => {
      if (!passInput) return;
      const show = passInput.type === "password";
      passInput.type = show ? "text" : "password";
      eyeBtn.innerHTML = icon(show ? "eye-off" : "eye");
      eyeBtn.title = show ? "パスワードを隠す" : "パスワードを表示";
      passInput.focus();
    });
    passInput?.focus();
    backdrop.querySelector("#d-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (ev) => {
      ev.preventDefault();
      const password = passInput?.value ?? "";
      if (!password) return;
      const save = backdrop.querySelector<HTMLInputElement>("#d-save")?.checked ?? true;
      if (okBtn) {
        okBtn.disabled = true;
        okBtn.textContent = "connecting…";
      }
      if (statusEl) statusEl.textContent = `connecting to ${entry.host}…`;
      void opts
        .onConnect(entry, password, save, newWindow)
        // 解決 = capture ウィンドウが開いた。ランチャーは残るのでダイアログだけ閉じる。
        .then(() => closeDialog())
        .catch((err) => {
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
  // 現在表示中の並び (ドラッグ並べ替えの基準)。
  let currentHosts: HostEntry[] = [];

  // 到達性プローブ結果 (host 小文字 → online)。未判定はドット非表示。
  const probeMap = new Map<string, boolean>();
  let probing = false;
  /** 未接続の保存ホストへ並列 TCP プローブし、結果をカードのドットへ反映する。 */
  async function runProbe(): Promise<void> {
    if (probing) return;
    const targets = currentHosts
      .filter((h) => !isConnected(h.host))
      .map((h) => ({ host: h.host, port: h.port }));
    if (targets.length === 0) return;
    probing = true;
    try {
      const results = await probeHosts(targets);
      for (const r of results) probeMap.set(r.host.trim().toLowerCase(), r.online);
      renderHosts(currentHosts);
    } catch (e) {
      logger.warn("probe hosts failed", e);
    } finally {
      probing = false;
    }
  }

  /** from の要素を to の位置へ移動し、保存して再描画する。 */
  function reorder(from: number, to: number): void {
    if (from === to || from < 0 || to < 0) return;
    const arr = [...currentHosts];
    const [moved] = arr.splice(from, 1);
    arr.splice(to, 0, moved);
    currentHosts = arr;
    renderHosts(arr);
    void setRecentHosts(arr).catch((e) => logger.warn("reorder hosts failed", e));
  }

  /**
   * カード追従ドラッグ並べ替え。掴んだカードの分身 (clone) がカーソルに追従し、
   * 元カードは空きスロット (placeholder) として他カードの間を移動する。
   * HTML5 DnD は WebView2 で不安定なため pointer イベントで自前実装する。
   */
  function startDrag(from: number, downEvent: PointerEvent, card: HTMLElement): void {
    const rect = card.getBoundingClientRect();
    const offsetY = downEvent.clientY - rect.top;
    let targetPos = from;

    // カーソル追従用の分身。
    const clone = card.cloneNode(true) as HTMLElement;
    clone.classList.add("drag-clone");
    clone.style.width = `${rect.width}px`;
    clone.style.left = `${rect.left}px`;
    clone.style.top = `${rect.top}px`;
    opts.root.appendChild(clone);
    // 元カードは空きスロットにする。
    card.classList.add("drag-placeholder");

    const onMove = (ev: PointerEvent): void => {
      // 分身がホスト一覧の領域外 (タイトルバーや画面外) へ出ないよう上下をクランプする。
      const listRect = hostList.getBoundingClientRect();
      const desiredTop = ev.clientY - offsetY;
      const maxTop = Math.max(listRect.top, listRect.bottom - rect.height);
      const top = Math.min(Math.max(desiredTop, listRect.top), maxTop);
      clone.style.top = `${top}px`;
      // placeholder 以外のカードの中点と比較して挿入位置を決め、元カードを移動する。
      const others = Array.from(hostList.querySelectorAll<HTMLElement>(".host-card")).filter(
        (c) => c !== card,
      );
      let pos = others.length;
      for (let i = 0; i < others.length; i++) {
        const r = others[i].getBoundingClientRect();
        if (ev.clientY < r.top + r.height / 2) {
          pos = i;
          break;
        }
      }
      targetPos = pos;
      if (pos >= others.length) hostList.appendChild(card);
      else hostList.insertBefore(card, others[pos]);
    };
    const onUp = (): void => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      clone.remove();
      card.classList.remove("drag-placeholder");
      reorder(from, targetPos);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  function renderHosts(hosts: HostEntry[]): void {
    currentHosts = hosts;
    if (hosts.length === 0) {
      hostList.innerHTML = `<div class="empty-hint">保存済みのホストはありません。<br>右上の ＋ から追加してください。</div>`;
      return;
    }
    hostList.innerHTML = hosts
      .map((h, i) => {
        const name = h.label || h.host;
        const initial = escapeHtml((name[0] ?? "?").toUpperCase());
        const connected = isConnected(h.host);
        // 画像アイコンがあれば img、無ければ頭文字 + 自動生成色。data URL は自前生成
        // (canvas.toDataURL) のみで base64 文字種なので属性を破れない。
        const avatar = hasImageIcon(h.icon)
          ? `<div class="avatar avatar-img"><img src="${h.icon}" alt="" draggable="false" /></div>`
          : `<div class="avatar" style="background:${avatarColor(name)}">${initial}</div>`;
        // 接続中は緑扱い。プローブ未判定はドットを出さない。
        const online = connected ? true : probeMap.get(h.host.trim().toLowerCase());
        const presence =
          online === undefined ? "" : `<span class="presence ${online ? "online" : "offline"}"></span>`;
        const saved = hasSaved(h.host)
          ? `<span class="saved-badge" title="パスワード保存済み (押すと即接続)">${icon("key")}</span>`
          : "";
        // 接続中: 緑ピル + 切断ボタン。並べ替えグリップは編集系ボタンと同列なので残す。
        const connPill = connected ? `<span class="conn-pill">接続中</span>` : "";
        const tailBtn = connected
          ? `<button class="card-btn danger" data-act="disconnect" title="切断">${icon("power")}</button>`
          : `<span class="chevron">${icon("chevron")}</span>`;
        return `
          <div class="host-card${connected ? " connected" : ""}" data-idx="${i}" tabindex="0" role="button">
            <span class="grip" data-act="grip" title="ドラッグで並べ替え">${icon("grip")}</span>
            <div class="avatar-wrap">${avatar}${presence}</div>
            <div class="meta">
              <div class="label">${escapeHtml(name)}</div>
              <div class="addr">${escapeHtml(h.host)}:${h.port}</div>
            </div>
            ${connPill}
            ${saved}
            <button class="card-btn" data-act="edit" title="編集">${icon("edit")}</button>
            <button class="card-btn" data-act="menu" title="メニュー">${icon("more")}</button>
            ${tailBtn}
          </div>`;
      })
      .join("");
    applyAriaLabels(hostList); // grip/編集/メニュー/切断 などアイコンのみボタンに付与

    hostList.querySelectorAll<HTMLElement>(".host-card").forEach((card) => {
      const idx = Number(card.dataset.idx);
      card.addEventListener("click", (e) => {
        // SVG 内クリックでも拾えるよう data-act を持つ最近接要素から判定する。
        const actEl = (e.target as HTMLElement).closest<HTMLElement>("[data-act]");
        const act = actEl?.dataset.act;
        if (act === "grip") {
          e.stopPropagation(); // 掴むだけのクリックでは接続しない
        } else if (act === "edit") {
          e.stopPropagation();
          openHostDialog(hosts[idx]);
        } else if (act === "menu") {
          e.stopPropagation();
          openCardMenu(actEl as HTMLElement, hosts[idx]);
        } else if (act === "disconnect") {
          e.stopPropagation();
          const host = hosts[idx].host;
          // 該当 host のタブを持つ開いている capture ウィンドウへ切断を依頼する (UI のタブ除去)。
          void emit("aurora://disconnect-host", { host });
          // さらに Rust 側で host 単位に直接停止する。接続中にウィンドウを閉じた等で
          // 残った「窓の無い孤児セッション」(emit が届かず host を占有し続ける) も確実に切る。
          void disconnectHost(host);
        } else if (isConnected(hosts[idx].host)) {
          // 接続中カードは 2 本目の接続を張らず、既存セッションの窓を前面化+タブ活性化する。
          void focusHostWindow(hosts[idx].host)
            .then((found) => {
              // 見つからない = 接続中表示が古い (窓が閉じた直後など)。一覧を取り直す。
              if (!found) void handle.refreshHosts();
            })
            .catch((e) => logger.warn("focus host window failed", e));
        } else if (hasSaved(hosts[idx].host)) {
          // パスワード保存済み: ダイアログを出さず即接続 (平文はフロントに載らない)。
          // 保存値が拒否された等で失敗したら入力ダイアログへフォールバックする。
          const entry = hosts[idx];
          handle.setConnecting(true);
          // 保存済み即接続: password=null (Rust が DPAPI 復号)。再保存は不要なので save=false。
          void opts
            .onConnect(entry, null, false, false)
            // capture ウィンドウが開いたら一時的な connecting 表示を解き、実際の接続中集計へ戻す
            // (接続成立は別窓側で起き、`aurora://connections` 経由でドットが緑になる)。
            .then(() => updateStatusBar())
            .catch((e) => {
              logger.warn("open capture window failed", e);
              updateStatusBar();
              openConnectDialog(entry);
            });
        } else {
          openConnectDialog(hosts[idx]);
        }
      });

      // マウスなし操作: Enter/Space でクリックと同じ分岐へ (target=card なので接続系の分岐に落ちる)。
      card.addEventListener("keydown", (e) => {
        // 子ボタン (編集/メニュー/切断) からのバブリングで接続まで発火しないようカード自身のみ。
        if (e.target !== card) return;
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          card.click();
        }
      });

      // grip を掴んでドラッグ並べ替え (カードがカーソルに追従、他カードが隙間を空ける)。
      card.querySelector<HTMLElement>(".grip")?.addEventListener("pointerdown", (e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        closeCardMenu();
        startDrag(idx, e, card);
      });
    });
  }

  // --- アクション配線 ---
  $("#add-host")?.addEventListener("click", () => openHostDialog(null));
  $("#probe-refresh")?.addEventListener("click", () => void runProbe());

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
  reconnectToggle?.addEventListener("change", () => {
    settings = { ...settings, auto_reconnect: reconnectToggle.checked };
    persist();
  });
  virtualToggle?.addEventListener("change", () => {
    settings = { ...settings, use_virtual_display: virtualToggle.checked };
    persist();
  });
  tbPositionSelect?.addEventListener("change", () => {
    settings = { ...settings, toolbar_position: tbPositionSelect.value as ToolbarPosition };
    persist();
  });
  tabPositionSelect?.addEventListener("change", () => {
    settings = { ...settings, tab_position: tabPositionSelect.value as ToolbarPosition };
    persist();
  });
  tbPinnedToggle?.addEventListener("change", () => {
    settings = { ...settings, toolbar_pinned: tbPinnedToggle.checked };
    persist();
  });
  accentInput?.addEventListener("input", () => {
    settings = { ...settings, accent_color: accentInput.value };
    applyAccent(accentInput.value);
    persist();
  });
  themeSelect?.addEventListener("change", () => {
    const theme = themeSelect.value as ThemeId;
    settings = { ...settings, theme };
    applyTheme(theme, settings.accent_color);
    persist();
  });
  portInput?.addEventListener("change", () => {
    const port = Number(portInput.value) || DEFAULT_PORT;
    settings = { ...settings, default_port: port };
    portInput.value = String(port);
    persist();
  });
  /** fps/mbps の現在値からプリセット選択を一致させる (手動値なら「カスタム」)。 */
  function syncPresetSelect(): void {
    if (presetSelect) presetSelect.value = matchPreset(settings.fps, settings.max_mbps);
  }
  presetSelect?.addEventListener("change", () => {
    const p = findPreset(presetSelect.value);
    if (!p) return; // カスタム: fps/Mbps の手動値を維持する
    settings = { ...settings, fps: p.fps, max_mbps: p.mbps };
    if (fpsInput) fpsInput.value = String(p.fps);
    if (mbpsInput) mbpsInput.value = String(p.mbps);
    persist();
  });
  fpsInput?.addEventListener("change", () => {
    const fps = Math.max(0, Math.floor(Number(fpsInput.value) || 0));
    settings = { ...settings, fps };
    fpsInput.value = String(fps);
    syncPresetSelect();
    persist();
  });
  mbpsInput?.addEventListener("change", () => {
    const mbps = Math.max(1, Math.floor(Number(mbpsInput.value) || 1));
    settings = { ...settings, max_mbps: mbps };
    mbpsInput.value = String(mbps);
    syncPresetSelect();
    persist();
  });
  shareFilesToggle?.addEventListener("change", () => {
    settings = { ...settings, share_files: shareFilesToggle.checked };
    persist();
  });

  // 下端ステータスバーを実際の接続中 host 数に追従させる。マルチウィンドウでは接続は capture
  // 窓側で起きるため、launcher は `aurora://connections` の集計 (connectedSet) だけを映す。
  function updateStatusBar(): void {
    const n = connectedSet.size;
    handle.setConnState(n > 0 ? `接続中 ${n} 件` : "未接続", n > 0 ? "connected" : "idle");
  }

  const handle: LauncherHandle = {
    show(): void {
      opts.root.classList.remove("hidden");
    },
    hide(): void {
      opts.root.classList.add("hidden");
      closeDialog();
    },
    setConnecting(connecting: boolean): void {
      if (connecting) handle.setConnState("接続中…", "connecting");
    },
    setConnState(text: string, kind: ConnState = "idle"): void {
      // 下端ステータスバーにドット色 + テキストで状態を出す。
      statusText.textContent = text;
      statusBar.classList.remove("connecting", "connected", "error");
      if (kind !== "idle") statusBar.classList.add(kind);
    },
    async refreshHosts(): Promise<void> {
      try {
        // 鍵マーク/即接続の判定用に、保存済みパスワードの host 集合も更新する。
        try {
          savedSet = new Set(await savedPasswordHosts());
        } catch {
          savedSet = new Set();
        }
        // 接続中表示・切断ボタンの判定用に、接続中 host 集合も更新する。
        try {
          connectedSet = new Set((await connectedHosts()).map((h) => h.trim().toLowerCase()));
        } catch {
          connectedSet = new Set();
        }
        renderHosts(await loadRecentHosts());
        updateStatusBar();
        void runProbe();
      } catch {
        // 履歴は補助機能。失敗しても致命的ではない。
      }
    },
    getSettings(): ClientSettings {
      return settings;
    },
  };

  /** 現在の settings を設定フォームの各コントロールへ反映する (テーマ/アクセント色は即適用)。 */
  function applySettingsToForm(): void {
    if (fullscreenToggle) fullscreenToggle.checked = settings.default_fullscreen;
    if (statsToggle) statsToggle.checked = settings.show_stats;
    if (captureToggle) captureToggle.checked = settings.default_capture;
    if (reconnectToggle) reconnectToggle.checked = settings.auto_reconnect;
    if (virtualToggle) virtualToggle.checked = settings.use_virtual_display;
    if (tbPositionSelect) tbPositionSelect.value = settings.toolbar_position;
    if (tabPositionSelect) tabPositionSelect.value = settings.tab_position;
    if (tbPinnedToggle) tbPinnedToggle.checked = settings.toolbar_pinned;
    if (accentInput) accentInput.value = settings.accent_color;
    if (themeSelect) themeSelect.value = settings.theme;
    if (portInput) portInput.value = String(settings.default_port);
    if (fpsInput) fpsInput.value = String(settings.fps);
    if (mbpsInput) mbpsInput.value = String(settings.max_mbps);
    if (shareFilesToggle) shareFilesToggle.checked = settings.share_files;
    syncPresetSelect();
    applyTheme(settings.theme, settings.accent_color);
  }

  // 初期ロード: 設定を読み込んでコントロールへ反映。
  void (async () => {
    try {
      settings = await loadSettings();
    } catch {
      settings = { ...DEFAULT_SETTINGS };
    }
    applySettingsToForm();
  })();

  // 他ウィンドウ (capture のツールバー等) が設定を保存したら、この窓のコピーとフォームを
  // 追従させる。古いコピーのまま persist() すると他窓の変更を巻き戻すため必須。
  // 自分の persist() の同報も受けるが、同値の再反映になるだけで無害。
  void listen<ClientSettings>("aurora://settings-changed", (e) => {
    settings = e.payload;
    applySettingsToForm();
  });
  void handle.refreshHosts();

  // 接続中 host の変化 (capture ウィンドウでの接続/切断/ウィンドウ破棄) を購読し、
  // 接続中表示・切断ボタン・保存バッジ (接続成功で新規保存された分) を更新する。
  void listen<string[]>("aurora://connections", (e) => {
    connectedSet = new Set(e.payload.map((h) => h.trim().toLowerCase()));
    // 保存バッジ (新規保存) の反映も兼ねて履歴ごと再描画する。
    void handle.refreshHosts();
  });

  return handle;
}
