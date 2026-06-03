import { Channel } from "@tauri-apps/api/core";
import {
  connectToHost,
  disconnect,
  saveRecentHost,
  selectDisplay,
  type ClientEvent,
  type DisplayDto,
} from "./ipc";
import { handleVideoChunk, initDecoder, resetDecoder } from "./decoder";
import { attachInput, detachInput } from "./input";
import { mountConnectionUi } from "./connection-ui";
import { mountToolbar } from "./toolbar";
import { initToast, showToast } from "./toast";
import { clearStats, initStatsOverlay, toggleStats, updateStats } from "./stats-overlay";
import { handleFileResult, mountFileTransfer } from "./file-transfer";
import { installWebviewGuard } from "./webview-guard";

const canvas = document.getElementById("screen") as HTMLCanvasElement;
const overlay = document.getElementById("connect-overlay") as HTMLElement;
const topbar = document.getElementById("topbar") as HTMLElement;
const hotzone = document.getElementById("topbar-hotzone") as HTMLElement;
const statsHud = document.getElementById("stats-hud") as HTMLElement;
const filePanel = document.getElementById("file-panel") as HTMLElement;
const toastContainer = document.getElementById("toast-container") as HTMLElement;

initDecoder(canvas);
initToast(toastContainer);
initStatsOverlay(statsHud);

// ローカル UI ホットキー (Ctrl+Alt+Shift+S/B)。ブラウザ既定挙動も抑止する。
installWebviewGuard((hotkey) => {
  if (hotkey === "toggle-stats") {
    toggleStats();
    toolbar.setStatsToggled((statsHud.classList.contains("visible")));
  } else if (hotkey === "toggle-bar") {
    toolbar.toggleBar();
  }
});

let fileVisible = false;
function setFilePanel(visible: boolean): void {
  fileVisible = visible;
  filePanel.classList.toggle("visible", visible);
  toolbar.setFileToggled(visible);
}

// 接続中ツールバー。
const toolbar = mountToolbar({
  topbar,
  hotzone,
  onDisconnect: () => doDisconnect("disconnected"),
  onSelectDisplay: (d) => switchDisplay(d),
  onToggleStats: () => {
    toggleStats();
    toolbar.setStatsToggled(statsHud.classList.contains("visible"));
  },
  onToggleFilePanel: () => setFilePanel(!fileVisible),
});

// ファイル転送ポップオーバー。
mountFileTransfer(filePanel);

// 未接続の中央カード。
let lastHost = "";
const connectionUi = mountConnectionUi({
  root: overlay,
  onSubmit: async (host, password) => {
    lastHost = host;
    resetDecoder();
    const eventChannel = new Channel<ClientEvent>();
    const videoChannel = new Channel<ArrayBuffer>();
    eventChannel.onmessage = handleClientEvent;
    videoChannel.onmessage = (data) => {
      // Tauri は ArrayBuffer / number[] のどちらかで来る場合がある。
      const buf = data instanceof ArrayBuffer ? data : new Uint8Array(data as unknown as number[]).buffer;
      handleVideoChunk(buf);
    };
    await connectToHost(host, password, eventChannel, videoChannel);
  },
});

connectionUi.show();

function switchDisplay(d: DisplayDto): void {
  // 解像度/SPS が変わるためデコーダを作り直し、入力スケールを新ディスプレイへ更新。
  void selectDisplay(d.id);
  resetDecoder();
  attachInput(canvas, { width: d.width, height: d.height });
  canvas.focus();
  toolbar.setConnLabel(`${lastHost} · ${d.name} ${d.width}x${d.height}`);
  showToast(`switching to ${d.name} ${d.width}x${d.height}`);
}

/** 切断処理 (ユーザー操作・ホスト切断の両方から呼ぶ)。 */
function doDisconnect(reason: string): void {
  void disconnect();
  detachInput();
  toolbar.setConnected(false);
  setFilePanel(false);
  clearStats();
  connectionUi.setConnecting(false);
  connectionUi.setStatus("");
  connectionUi.show();
  void connectionUi.refreshHosts();
  showToast(reason, reason.startsWith("disconnected:") ? "error" : "info");
}

function handleClientEvent(ev: ClientEvent): void {
  switch (ev.kind) {
    case "connected":
      connectionUi.setStatus("handshake complete…");
      break;
    case "host_info": {
      // 接続成立。カードを隠して全画面化、ツールバー有効化。
      connectionUi.setConnecting(false);
      connectionUi.hide();
      void saveRecentHost(lastHost);
      toolbar.setConnected(true);
      toolbar.setDisplays(ev.displays, ev.displays[0]?.id);
      const d = ev.displays[0];
      if (d) {
        toolbar.setConnLabel(`${lastHost} · ${d.name} ${d.width}x${d.height}`);
        attachInput(canvas, { width: d.width, height: d.height });
        canvas.focus();
      } else {
        toolbar.setConnLabel(`${lastHost} · (no displays?)`);
      }
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
