// capture ウィンドウ (1 セッション = 1 ホスト) のエントリ。
// URL クエリ (?session=<id>&host=...&port=...&save=0|1) から接続情報を読み、自ウィンドウの
// セッションを開く。接続資格情報 (password) は Rust 側 staging が持つためここには載らない。
// launcher は別ウィンドウなので、切断はこのウィンドウを閉じる/エラー表示に帰着する。

import { Channel } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import "./styles/base.css";
import {
  connectToHost,
  disconnect,
  loadSettings,
  saveRecentHost,
  selectDisplay,
  setVideoParams,
  setAudioMuted,
  setResolution,
  setClientViewport,
  uploadFileFromPath,
  formatDisplayName,
  type ClientEvent,
  type ClientSettings,
  type DisplayDto,
  type HostEntry,
} from "./core/ipc";
import { handleVideoChunk, initDecoder, resetDecoder } from "./core/decoder";
import { attachInput, detachInput, isCaptured, setCaptureMode } from "./core/input";
import { mountToolbar } from "./ui/toolbar";
import { initToast, showToast } from "./ui/toast";
import { clearStats, initStatsOverlay, toggleStats, updateStats } from "./ui/stats-overlay";
import { handleFileResult, mountFileTransfer } from "./ui/file-transfer";
import { installWebviewGuard } from "./core/webview-guard";
import { exitFullscreen, isFullscreen, toggleFullscreen } from "./core/fullscreen";
import { closeWindow, setResizable } from "./ui/window-controls";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const topbar = document.getElementById("topbar") as HTMLElement;
const statsHud = document.getElementById("stats-hud") as HTMLElement;
const filePanel = document.getElementById("file-panel") as HTMLElement;
const toastContainer = document.getElementById("toast-container") as HTMLElement;
const statusEl = document.getElementById("conn-status") as HTMLElement;

initDecoder(canvas);
initToast(toastContainer);
initStatsOverlay(statsHud);

// --- URL から接続情報を読む ---
const params = new URLSearchParams(window.location.search);
const sessionId = params.get("session") ?? "";
const entry: HostEntry = {
  host: params.get("host") ?? "",
  port: Number(params.get("port")) || 0,
  label: params.get("host") ?? "",
};
const initialSave = params.get("save") === "1";
const lastHost = entry.host;

