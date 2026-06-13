import { Channel } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import "./styles/base.css";
import {
  connectToHost,
  disconnect,
  saveRecentHost,
  selectDisplay,
  setVideoParams,
  uploadFileFromPath,
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
  onDisconnect: () => doDisconnect("disconnected", true),
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

/** セッションを開く下回り。チャネルを張って connect_to_host を呼ぶ (再接続も共用)。 */
// セッション世代。openSession ごとに進め、旧セッションのチャネルに残っていたイベント
// (遅れて届く Disconnected 等) が新しい接続の状態を壊さないようにする。
let sessionGen = 0;

async function openSession(
  entry: HostEntry,
  password: string | null,
  save: boolean,
): Promise<void> {
  const gen = ++sessionGen;
  lastHost = entry.host;
  pendingEntry = entry;
  disconnectHandled = false;
  resetDecoder();
  const eventChannel = new Channel<ClientEvent>();
  const videoChannel = new Channel<ArrayBuffer>();
  eventChannel.onmessage = (ev) => handleClientEvent(ev, gen);
  videoChannel.onmessage = (data) => {
    if (gen !== sessionGen) return; // 旧セッションの残映像は捨てる
    // Tauri は ArrayBuffer / number[] のどちらかで来る場合がある。
    const buf = data instanceof ArrayBuffer ? data : new Uint8Array(data as unknown as number[]).buffer;
    handleVideoChunk(buf);
  };
  await connectToHost(entry.host, entry.port, password, save, eventChannel, videoChannel);
}

const launcher = mountLauncher({
  root: overlay,
  onConnect: async (entry, password, save) => {
    // 手動接続は進行中の自動再接続を打ち切る。
    cancelReconnect();
    await openSession(entry, password, save);
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

// --- 自動再接続 ---
// 通信断 (サーバ起因の切断) のとき、保存済みパスワード (DPAPI) で間隔を空けて
// 接続し直す。ハンドシェイク成功時に password は必ず保存されるため、再接続は
// password=null (Rust が復号) で行える。手動接続・接続成立で打ち切る。
const RECONNECT_DELAYS_MS = [1000, 2000, 4000, 8000, 15000];
const MAX_RECONNECT_ATTEMPTS = 10;
// 接続成立 (host_info) からこの時間生存して初めて「安定」とみなし、再接続カウンタを 0 に
// 戻す。host_info は Noise ハンドシェイク直後・映像確立前に届くため、ここで即リセットすると、
// 直後に落ちる接続 (例: 単一接続ホストへの再接続が重なり host が古い socket を掴んで切断) が
// 毎回「成功」扱いになりバックオフが伸びず 1 秒間隔のフラップが永久に続く。生存時間で判定して
// すぐ落ちるフラップはバックオフを伸ばし、最大試行で打ち切る。
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

/** 接続成立時 (host_info)。再接続ループは止めるが、カウンタは安定後に初めてリセットする。 */
function markSessionEstablished(): void {
  reconnecting = false;
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  // すぐ落ちるフラップではバックオフを伸ばすため、一定時間生存して初めて attempt をリセット。
  clearStabilizeTimer();
  stabilizeTimer = window.setTimeout(() => {
    stabilizeTimer = null;
    reconnectAttempt = 0;
  }, RECONNECT_STABLE_MS);
}

function startReconnect(): void {
  if (!pendingEntry) return;
  // attempt カウンタはリセットしない (安定接続後の stabilize か手動接続の cancelReconnect でのみ
  // 0 に戻る)。これによりフラップ時はバックオフが累積する。
  if (reconnecting && reconnectTimer !== null) {
    // 次の試行が予約済みならそれに任せる。
    return;
  }
  reconnecting = true;
  scheduleReconnect();
}

function scheduleReconnect(): void {
  // doDisconnect 経由と attemptReconnect の catch の両方から来ても timer を重複させない。
  if (reconnectTimer !== null) clearTimeout(reconnectTimer);
  const delay = RECONNECT_DELAYS_MS[Math.min(reconnectAttempt, RECONNECT_DELAYS_MS.length - 1)];
  launcher.setConnState(`再接続待機 ${Math.round(delay / 1000)}s… (試行 ${reconnectAttempt + 1})`, "connecting");
  reconnectTimer = window.setTimeout(() => void attemptReconnect(), delay);
}

async function attemptReconnect(): Promise<void> {
  reconnectTimer = null;
  if (!reconnecting || !pendingEntry) return;
  // 上限到達で打ち切る。openSession が成功 (host_info) しても直後に落ちるフラップでは catch を
  // 通らないため、ここで毎試行チェックして無限フラップを止める。
  if (reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
    cancelReconnect();
    launcher.setConnState("再接続に失敗しました", "error");
    showToast("再接続に失敗しました", "error");
    return;
  }
  reconnectAttempt++;
  launcher.setConnState(`再接続中… (試行 ${reconnectAttempt})`, "connecting");
  showToast(`再接続を試行中… (${reconnectAttempt})`);
  try {
    // password=null で保存済み (DPAPI) を使う。再保存不要なので save=false。
    // 成功確定は host_info イベントで markSessionEstablished (安定後にカウンタリセット)。
    await openSession(pendingEntry, null, false);
  } catch {
    if (!reconnecting) return; // 途中で手動接続等にキャンセルされた
    scheduleReconnect();
  }
}

// セッションごとの切断処理の実行済みフラグ。サーバ起因の切断では reader/writer の
// 両タスクが Disconnected を送るため二重に呼ばれる。`connected` での判定だと、接続確立前
// (host_info 受信前) にセッションが死んだとき切断が握りつぶされ UI が固まる/再接続が
// 走らないため、openSession でリセットする専用フラグで重複だけを抑止する。
let disconnectHandled = true;

/** 切断処理 (ユーザー操作・ホスト切断の両方から呼ぶ)。 */
function doDisconnect(reason: string, userInitiated = false): void {
  if (disconnectHandled) return;
  disconnectHandled = true;
  // 安定判定タイマを止める (このセッションは終わったのでカウンタを 0 に戻させない)。
  clearStabilizeTimer();
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
  // ユーザー操作でない通信断なら、設定に応じて自動再接続を開始する。
  if (!userInitiated && errored && pendingEntry && launcher.getSettings().auto_reconnect) {
    startReconnect();
  }
}

function handleClientEvent(ev: ClientEvent, gen: number): void {
  if (gen !== sessionGen) return; // 旧セッションの残イベントは無視
  switch (ev.kind) {
    case "connected":
      launcher.setConnState("handshake complete…", "connecting");
      break;
    case "host_info": {
      // 接続成立。再接続ループは止めるが、カウンタは一定時間生存後にリセットする
      // (host_info は映像確立前に届くため、即リセットするとフラップが減衰しない)。
      markSessionEstablished();
      // ランチャーを隠してツールバー有効化。
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

// --- キャプチャ画面へのファイル D&D アップロード (client → host) ---
// 接続中のみ受け付ける。Tauri ネイティブの drag-drop はファイルの「パス」を返すので、
// Rust 側 (upload_file_from_path) がそのファイルを読み、既存アップロードプロトコルで
// host へ送る (バイト列を WebView に載せない)。完了/失敗は file_result のトーストで出る。
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

// アップロード転送 id。同一ミリ秒の複数ファイルで衝突しないよう、時刻シードの連番にする。
let nextUploadId = Date.now() >>> 0;

async function uploadDroppedPaths(paths: string[]): Promise<void> {
  // 複数ファイルは host が 1 転送ずつ処理するため逐次アップロードする
  // (同時に流すと file チャネル上でチャンクが交錯し offset 不一致になる)。
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
  // 未接続 (ランチャー表示中) はファイルドロップを無視する。
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
