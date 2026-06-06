import { Channel } from "@tauri-apps/api/core";
import "./styles/base.css";
import {
  connectToHost,
  disconnect,
  saveRecentHost,
  selectDisplay,
  setVideoParams,
  formatDisplayName,
  type ClientEvent,
  type DisplayDto,
  type HostEntry,
} from "./core/ipc";
import { handleVideoChunk, initDecoder, resetDecoder } from "./core/decoder";
import { attachInput, detachInput, isCaptured, setCaptureMode } from "./core/input";
import { mountLauncher } from "./ui/launcher";
import { mountToolbar } from "./ui/toolbar";
import { initToast, showToast } from "./ui/toast";
import { clearStats, initStatsOverlay, toggleStats, updateStats } from "./ui/stats-overlay";
import { handleFileResult, mountFileTransfer } from "./ui/file-transfer";
import { installWebviewGuard } from "./core/webview-guard";
import { exitFullscreen, isFullscreen, toggleFullscreen } from "./core/fullscreen";
import { restoreLauncherWindow, setResizable } from "./ui/window-controls";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const overlay = document.getElementById("connect-overlay") as HTMLElement;
const topbar = document.getElementById("topbar") as HTMLElement;
const statsHud = document.getElementById("stats-hud") as HTMLElement;
const filePanel = document.getElementById("file-panel") as HTMLElement;
const toastContainer = document.getElementById("toast-container") as HTMLElement;

initDecoder(canvas);
initToast(toastContainer);
initStatsOverlay(statsHud);

// ローカル UI ホットキー (Ctrl+Alt+Shift+S/B/F/M, F11)。ブラウザ既定挙動も抑止する。
installWebviewGuard((hotkey) => {
  if (hotkey === "toggle-stats") {
    toggleStats();
    toolbar.setStatsToggled((statsHud.classList.contains("visible")));
  } else if (hotkey === "toggle-bar") {
    toolbar.toggleBar();
  } else if (hotkey === "toggle-fullscreen") {
    void doToggleFullscreen();
  } else if (hotkey === "toggle-capture") {
    // 実状態は onCaptureChange で確定し、トースト/トグル表示を更新する。
    setCaptureMode(!isCaptured());
  }
});

async function doToggleFullscreen(): Promise<void> {
  const on = await toggleFullscreen();
  toolbar.setFullscreenToggled(on);
  // フルスクリーンを抜けると Tauri がウィンドウを作成時状態 (resizable:false) に戻すため、
  // 接続中はリサイズ可を再適用する (Win11 の角丸=WS_THICKFRAME も復活する)。
  if (connected) void setResizable(true);
  showToast(on ? "fullscreen on" : "fullscreen off");
}

// input.ts のポインタロック状態変化をツールバー表示とトーストへ反映する。
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

// 接続中ツールバー。
const toolbar = mountToolbar({
  topbar,
  onDisconnect: () => doDisconnect("disconnected"),
  onSelectDisplay: (d) => switchDisplay(d),
  onToggleStats: () => {
    toggleStats();
    toolbar.setStatsToggled(statsHud.classList.contains("visible"));
  },
  onToggleFilePanel: () => setFilePanel(!fileVisible),
  onToggleFullscreen: () => void doToggleFullscreen(),
  onToggleCapture: () => setCaptureMode(!isCaptured()),
  onSetVideoParams: (fps, mbps) => void setVideoParams(fps, mbps * 1_000_000),
});

// ファイル転送ポップオーバー。
mountFileTransfer(filePanel);

// 未接続のランチャー (サイドバー + ホスト一覧 / 設定)。
let lastHost = "";
let pendingEntry: HostEntry | null = null;
// 接続中か (フルスクリーン切替後の resizable 再適用判定に使う)。
let connected = false;
const launcher = mountLauncher({
  root: overlay,
  onConnect: async (entry, password) => {
    lastHost = entry.host;
    pendingEntry = entry;
    resetDecoder();
    const eventChannel = new Channel<ClientEvent>();
    const videoChannel = new Channel<ArrayBuffer>();
    eventChannel.onmessage = handleClientEvent;
    videoChannel.onmessage = (data) => {
      // Tauri は ArrayBuffer / number[] のどちらかで来る場合がある。
      const buf = data instanceof ArrayBuffer ? data : new Uint8Array(data as unknown as number[]).buffer;
      handleVideoChunk(buf);
    };
    await connectToHost(entry.host, entry.port, password, eventChannel, videoChannel);
  },
});

