// capture ウィンドウのエントリ (orchestrator)。
// 1 ウィンドウに複数の Session (= 接続先タブ) を保持し、アクティブな 1 つだけを共有 UI
// (canvas / toolbar / status / stats / file パネル) へ結びつける。タブ切替で旧 Session を
// deactivate (映像一時停止・入力 detach)、新 Session を activate する。
// URL クエリ (?session=<id>&host=...&port=...&save=0|1) で最初のタブを開く。資格情報 (password)
// は Rust 側 staging が持つためここには載らない。「+」で同じ窓に新しいタブを追加できる。

import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { emit, listen } from "@tauri-apps/api/event";
import "./styles/base.css";
import {
  DEFAULT_SETTINGS,
  findCaptureWindowAt,
  handoffSessionToWindow,
  initPerfFlag,
  loadRecentHosts,
  loadSettings,
  openSessionWindow,
  saveSettings,
  stageSession,
  takeHandoff,
  unstageSession,
  uploadFileFromPath,
  type ClientSettings,
  type DisplayDto,
  type HostEntry,
} from "./core/ipc";
import { Session, type SessionHost, type SessionSnapshot, type StatsEvent } from "./core/session";
import { mountToolbar, type ToolbarHandle } from "./ui/toolbar";
import { mountTabs, type TabsHandle } from "./ui/tabs";
import { openConnectPicker } from "./ui/connect-dialog";
import { initToast, showToast, type ToastKind } from "./ui/toast";
import {
  clearStats,
  initStatsOverlay,
  setStatsPosition,
  toggleStats,
  updateStats,
} from "./ui/stats-overlay";
import { hideReconnectBadge, initReconnectBadge, showReconnectBadge } from "./ui/reconnect-badge";
import { handleFileProgress, handleFileResult, mountFileTransfer } from "./ui/file-transfer";
import {
  initCaptureOverlay,
  openConfirmOverlay,
  renderStatusOverlay,
  showDropHint,
} from "./ui/capture-overlay";
import { installWebviewGuard } from "./core/webview-guard";
import { applyShortcutBindings } from "./core/shortcuts";
import { exitFullscreen, isFullscreen, toggleFullscreen } from "./core/fullscreen";
import { closeWindow, minimizeWindow, setResizable } from "./ui/window-controls";
import { installGlobalErrorLogging, logger } from "./core/log";

// 未捕捉例外/Promise reject を Rust ログ (client.log) へ転送する。
installGlobalErrorLogging();

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const tabbar = document.getElementById("tabbar") as HTMLElement;
const statsHud = document.getElementById("stats-hud") as HTMLElement;
const filePanel = document.getElementById("file-panel") as HTMLElement;
const toastContainer = document.getElementById("toast-container") as HTMLElement;
const statusEl = document.getElementById("conn-status") as HTMLElement;
const pickerBackdrop = document.getElementById("picker-backdrop") as HTMLElement;
const reconnectBadgeEl = document.getElementById("reconnect-badge") as HTMLElement;

initToast(toastContainer);
initStatsOverlay(statsHud);
initReconnectBadge(reconnectBadgeEl);
mountFileTransfer(filePanel);
initCaptureOverlay(statusEl);

// --- 設定 (ウィンドウ内の全タブで共有。capture には設定 UI がない) ---
let settings: ClientSettings | null = null;
function getSettings(): ClientSettings {
  return settings ?? DEFAULT_SETTINGS;
}

// --- セッション管理 ---
const sessions = new Map<string, Session>();
let activeId: string | null = null;
function active(): Session | null {
  return activeId ? (sessions.get(activeId) ?? null) : null;
}

function setActive(id: string): void {
  if (activeId === id) return;
  const prev = activeId ? sessions.get(activeId) : null;
  prev?.deactivate();
  activeId = id;
  tabs.setActive(id);
  sessions.get(id)?.activate();
}

/** セッションが恒久終了した: タブ除去・アクティブ付替え・最後なら窓を閉じる。 */
function removeSession(s: Session): void {
  sessions.delete(s.id);
  tabs.remove(s.id);
  if (sessions.size === 0) {
    void closeWindow();
    return;
  }
  if (activeId === s.id) {
    activeId = null;
    const next = sessions.keys().next().value;
    if (next) setActive(next);
  }
}

