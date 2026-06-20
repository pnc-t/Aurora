// capture ウィンドウのエントリ (orchestrator)。
// 1 ウィンドウに複数の Session (= 接続先タブ) を保持し、アクティブな 1 つだけを共有 UI
// (canvas / toolbar / status / stats / file パネル) へ結びつける。タブ切替で旧 Session を
// deactivate (映像一時停止・入力 detach)、新 Session を activate する。
// URL クエリ (?session=<id>&host=...&port=...&save=0|1) で最初のタブを開く。資格情報 (password)
// は Rust 側 staging が持つためここには載らない。「+」で同じ窓に新しいタブを追加できる。

import { getCurrentWebview } from "@tauri-apps/api/webview";
import "./styles/base.css";
import {
  loadSettings,
  openSessionWindow,
  stageSession,
  takeHandoff,
  uploadFileFromPath,
  type ClientSettings,
  type DisplayDto,
  type HostEntry,
} from "./core/ipc";
import {
  Session,
  type SessionHost,
  type SessionSnapshot,
  type StatsEvent,
  type StatusKind,
} from "./core/session";
import { mountToolbar, type ToolbarHandle } from "./ui/toolbar";
import { mountTabs, type TabsHandle } from "./ui/tabs";
import { openConnectPicker } from "./ui/connect-dialog";
import { initToast, showToast, type ToastKind } from "./ui/toast";
import { clearStats, initStatsOverlay, toggleStats, updateStats } from "./ui/stats-overlay";
import { handleFileResult, mountFileTransfer } from "./ui/file-transfer";
import { installWebviewGuard } from "./core/webview-guard";
import { isFullscreen, toggleFullscreen } from "./core/fullscreen";
import { closeWindow, setResizable } from "./ui/window-controls";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const tabbar = document.getElementById("tabbar") as HTMLElement;
const statsHud = document.getElementById("stats-hud") as HTMLElement;
const filePanel = document.getElementById("file-panel") as HTMLElement;
const toastContainer = document.getElementById("toast-container") as HTMLElement;
const statusEl = document.getElementById("conn-status") as HTMLElement;
const pickerBackdrop = document.getElementById("picker-backdrop") as HTMLElement;

initToast(toastContainer);
initStatsOverlay(statsHud);
mountFileTransfer(filePanel);

// --- 設定 (ウィンドウ内の全タブで共有。capture には設定 UI がない) ---
let settings: ClientSettings | null = null;
function getSettings(): ClientSettings {
  return (
    settings ?? {
      default_fullscreen: false,
      show_stats: false,
      default_capture: false,
      default_port: 7000,
      toolbar_pinned: false,
      toolbar_position: "top",
      accent_color: "#3a82f7",
      theme: "dark",
      fps: 60,
      max_mbps: 15,
      auto_reconnect: true,
      use_virtual_display: false,
    }
  );
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

// --- 中央ステータスオーバーレイ (アクティブ Session の状態を描画) ---
// reason は host 由来を含みうるので textContent で組む (XSS 注意)。
function renderStatusOverlay(kind: StatusKind, text: string, showClose: boolean): void {
  if (kind === "hidden") {
    statusEl.style.display = "none";
    statusEl.replaceChildren();
    return;
  }
  Object.assign(statusEl.style, {
    position: "fixed",
    inset: "0",
    display: "grid",
    placeItems: "center",
    font: "500 15px system-ui, sans-serif",
    color: "#fff",
    background: "rgba(14,16,22,0.92)",
    zIndex: "5000",
  });
  const card = document.createElement("div");
  Object.assign(card.style, {
    display: "grid",
    gap: "14px",
    justifyItems: "center",
    padding: "24px 28px",
    maxWidth: "70vw",
    textAlign: "center",
  });
  const msg = document.createElement("div");
  msg.textContent = text;
  if (kind === "error") msg.style.color = "#ff8b8b";
  card.appendChild(msg);
  if (showClose) {
    const btn = document.createElement("button");
    btn.textContent = "閉じる";
    Object.assign(btn.style, {
      padding: "8px 18px",
      borderRadius: "8px",
      border: "1px solid rgba(255,255,255,0.25)",
      background: "rgba(255,255,255,0.08)",
      color: "#fff",
      cursor: "pointer",
    });
    // 切断後のエラー画面の「閉じる」は当該タブを閉じる (= 最後なら窓も閉じる)。
    btn.addEventListener("click", () => active()?.closeByUser());
    card.appendChild(btn);
  }
  statusEl.replaceChildren(card);
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
    renderStatusOverlay(s.status, s.statusText, s.statusShowClose);
  },
  toast: (s, text, kind?: ToastKind) => {
    if (activeId === s.id) showToast(text, kind);
  },
  updateStats: (s, ev: StatsEvent) => {
    if (activeId === s.id) updateStats(ev);
  },
  clearStats: (s) => {
    if (activeId === s.id) clearStats();
  },
  fileResult: (s, id, ok, message) => {
    if (activeId === s.id) handleFileResult(id, ok, message);
  },
  refreshTab: (s) => tabs.update(s.id, s.tabLabel, s.tabStatus),
  removed: (s) => removeSession(s),
  setResizable: (on) => void setResizable(on),
};

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
  onSetVideoParams: (fps, mbps) => active()?.setVideoParams(fps, mbps),
  onSetResolution: (maxHeight) => active()?.setResolution(maxHeight),
});