launcher.show();

function switchDisplay(d: DisplayDto): void {
  // 解像度/SPS が変わるためデコーダを作り直し、入力スケールを新ディスプレイへ更新。
  void selectDisplay(d.id);
  resetDecoder();
  attachInput(canvas, { width: d.width, height: d.height }, { onCaptureChange });
  canvas.focus();
  const dn = formatDisplayName(d.name);
  toolbar.setConnLabel(`${lastHost} · ${dn} ${d.width}x${d.height}`);
  showToast(`switching to ${dn} ${d.width}x${d.height}`);
}

/** 切断処理 (ユーザー操作・ホスト切断の両方から呼ぶ)。 */
function doDisconnect(reason: string): void {
  // サーバ起因の切断では reader/writer の両タスクが Disconnected を送るため二重に呼ばれる。
  // 既に未接続なら何もしない (トーストやランチャー復帰の重複を防ぐ)。
  if (!connected) return;
  void disconnect();
  connected = false;
  detachInput(); // ポインタロックも解除される
  toolbar.setConnected(false);
  toolbar.setCaptureToggled(false);
  // ランチャーへ戻るので、接続中に変えたサイズを固定サイズへ戻しリサイズも無効化。
  void restoreLauncherWindow();
  setFilePanel(false);
  // フルスクリーンを解除して接続カードを操作できるようにする。
  void exitFullscreen().then(() => toolbar.setFullscreenToggled(isFullscreen()));
  clearStats();
  const errored = reason.startsWith("disconnected:");
  launcher.setConnState(errored ? reason : "未接続", errored ? "error" : "idle");
  launcher.show();
  void launcher.refreshHosts();
  showToast(reason, errored ? "error" : "info");
}

function handleClientEvent(ev: ClientEvent): void {
  switch (ev.kind) {
    case "connected":
      launcher.setConnState("handshake complete…", "connecting");
      break;
    case "host_info": {
      // 接続成立。ランチャーを隠してツールバー有効化。
      launcher.hide();
      launcher.setConnState(`connected · ${lastHost}`, "connected");
      if (pendingEntry) void saveRecentHost(pendingEntry);
      toolbar.setConnected(true);
      connected = true;
      // 接続中はウィンドウのリサイズを許可 (未接続のランチャーは固定サイズ)。
      void setResizable(true);
      toolbar.setDisplays(ev.displays, ev.displays[0]?.id);
      const d = ev.displays[0];
      if (d) {
        toolbar.setConnLabel(`${lastHost} · ${formatDisplayName(d.name)} ${d.width}x${d.height}`);
        attachInput(canvas, { width: d.width, height: d.height }, { onCaptureChange });
        canvas.focus();
      } else {
        toolbar.setConnLabel(`${lastHost} · (no displays?)`);
      }
      // 設定を反映: 統計 HUD / マウスキャプチャ / フルスクリーン。
      const settings = launcher.getSettings();
      if (settings.show_stats && !statsHud.classList.contains("visible")) {
        toggleStats();
        toolbar.setStatsToggled(true);
      }
      if (settings.default_capture) {
        // ポインタロックはユーザー操作直後でないと拒否されることがある (その場合はトーストで通知)。
        setCaptureMode(true);
      }
      if (settings.default_fullscreen && !isFullscreen()) {
        void doToggleFullscreen();
      }
      // 映像パラメータを設定値でホストへ適用し、ツールバー表示も同期。
      toolbar.setVideoParams(settings.fps, settings.max_mbps);
      void setVideoParams(settings.fps, settings.max_mbps * 1_000_000);
      showToast("connected", "success");
      break;
    }
    case "stats":
      updateStats(ev);
      break;
    case "pong":
      // RTT は stats 側で表示する。
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