// --- Session ↔ 共有 UI の窓口 ---
const host: SessionHost = {
  canvas,
  get toolbar(): ToolbarHandle {
    return toolbar;
  },
  getSettings,
  isActive: (s) => activeId === s.id,
  renderStatus: (s) => {
    if (activeId !== s.id) return;
    renderStatusOverlay(s.status, s.statusText, {
      // 切断後のエラー画面の「閉じる」/接続中の「キャンセル」は当該タブを閉じる (= 最後なら窓も閉じる)。
      onClose: s.statusShowClose ? () => s.closeByUser() : undefined,
      // エラー画面では再試行カウンタをリセットして即やり直せる「再接続」を出す (B-1)。
      onReconnect: s.status === "error" ? () => s.manualReconnect() : undefined,
    });
  },
  toast: (s, text, kind?: ToastKind) => {
    if (activeId === s.id) showToast(text, kind);
  },
  updateStats: (s, ev: StatsEvent) => {
    if (activeId === s.id) updateStats(ev, s.videoCodec);
  },
  clearStats: (s) => {
    if (activeId === s.id) clearStats();
  },
  fileResult: (s, id, ok, message) => {
    if (activeId === s.id) handleFileResult(id, ok, message);
  },
  fileProgress: (s, id, done, total) => {
    if (activeId === s.id) handleFileProgress(id, done, total);
  },
  reconnectBadge: (s, text) => {
    if (activeId === s.id) showReconnectBadge(text);
  },
  clearReconnectBadge: (s) => {
    if (activeId === s.id) hideReconnectBadge();
  },
  refreshTab: (s) => tabs.update(s.id, s.tabLabel, s.tabStatus),
  removed: (s) => removeSession(s),
  setResizable: (on) => void setResizable(on),
  persistVideoParams: (fps, mbps) => {
    // ローカルコピーを即更新してから保存し、他窓 (ランチャー/別 capture 窓) へも同報する。
    // 同報しないと、古いコピーを持つ窓が次に persist したとき変更が巻き戻る。
    settings = { ...getSettings(), fps, max_mbps: mbps };
    void saveSettings(settings).catch((e) => logger.warn("save settings failed", e));
    void emit("aurora://settings-changed", settings);
  },
};

// 他ウィンドウの設定保存に追従する (この窓の再接続・新規タブが古い値で始まらないように)。
// ツールバー/タブの配置は接続を跨がず即反映する (再接続や新窓を開くまで古い配置が
// 残ると、設定を変えたのに効かないように見える)。
void listen<ClientSettings>("aurora://settings-changed", (e) => {
  settings = e.payload;
  tabs.setPosition(e.payload.tab_position);
  toolbar.setPosition(e.payload.toolbar_position);
  setStatsPosition(e.payload.stats_position);
  // ショートカット割当が変わった可能性があるので判定テーブルとツールバーの表記を更新する。
  applyShortcutBindings(e.payload.shortcuts);
  toolbar.refreshShortcutLabels();
  // コーデック設定 (グローバル/ホスト別 override) が変わった可能性があるので、接続中の全
  // セッションへ再計算させる (変化が無ければ内部で no-op)。
  for (const s of sessions.values()) void s.refreshCodecFromSettings();
  // fps/ビットレート上限も再接続を待たず接続中の全セッションへ反映する (A-3、差分時のみ送出)。
  for (const s of sessions.values()) s.refreshVideoParamsFromSettings();
});

// --- 共有ツールバー (操作は常にアクティブ Session へ) ---
let fileVisible = false;
function setFilePanel(visible: boolean): void {
  fileVisible = visible;
  filePanel.classList.toggle("visible", visible);
  toolbar.setFileToggled(visible);
}

async function doToggleFullscreen(): Promise<void> {
  const on = await toggleFullscreen();
  toolbar.setFullscreenToggled(on);
  // フルスクリーン中はタブストリップを自動格納する (上端の黒帯を消し、映像を端まで届かせる)。
  tabs.setFullscreen(on);
  // フルスクリーンを抜けると Tauri がウィンドウを作成時状態へ戻すため、接続中はリサイズ可を再適用。
  if (!on && active()?.isConnected) void setResizable(true);
  showToast(on ? "fullscreen on" : "fullscreen off");
}