// --- タブストリップ ---
const tabs: TabsHandle = mountTabs(tabbar, {
  onSelect: (id) => setActive(id),
  onClose: (id) => sessions.get(id)?.closeByUser(),
  onAdd: () => openAddTab(),
  onDetach: (id, sx, sy) => void splitTab(id, sx, sy),
});

// --- タブをウィンドウ外へドラッグ: 別ウィンドウへ分割 (networking 無停止で移譲) ---
async function splitTab(id: string, screenX: number, screenY: number): Promise<void> {
  const s = sessions.get(id);
  // 接続確立済みのタブだけ移譲する (接続中/エラー中は表示状態が定まらない)。
  if (!s || !s.isConnected) return;
  try {
    await openSessionWindow(id, JSON.stringify(s.snapshot()), screenX, screenY);
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
  const s = new Session(id, entry, save, host);
  sessions.set(id, s);
  tabs.add(id, s.tabLabel, "connecting");
  if (activateNew) setActive(id);
  await s.start();
}

// --- ローカル UI ホットキー ---
installWebviewGuard((hotkey) => {
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
  }
});

// --- 窓リサイズ → アクティブ Session のビューポート追従 ---
window.addEventListener("resize", () => active()?.onWindowResize());

// --- キャプチャ画面へのファイル D&D アップロード (client → host、アクティブ Session 宛) ---
let dropHint: HTMLElement | null = null;
function showDropHint(on: boolean): void {
  if (!dropHint) {
    dropHint = document.createElement("div");
    dropHint.id = "drop-hint";
    dropHint.textContent = "ドロップしてホストへ転送";
    Object.assign(dropHint.style, {
      position: "fixed",
      inset: "0",
      display: "none",
      placeItems: "center",
      font: "600 18px system-ui, sans-serif",
      color: "#fff",
      background: "rgba(18,20,26,0.55)",
      border: "3px dashed rgba(255,255,255,0.65)",
      zIndex: "9999",
      pointerEvents: "none",
    });
    document.body.appendChild(dropHint);
  }
  dropHint.style.display = on ? "grid" : "none";
}

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
async function adoptInitial(id: string): Promise<void> {
  let snap: SessionSnapshot;
  try {
    const json = await takeHandoff(id);
    if (!json) throw new Error("引き継ぎデータがありません");
    snap = JSON.parse(json) as SessionSnapshot;
  } catch (e) {
    renderStatusOverlay("error", `タブの引き継ぎに失敗しました: ${String(e)}`, true);
    return;
  }
  const s = new Session(id, snap.entry, snap.save, host);
  try {
    await s.adoptFrom(snap);
  } catch (e) {
    renderStatusOverlay("error", `アタッチに失敗しました: ${String(e)}`, true);
    return;
  }
  sessions.set(id, s);
  tabs.add(id, s.tabLabel, "connected");
  setActive(id); // activate(): 映像再開 (unpause) + keyframe + 入力 attach + 共有 UI 同期
  void setResizable(true);
  if (snap.captured) s.setCaptureMode(true);
  if (getSettings().show_stats && !statsHud.classList.contains("visible")) {
    toggleStats();
    toolbar.setStatsToggled(true);
  }
}

// --- 起動: 設定を読み、URL に応じて「最初のタブを接続」or「移譲タブを引き受け」 ---
const params = new URLSearchParams(window.location.search);
const adoptId = params.get("adopt");

void (async () => {
  try {
    settings = await loadSettings();
  } catch {
    settings = null; // getSettings() の既定にフォールバック
  }
  if (adoptId) {
    await adoptInitial(adoptId);
    return;
  }
  const initialId = params.get("session") ?? "";
  const initialEntry: HostEntry = {
    host: params.get("host") ?? "",
    port: Number(params.get("port")) || 0,
    label: params.get("host") ?? "",
  };
  const initialSave = params.get("save") === "1";
  if (!initialId || !initialEntry.host || !initialEntry.port) {
    renderStatusOverlay("error", "接続情報が不正です", true);
    return;
  }
  renderStatusOverlay("connecting", `connecting to ${initialEntry.host}…`, false);
  try {
    await addSession(initialId, initialEntry, initialSave, /*activate=*/ true);
    // 接続時の自動フルスクリーン (窓単位、最初のタブのみで判断)。
    if (getSettings().default_fullscreen && !isFullscreen()) void doToggleFullscreen();
    if (getSettings().show_stats && !statsHud.classList.contains("visible")) {
      toggleStats();
      toolbar.setStatsToggled(true);
    }
  } catch (e) {
    renderStatusOverlay("error", `接続に失敗しました: ${String(e)}`, true);
  }
})();