// 接続中はランチャーから引き継いだ設定を IPC で読む (capture には設定 UI がない)。
let settings: ClientSettings | null = null;
function getSettings(): ClientSettings {
  // host_info より前に設定読み込みは完了している (起動時に await 済み)。万一未読込でも
  // 安全側の既定 (仮想無効・自動再接続有効) を返す。
  return (
    settings ?? {
      default_fullscreen: false,
      show_stats: false,
      default_capture: false,
      default_port: entry.port,
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

/** 接続前/切断後の中央オーバーレイ。connected で hidden。reason は host 由来を含みうるので textContent。 */
function setStatus(text: string, kind: "connecting" | "error" | "hidden", showClose = false): void {
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
    btn.addEventListener("click", () => void closeWindow());
    card.appendChild(btn);
  }
  statusEl.replaceChildren(card);
}

// ローカル UI ホットキー (Ctrl+Alt+Shift+S/B/F/M, F11)。ブラウザ既定挙動も抑止する。
installWebviewGuard((hotkey) => {
  if (hotkey === "toggle-stats") {
    toggleStats();
    toolbar.setStatsToggled(statsHud.classList.contains("visible"));
  } else if (hotkey === "toggle-bar") {
    toolbar.toggleBar();
  } else if (hotkey === "toggle-fullscreen") {
    void doToggleFullscreen();
  } else if (hotkey === "toggle-capture") {
    setCaptureMode(!isCaptured());
  }
});

async function doToggleFullscreen(): Promise<void> {
  const on = await toggleFullscreen();
  toolbar.setFullscreenToggled(on);
  // フルスクリーンを抜けると Tauri がウィンドウを作成時状態へ戻すため、接続中はリサイズ可を再適用。
  if (connected) void setResizable(true);
  showToast(on ? "fullscreen on" : "fullscreen off");
}

function onCaptureChange(captured: boolean): void {
  toolbar.setCaptureToggled(captured);
  showToast(captured ? "mouse captured (Esc to release)" : "mouse released");
}

let fileVisible = false;
function setFilePanel(visible: boolean): void {
  fileVisible = visible;
  filePanel.classList.toggle("visible", visible);
  toolbar.setFileToggled(visible);
}

// 接続中ツールバー。切断ボタンはこのウィンドウを閉じる (= その接続だけ切れる)。
const toolbar = mountToolbar({
  topbar,
  onDisconnect: () => doDisconnect("disconnected", true),
  onSelectDisplay: (d) => switchDisplay(d),
  onToggleStats: () => {
    toggleStats();
    toolbar.setStatsToggled(statsHud.classList.contains("visible"));
  },
  onToggleFilePanel: () => setFilePanel(!fileVisible),
  onToggleFullscreen: () => void doToggleFullscreen(),
  onToggleCapture: () => setCaptureMode(!isCaptured()),
  onToggleMute: (muted) => {
    void setAudioMuted(muted);
    showToast(muted ? "音声ミュート" : "音声ミュート解除");
  },
  onSetVideoParams: (fps, mbps) => void setVideoParams(fps, mbps * 1_000_000),
  onSetResolution: (maxHeight) => {
    void setResolution(maxHeight);
    showToast(maxHeight === 0 ? "解像度: フル" : `解像度: ${maxHeight}p`);
  },
});

mountFileTransfer(filePanel);

// 接続中か (フルスクリーン切替後の resizable 再適用判定に使う)。
let connected = false;

// --- ホスト解像度の自動追従 (RDP 風) ---
let currentDisplays: DisplayDto[] = [];
let currentDisplay: DisplayDto | null = null;
let viewportTimer: number | null = null;
let lastSentVw = 0;
let lastSentVh = 0;

function currentViewportPx(): { w: number; h: number } {
  const dpr = window.devicePixelRatio || 1;
  return {
    w: Math.max(1, Math.round(window.screen.width * dpr)),
    h: Math.max(1, Math.round(window.screen.height * dpr)),
  };
}

function sendViewportNow(force = false): void {
  if (!connected) return;
  const useVirtual = currentDisplay?.is_virtual ?? false;
  if (!useVirtual) return;
  const { w, h } = currentViewportPx();
  if (!force && Math.abs(w - lastSentVw) < 8 && Math.abs(h - lastSentVh) < 8) return;
  lastSentVw = w;
  lastSentVh = h;
  void setClientViewport(w, h);
}

function scheduleSendViewport(): void {
  if (viewportTimer !== null) clearTimeout(viewportTimer);
  viewportTimer = window.setTimeout(() => {
    viewportTimer = null;
    sendViewportNow();
  }, 400);
}

window.addEventListener("resize", () => {
  if (connected) scheduleSendViewport();
});

// セッション世代。openSession ごとに進め、遅れて届く旧チャネルのイベントを無視する。
let sessionGen = 0;
let connectInFlight = false;

async function openSession(save: boolean): Promise<void> {
  if (connectInFlight) return;
  connectInFlight = true;
  const gen = ++sessionGen;
  disconnectHandled = false;
  resetDecoder();
  const eventChannel = new Channel<ClientEvent>();
  const videoChannel = new Channel<ArrayBuffer>();
  eventChannel.onmessage = (ev) => handleClientEvent(ev, gen);
  videoChannel.onmessage = (data) => {
    if (gen !== sessionGen) return; // 旧セッションの残映像は捨てる
    const buf =
      data instanceof ArrayBuffer ? data : new Uint8Array(data as unknown as number[]).buffer;
    handleVideoChunk(buf);
  };
  try {
    await connectToHost(sessionId, entry.host, entry.port, save, eventChannel, videoChannel);
  } finally {
    connectInFlight = false;
  }
}

function toolbarDisplayList(): DisplayDto[] {
  if (getSettings().use_virtual_display) {
    const v = currentDisplays.find((d) => d.is_virtual);
    return v ? [v] : [];
  }
  return currentDisplays.filter((d) => !d.is_virtual);
}

function switchDisplay(d: DisplayDto): void {
  void selectDisplay(d.id);
  currentDisplay = d;
  resetDecoder();
  attachInput(canvas, { width: d.width, height: d.height }, { onCaptureChange });
  canvas.focus();
  const dn = formatDisplayName(d.name);
  toolbar.setConnLabel(`${lastHost} · ${dn} ${d.width}x${d.height}`);
  showToast(`switching to ${dn} ${d.width}x${d.height}`);
  sendViewportNow(true);
}

// --- 自動再接続 ---
const RECONNECT_DELAYS_MS = [1000, 2000, 4000, 8000, 15000];
const MAX_RECONNECT_ATTEMPTS = 10;
const RECONNECT_STABLE_MS = 6000;
let reconnecting = false;
let reconnectAttempt = 0;
let reconnectTimer: number | null = null;
let stabilizeTimer: number | null = null;

function clearStabilizeTimer(): void {
  if (stabilizeTimer !== null) {
    clearTimeout(stabilizeTimer);
    stabilizeTimer = null;
  }
}

function cancelReconnect(): void {
  reconnecting = false;
  reconnectAttempt = 0;
  clearStabilizeTimer();
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
}

function markSessionEstablished(): void {
  reconnecting = false;
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  clearStabilizeTimer();
  stabilizeTimer = window.setTimeout(() => {
    stabilizeTimer = null;
    reconnectAttempt = 0;
  }, RECONNECT_STABLE_MS);
}

function startReconnect(): void {
  if (reconnecting && reconnectTimer !== null) return;
  reconnecting = true;
  scheduleReconnect();
}

function scheduleReconnect(): void {
  if (reconnectTimer !== null) clearTimeout(reconnectTimer);
  const delay = RECONNECT_DELAYS_MS[Math.min(reconnectAttempt, RECONNECT_DELAYS_MS.length - 1)];
  setStatus(`再接続待機 ${Math.round(delay / 1000)}s… (試行 ${reconnectAttempt + 1})`, "connecting");
  reconnectTimer = window.setTimeout(() => void attemptReconnect(), delay);
}

async function attemptReconnect(): Promise<void> {
  reconnectTimer = null;
  if (!reconnecting) return;
  if (reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
    cancelReconnect();
    setStatus("再接続に失敗しました", "error", true);
    showToast("再接続に失敗しました", "error");
    return;
  }
  reconnectAttempt++;
  setStatus(`再接続中… (試行 ${reconnectAttempt})`, "connecting");
  showToast(`再接続を試行中… (${reconnectAttempt})`);
  try {
    // password=null で保存済み (DPAPI) を使う。再保存不要なので save=false。
    await openSession(false);
  } catch {
    if (!reconnecting) return;
    scheduleReconnect();
  }
}

// セッションごとの切断処理の実行済みフラグ (reader/writer 双方が Disconnected を送るため)。
let disconnectHandled = true;

/** 切断処理。userInitiated=true (ツールバー切断) はウィンドウを閉じる。 */
function doDisconnect(reason: string, userInitiated = false): void {
  if (disconnectHandled) return;
  disconnectHandled = true;
  clearStabilizeTimer();
  void disconnect();
  connected = false;
  if (viewportTimer !== null) {
    clearTimeout(viewportTimer);
    viewportTimer = null;
  }
  lastSentVw = 0;
  lastSentVh = 0;
  detachInput();
  toolbar.setConnected(false);
  toolbar.setCaptureToggled(false);
  setFilePanel(false);
  clearStats();

  // ユーザー操作の切断はこのウィンドウを閉じる (= その接続だけ切れる。他ウィンドウは生存)。
  if (userInitiated) {
    void closeWindow();
    return;
  }

  // ホスト起因の切断: フルスクリーンを解除して状態を見せる。
  void exitFullscreen().then(() => toolbar.setFullscreenToggled(isFullscreen()));
  const errored = reason.startsWith("disconnected:");
  showToast(reason, errored ? "error" : "info");
  if (errored && getSettings().auto_reconnect) {
    startReconnect();
  } else {
    setStatus(errored ? reason : "切断されました", "error", true);
  }
}

function handleClientEvent(ev: ClientEvent, gen: number): void {
  if (gen !== sessionGen) return; // 旧セッションの残イベントは無視
  switch (ev.kind) {
    case "connected":
      setStatus("handshake complete…", "connecting");
      break;
    case "host_info": {
      markSessionEstablished();
      setStatus("", "hidden");
      if (entry.host) void saveRecentHost(entry);
      const settings = getSettings();
      toolbar.setPosition(settings.toolbar_position);
      toolbar.setConnected(true, settings.toolbar_pinned);
      toolbar.setAudioAvailable(ev.audio);
      connected = true;
      void setResizable(true);
      currentDisplays = ev.displays;
      let target = ev.displays[0];
      if (settings.use_virtual_display) {
        const vd = ev.displays.find((dd) => dd.is_virtual);
        if (vd) {
          target = vd;
        } else {
          showToast("仮想ディスプレイが見つかりません (ホストに VDD 未導入?)", "error");
        }
      }
      currentDisplay = target ?? null;
      toolbar.setDisplays(toolbarDisplayList(), target?.id);
      if (target) {
        if (target.id !== ev.displays[0]?.id) void selectDisplay(target.id);
        toolbar.setConnLabel(
          `${lastHost} · ${formatDisplayName(target.name)} ${target.width}x${target.height}`,
        );
        attachInput(canvas, { width: target.width, height: target.height }, { onCaptureChange });
        canvas.focus();
        sendViewportNow(true);
      } else {
        toolbar.setConnLabel(`${lastHost} · (no displays?)`);
      }
      if (settings.show_stats && !statsHud.classList.contains("visible")) {
        toggleStats();
        toolbar.setStatsToggled(true);
      }
      if (settings.default_capture) {
        setCaptureMode(true);
      }
      if (settings.default_fullscreen && !isFullscreen()) {
        void doToggleFullscreen();
      }
      toolbar.setVideoParams(settings.fps, settings.max_mbps);
      void setVideoParams(settings.fps, settings.max_mbps * 1_000_000);
      void setResolution(0);
      showToast("connected", "success");
      break;
    }
    case "display_resized": {
      const idx = currentDisplays.findIndex((dd) => dd.id === ev.id);
      if (idx >= 0) {
        currentDisplays[idx] = { ...currentDisplays[idx], width: ev.width, height: ev.height };
      }
      if (currentDisplay && currentDisplay.id === ev.id) {
        currentDisplay = { ...currentDisplay, width: ev.width, height: ev.height };
      }
      resetDecoder();
      attachInput(canvas, { width: ev.width, height: ev.height }, { onCaptureChange });
      canvas.focus();
      toolbar.setDisplays(toolbarDisplayList(), ev.id);
      const dn = currentDisplay ? formatDisplayName(currentDisplay.name) : `Display ${ev.id}`;
      toolbar.setConnLabel(`${lastHost} · ${dn} ${ev.width}x${ev.height}`);
      break;
    }
    case "stats":
      updateStats(ev);
      break;
    case "pong":
      break;
    case "file_result":
      handleFileResult(ev.id, ev.ok, ev.message);
      showToast(
        ev.ok ? `file ok: ${ev.message}` : `file failed: ${ev.message}`,
        ev.ok ? "success" : "error",
      );
      break;
    case "disconnected":
      doDisconnect(`disconnected: ${ev.reason}`);
      break;
  }
}

// --- キャプチャ画面へのファイル D&D アップロード (client → host) ---
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
  if (!connected) {
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

// --- 起動: 設定を読んでから自分のセッションを開く ---
setStatus(`connecting to ${entry.host}…`, "connecting");
void (async () => {
  try {
    settings = await loadSettings();
  } catch {
    settings = null; // getSettings() の既定にフォールバック
  }
  if (!sessionId || !entry.host || !entry.port) {
    setStatus("接続情報が不正です", "error", true);
    return;
  }
  try {
    await openSession(initialSave);
  } catch (e) {
    setStatus(`接続に失敗しました: ${String(e)}`, "error", true);
  }
})();