const toolbar: ToolbarHandle = mountToolbar({
  topbar: document.getElementById("topbar") as HTMLElement,
  onDisconnect: () => active()?.closeByUser(),
  onSelectDisplay: (d: DisplayDto) => active()?.switchDisplay(d),
  onToggleStats: () => {
    toggleStats();
    toolbar.setStatsToggled(statsHud.classList.contains("visible"));
  },
  onToggleFilePanel: () => setFilePanel(!fileVisible),
  onToggleFullscreen: () => void doToggleFullscreen(),
  onToggleCapture: () => {
    const s = active();
    if (s) s.setCaptureMode(!s.isCaptured());
  },
  onToggleMute: (muted) => active()?.setAudioMuted(muted),
  onToggleClipboard: (enabled) => active()?.setClipboardSyncEnabled(enabled),
  onSetVideoParams: (fps, mbps) => active()?.setVideoParams(fps, mbps),
  onSetResolution: (maxHeight) => active()?.setResolution(maxHeight),
});

// --- タブストリップ ---
const tabs: TabsHandle = mountTabs(tabbar, {
  onSelect: (id) => setActive(id),
  onClose: (id) => sessions.get(id)?.closeByUser(),
  onAdd: () => openAddTab(),
  onDetach: (id, sx, sy) => void splitTab(id, sx, sy),
  onCloseOthers: (id) => {
    for (const s of [...sessions.values()]) if (s.id !== id) s.closeByUser();
  },
  onMoveToNewWindow: (id) => void moveTabToNewWindow(id),
  onReconnect: (id) => sessions.get(id)?.manualReconnect(),
  onMinimizeWindow: () => void minimizeWindow(),
  onCloseWindow: () => void closeWindow(), // 複数タブ時の確認は onCloseRequested が一元的に出す
});

// --- ウィンドウを閉じる確認 (複数タブ時のみ)。OS の閉じる操作・メニュー経由の close() を
// どちらもここで受けるため、確認ダイアログはこの 1 箇所に集約される。 ---
// 注意: onCloseRequested リスナーを 1 つでも登録すると、Tauri v2 は close の最終実行を JS へ
// 移し、preventDefault しない場合に JS API 内部が destroy() を呼ぶ。そのため capabilities に
// `core:window:allow-destroy` が必須 (無いと全クローズが ACL 拒否で窓が閉じなくなる。
// ブラウザハーネスでは ACL が効かず検出できない — 2026-07-19 実機で発覚)。
void getCurrentWindow().onCloseRequested((e) => {
  if (sessions.size <= 1) return; // 単独タブはそのまま閉じる (セッションは Rust 側が破棄する)
  e.preventDefault();
  openConfirmOverlay(`${sessions.size} 件の接続を閉じてウィンドウを終了します。`, "すべて閉じる", () => {
    // 各タブを正規の手順で閉じる (最後の removeSession が窓を閉じる)。
    for (const s of [...sessions.values()]) s.closeByUser();
  });
});

// --- タブ切替ショートカット (Ctrl+Alt+Shift+←/→ と 1〜9 直接ジャンプ) ---
function orderedSessionIds(): string[] {
  return [...sessions.keys()];
}
function cycleTab(dir: 1 | -1): void {
  const ids = orderedSessionIds();
  if (ids.length < 2 || !activeId) return;
  const idx = ids.indexOf(activeId);
  setActive(ids[(idx + dir + ids.length) % ids.length]);
}
function jumpToTab(n: number): void {
  const ids = orderedSessionIds();
  const id = ids[n - 1];
  if (id) setActive(id);
}

// --- タブメニューの「新しいウィンドウへ移動」: 合流判定なしで常に新窓を作る ---
async function moveTabToNewWindow(id: string): Promise<void> {
  const s = sessions.get(id);
  if (!s || !s.isConnected) return;
  try {
    // 現在窓から少しずらした位置に出す (どこへ出たか見失わないように)。
    await openSessionWindow(id, JSON.stringify(s.snapshot()), window.screenX + 80, window.screenY + 80);
  } catch (e) {
    showToast(`ウィンドウへの移動に失敗: ${String(e)}`, "error");
    return;
  }
  s.detachForHandoff();
  removeSession(s);
}

// --- タブをウィンドウ外へドラッグ: 別ウィンドウへ分割 (networking 無停止で移譲) ---
async function splitTab(id: string, screenX: number, screenY: number): Promise<void> {
  const s = sessions.get(id);
  // 接続確立済みのタブだけ移譲する (接続中/エラー中は表示状態が定まらない)。
  if (!s || !s.isConnected) return;
  // ドロップ点が他の capture 窓の上なら新窓を作らず合流 (Brave 的)。判定失敗は分割へフォールバック。
  const targetLabel = await findCaptureWindowAt(screenX, screenY).catch(() => null);
  try {
    if (targetLabel) {
      await handoffSessionToWindow(id, targetLabel, JSON.stringify(s.snapshot()));
    } else {
      await openSessionWindow(id, JSON.stringify(s.snapshot()), screenX, screenY);
    }
  } catch (e) {
    showToast(`分割に失敗: ${String(e)}`, "error");
    return;
  }
  // 新ウィンドウ生成 = Rust 側で所有権付替え + sink デタッチ済み。この窓のローカル資源だけ解放し、
  // タブを除去する (disconnect は呼ばない。最後の 1 枚なら removeSession が窓を閉じる)。
  s.detachForHandoff();
  removeSession(s);
}

// --- 「+」: 同じ窓に新しいタブ (接続) を追加 ---
function openAddTab(): void {
  openConnectPicker(pickerBackdrop, {
    onConnect: async (entry: HostEntry, password: string | null, save: boolean) => {
      // 同一 host のタブを同じ窓に 2 枚開いても意味がないため既存タブへ切替える。
      const target = entry.host.trim().toLowerCase();
      const existing = [...sessions.values()].find(
        (s) => s.entry.host.trim().toLowerCase() === target,
      );
      if (existing) {
        setActive(existing.id);
        showToast(`${entry.host} は既にこのウィンドウで接続中です`);
        return;
      }
      // session_id 発番 + password を staging へ積む (窓は作らない)。
      const id = await stageSession(password);
      await addSession(id, entry, save, /*activate=*/ true);
    },
  });
}

/** Session を生成してタブに追加し、接続を開始する。 */
async function addSession(
  id: string,
  entry: HostEntry,
  save: boolean,
  activateNew: boolean,
): Promise<void> {
  // URL/add-tab 経由の entry にはホスト別オーバーライドが載らないため、保存済み履歴から補完する。
  if (!entry.overrides) {
    try {
      const hosts = await loadRecentHosts();
      const target = entry.host.trim().toLowerCase();
      const m = hosts.find((h) => h.host.trim().toLowerCase() === target);
      if (m?.overrides) entry = { ...entry, overrides: m.overrides };
    } catch {
      // 履歴は補助情報。読めなくてもグローバル設定で接続は成立する。
    }
  }
  const s = new Session(id, entry, save, host);
  sessions.set(id, s);
  tabs.add(id, s.tabLabel, "connecting");
  if (activateNew) setActive(id);
  await s.start();
}

// フルスクリーン＋キャプチャからの「脱出」: マウス解放 + 全画面解除 + ツールバー表示。
// Esc 2 連打 (webview-guard) から呼ぶ。フルスクリーン中は keyboard lock で Esc がホストへ
// 流れ単押しでは抜けられないため、確実に client 側へ戻れる手段として用意する。
// 脱出対象 (キャプチャ中 or 全画面) があったら実行して true、無ければ false (Esc はホストへ)。
function breakout(): boolean {
  const s = active();
  const capturing = !!s?.isCaptured();
  const fs = isFullscreen();
  if (!capturing && !fs) return false;
  if (capturing) s?.setCaptureMode(false);
  if (fs) {
    void exitFullscreen().then(() => {
      // doToggleFullscreen の解除側と同じ UI 復帰 (ボタン状態 / タブ再表示 / リサイズ可)。
      toolbar.setFullscreenToggled(false);
      tabs.setFullscreen(false);
      if (active()?.isConnected) void setResizable(true);
    });
  }
  // 抜けた直後にツールバーが自動格納だと操作不能なので確実に表示する。
  toolbar.revealBar();
  showToast("マウス解放・全画面解除しました");
  return true;
}

// --- ローカル UI ホットキー ---
installWebviewGuard(
  (hotkey) => {
    if (hotkey === "toggle-stats") {
      toggleStats();
      toolbar.setStatsToggled(statsHud.classList.contains("visible"));
    } else if (hotkey === "toggle-bar") {
      toolbar.toggleBar();
    } else if (hotkey === "toggle-fullscreen") {
      void doToggleFullscreen();
    } else if (hotkey === "toggle-capture") {
      const s = active();
      if (s) s.setCaptureMode(!s.isCaptured());
    } else if (hotkey === "tab-prev") {
      cycleTab(-1);
    } else if (hotkey === "tab-next") {
      cycleTab(1);
    }
  },
  breakout,
  jumpToTab,
);

// --- ランチャーからの host 単位の切断要求 ---
// ランチャーの切断ボタンが `aurora://disconnect-host` を全ウィンドウへ emit する。
// この窓が該当 host のタブを持っていれば閉じる (closeByUser = 切断 + タブ除去、最後なら窓終了)。
void listen<{ host: string }>("aurora://disconnect-host", (e) => {
  const target = e.payload.host.trim().toLowerCase();
  for (const s of [...sessions.values()]) {
    if (s.entry.host.trim().toLowerCase() === target) s.closeByUser();
  }
});

// --- launcher からの接続をこの窓のタブとして追加 (Rust が最後にフォーカスされた capture 窓へルーティング) ---
type AddTabPayload = { session_id: string; host: string; port: number; label: string; save: boolean };

void getCurrentWebviewWindow().listen<AddTabPayload>("aurora://add-tab", (e) => {
  void (async () => {
    const p = e.payload;
    // ユーザーは接続操作をした直後なので対象窓を前面に出す。失敗しても追加処理は続行する。
    try {
      const win = getCurrentWindow();
      if (await win.isMinimized()) await win.unminimize();
      await win.setFocus();
    } catch {
      // 前面化に失敗してもタブ追加自体は続行する。
    }
    const target = p.host.trim().toLowerCase();
    const existing = [...sessions.values()].find((s) => s.entry.host.trim().toLowerCase() === target);
    if (existing) {
      setActive(existing.id);
      showToast(`${p.host} は既にこのウィンドウで接続中です`);
      void unstageSession(p.session_id);
      return;
    }
    try {
      await addSession(p.session_id, { host: p.host, port: p.port, label: p.label }, p.save, true);
    } catch (e) {
      showToast(`接続に失敗しました: ${String(e)}`, "error");
    }
  })();
});

// --- launcher の接続中カードクリック: 該当 host のタブをアクティブ化 (窓の前面化は Rust 側で済んでいる) ---
void getCurrentWebviewWindow().listen<{ host: string }>("aurora://activate-host", (e) => {
  const target = e.payload.host.trim().toLowerCase();
  const s = [...sessions.values()].find((x) => x.entry.host.trim().toLowerCase() === target);
  if (s) setActive(s.id);
});

// --- 別ウィンドウからドラッグされたタブを引き受ける (タブのドラッグ合流) ---
type AdoptTabPayload = { session_id: string };

void getCurrentWebviewWindow().listen<AdoptTabPayload>("aurora://adopt-tab", (e) => {
  void (async () => {
    const p = e.payload;
    // ドロップ先の窓は既に見えているので unminimize は不要、前面化のみ行う。
    try {
      await getCurrentWindow().setFocus();
    } catch {
      // 前面化に失敗してもタブ引き受け自体は続行する。
    }
    await adoptTab(p.session_id, false);
  })();
});

// --- 窓リサイズ → アクティブ Session のビューポート追従 ---
window.addEventListener("resize", () => active()?.onWindowResize());

// --- キーボード占有 (WH_KEYBOARD_LL) が奪った Win/Alt+Tab 等をアクティブ Session へ注入 ---
// grab の ON/OFF はマウスキャプチャ中のセッションが呼ぶが、転送先はウィンドウ単位 (Rust 側が
// 呼び出し元 window を記録) のため、届いたキーは常にこの窓の「今の」アクティブタブへ渡す。
void getCurrentWebviewWindow().listen<{ vk: number; down: boolean }>("grabbed-key", (e) => {
  active()?.sendGrabbedKey(e.payload.vk, e.payload.down);
});

// --- キャプチャ画面へのファイル D&D アップロード (client → host、アクティブ Session 宛) ---
let nextUploadId = Date.now() >>> 0;
async function uploadDroppedPaths(paths: string[]): Promise<void> {
  for (const path of paths) {
    const id = nextUploadId;
    nextUploadId = (nextUploadId + 1) >>> 0;
    const name = path.replace(/^.*[\\/]/, "");
    showToast(`アップロード中: ${name}`);
    try {
      await uploadFileFromPath(id, path);
    } catch (e) {
      showToast(`アップロード失敗: ${String(e)}`, "error");
    }
  }
}

void getCurrentWebview().onDragDropEvent((event) => {
  const p = event.payload;
  if (!active()?.isConnected) {
    showDropHint(false);
    return;
  }
  if (p.type === "enter" || p.type === "over") {
    showDropHint(true);
  } else if (p.type === "leave") {
    showDropHint(false);
  } else if (p.type === "drop") {
    showDropHint(false);
    if (p.paths.length > 0) void uploadDroppedPaths(p.paths);
  }
});

// --- 別ウィンドウから移譲されたタブを引き受ける (ドラッグ分割の受け側) ---
// initial: 起動直後の唯一のタブとして引き受けるとき true。false は稼働中の窓へ既存タブ群に
// 合流させるとき (失敗時に窓全体をエラー画面にせず、既存タブの表示を壊さないため分岐する)。
async function adoptTab(id: string, initial: boolean): Promise<void> {
  let snap: SessionSnapshot;
  try {
    const json = await takeHandoff(id);
    if (!json) throw new Error("引き継ぎデータがありません");
    snap = JSON.parse(json) as SessionSnapshot;
  } catch (e) {
    const msg = `タブの引き継ぎに失敗しました: ${String(e)}`;
    // セッション未成立のエラーなので、閉じる = 窓ごと終了。
    if (initial) renderStatusOverlay("error", msg, { onClose: () => void closeWindow() });
    else showToast(msg, "error");
    return;
  }
  logger.info(`adopting handed-off tab: ${id} (${snap.entry.host})`);
  const s = new Session(id, snap.entry, snap.save, host);
  try {
    await s.adoptFrom(snap);
  } catch (e) {
    const msg = `アタッチに失敗しました: ${String(e)}`;
    if (initial) renderStatusOverlay("error", msg, { onClose: () => void closeWindow() });
    else showToast(msg, "error");
    return;
  }
  sessions.set(id, s);
  tabs.add(id, s.tabLabel, "connected");
  setActive(id); // activate(): 映像再開 (unpause) + keyframe + 入力 attach + 共有 UI 同期
  void setResizable(true);
  if (snap.captured) s.setCaptureMode(true);
  // 稼働中の窓は既にユーザーが決めた表示状態があるため、統計 HUD の自動 ON は初回起動時のみ。
  if (initial && getSettings().show_stats && !statsHud.classList.contains("visible")) {
    toggleStats();
    toolbar.setStatsToggled(true);
  }
}

// --- 起動: 設定を読み、URL に応じて「最初のタブを接続」or「移譲タブを引き受け」 ---
const params = new URLSearchParams(window.location.search);
const adoptId = params.get("adopt");

void (async () => {
  // 計測フラグ (AURORA_PERF) を最初に取得しておく (decoder の vstat 集計が参照する)。
  await initPerfFlag();
  try {
    settings = await loadSettings();
  } catch {
    settings = null; // getSettings() の既定にフォールバック
  }
  // タブストリップの配置 (上下左右) を設定から適用する (窓単位・全タブ共有)。
  tabs.setPosition(getSettings().tab_position);
  setStatsPosition(getSettings().stats_position);
  // ショートカット割当を判定テーブルへ反映する。トップバーは既に静的ラベルで mount 済み
  // (Tauri 起動のたびに settings 読み込みが非同期で少し遅れるため) なので、ここで表記を
  // 最新化する。
  applyShortcutBindings(getSettings().shortcuts);
  toolbar.refreshShortcutLabels();
  logger.info(`capture window ready (mode=${adoptId ? "adopt" : "connect"})`);
  if (adoptId) {
    await adoptTab(adoptId, true);
    return;
  }
  const initialId = params.get("session") ?? "";
  const initialHost = params.get("host") ?? "";
  const initialEntry: HostEntry = {
    host: initialHost,
    port: Number(params.get("port")) || 0,
    // 表示名 (label) が空のときだけ host にフォールバックする。
    label: params.get("label") || initialHost,
  };
  const initialSave = params.get("save") === "1";
  if (!initialId || !initialEntry.host || !initialEntry.port) {
    renderStatusOverlay("error", "接続情報が不正です", { onClose: () => void closeWindow() });
    return;
  }
  renderStatusOverlay("connecting", `connecting to ${initialEntry.host}…`, {
    onClose: () => active()?.closeByUser(),
  });
  try {
    await addSession(initialId, initialEntry, initialSave, /*activate=*/ true);
    // 接続時の自動フルスクリーン (窓単位、最初のタブのみで判断)。
    if (getSettings().default_fullscreen && !isFullscreen()) void doToggleFullscreen();
    if (getSettings().show_stats && !statsHud.classList.contains("visible")) {
      toggleStats();
      toolbar.setStatsToggled(true);
    }
  } catch (e) {
    renderStatusOverlay("error", `接続に失敗しました: ${String(e)}`, {
      onClose: () => {
        const s = sessions.get(initialId);
        if (s) s.closeByUser();
        else void closeWindow();
      },
      onReconnect: () => sessions.get(initialId)?.manualReconnect(),
    });
  }
})();
