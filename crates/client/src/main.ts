import { Channel } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import "./styles/base.css";
import {
  connectToHost,
  disconnect,
  saveRecentHost,
  selectDisplay,
  setVideoParams,
  setAudioMuted,
  setResolution,
  setClientViewport,
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

// ファイル転送ポップオーバー。
mountFileTransfer(filePanel);

// 未接続のランチャー (サイドバー + ホスト一覧 / 設定)。
let lastHost = "";
let pendingEntry: HostEntry | null = null;
// 接続中か (フルスクリーン切替後の resizable 再適用判定に使う)。
let connected = false;

// --- ホスト解像度の自動追従 (RDP 風) ---
// クライアントの映像表示領域 (物理 px) をホストへ送り、ホストが最も近い対応モードへ
// 解像度を変える。これによりウィンドウに合わせて黒帯が消え、マウスも 1:1 に近づく。
let currentDisplays: DisplayDto[] = [];
let currentDisplay: DisplayDto | null = null;
let viewportTimer: number | null = null;
// 直近で送った viewport。微小な変化での連投を避ける。切断でリセットする。
let lastSentVw = 0;
let lastSentVh = 0;

/** 仮想ディスプレイに合わせる目標解像度 = クライアントのモニタ解像度 (物理 px)。
 *  ウィンドウのサイズではなく画面全体に固定する: host の仮想ディスプレイをクライアントの
 *  モニタ実解像度 (例 2560x1440) にし、窓をどうリサイズしても解像度は変わらない
 *  (窓内の表示差は canvas の object-fit:contain が吸収する)。接続直後に確定できるので、
 *  テイクオーバーの「初回 viewport 一発で確定」とも噛み合う (窓の最終サイズを待たない)。 */
function currentViewportPx(): { w: number; h: number } {
  const dpr = window.devicePixelRatio || 1;
  return {
    w: Math.max(1, Math.round(window.screen.width * dpr)),
    h: Math.max(1, Math.round(window.screen.height * dpr)),
  };
}

/** 現在の viewport をホストへ送る (接続中のみ)。`force` で前回値に関わらず送る。 */
function sendViewportNow(force = false): void {
  if (!connected) return;
  // 解像度追従は**仮想ディスプレイのときだけ**行う (テイクオーバーした仮想をクライアント
  // 表示領域に動的に合わせる)。物理キャプチャ時はホスト解像度を一切変えない。
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

// ウィンドウのリサイズに追従してホスト解像度を更新する (デバウンス)。
window.addEventListener("resize", () => {
  if (connected) scheduleSendViewport();
});

/** セッションを開く下回り。チャネルを張って connect_to_host を呼ぶ (再接続も共用)。 */
// セッション世代。openSession ごとに進め、旧セッションのチャネルに残っていたイベント
// (遅れて届く Disconnected 等) が新しい接続の状態を壊さないようにする。
let sessionGen = 0;
// 接続処理が in-flight か。手動接続と自動再接続が重なって connect_to_host を並行に走らせると、
// 単一接続ホストに TCP が重なり、host が古い接続を 10054 で切ってフラップになる。1 本だけに絞る。
let connectInFlight = false;

async function openSession(
  entry: HostEntry,
  password: string | null,
  save: boolean,
): Promise<void> {
  // 既に接続処理中なら重複起動しない (進行中の 1 本に任せる)。
  if (connectInFlight) return;
  connectInFlight = true;
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
  try {
    await connectToHost(entry.host, entry.port, password, save, eventChannel, videoChannel);
  } finally {
    connectInFlight = false;
  }
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

/** ツールバーのディスプレイ切替に出す一覧。
 *  - 仮想ディスプレイを使う (RDP) 設定が ON: 仮想のみを返す (= 物理へ切替させない)。
 *  - OFF: 物理のみを返す (= 仮想を選ばせない。仮想は RDP 設定専用)。 */
function toolbarDisplayList(): DisplayDto[] {
  if (launcher.getSettings().use_virtual_display) {
    const v = currentDisplays.find((d) => d.is_virtual);
    return v ? [v] : [];
  }
  return currentDisplays.filter((d) => !d.is_virtual);
}

function switchDisplay(d: DisplayDto): void {
  // 解像度/SPS が変わるためデコーダを作り直し、入力スケールを新ディスプレイへ更新。
  void selectDisplay(d.id);
  currentDisplay = d;
  resetDecoder();
  attachInput(canvas, { width: d.width, height: d.height }, { onCaptureChange });
  canvas.focus();
  const dn = formatDisplayName(d.name);
  toolbar.setConnLabel(`${lastHost} · ${dn} ${d.width}x${d.height}`);
  showToast(`switching to ${dn} ${d.width}x${d.height}`);
  // 切替後の新ディスプレイもクライアント表示領域へ追従させる (前回値に関わらず送る)。
  sendViewportNow(true);
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
  // viewport 追従状態をリセット (次接続/再接続で初回 viewport を必ず送る)。
  if (viewportTimer !== null) {
    clearTimeout(viewportTimer);
    viewportTimer = null;
  }
  lastSentVw = 0;
  lastSentVh = 0;
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
      // ホストが音声対応のときだけミュートボタンを出す (毎接続でミュート解除から)。
      toolbar.setAudioAvailable(ev.audio);
      connected = true;
      // 接続中はウィンドウのリサイズを許可 (未接続のランチャーは固定サイズ)。
      void setResizable(true);
      const settings = launcher.getSettings();
      currentDisplays = ev.displays;
      // 「仮想ディスプレイを使う」設定なら、ホストの仮想ディスプレイ (is_virtual) を既定の
      // キャプチャ対象にする (RDP 風)。無ければ物理の先頭へフォールバックして通知する。
      let target = ev.displays[0];
      if (settings.use_virtual_display) {
        const vd = ev.displays.find((dd) => dd.is_virtual);
        if (vd) {
          target = vd;
        } else {
          showToast("仮想ディスプレイが見つかりません (ホストに VDD 未導入?)", "error");
        }
      }
      // [一時診断] 接続時の仮想ディスプレイ判定を可視化する (select_display が飛ばない原因の切り分け)。
      showToast(
        `診断 設定仮想=${settings.use_virtual_display} / 検出=${
          ev.displays.filter((d) => d.is_virtual).map((d) => formatDisplayName(d.name)).join(",") ||
          "なし"
        } / 選択=${target ? formatDisplayName(target.name) : "なし"}`,
      );
      // currentDisplay を先に設定してから toolbarDisplayList() を呼ぶ。
      // 仮想ディスプレイ使用時は仮想のみをツールバーに表示し (物理はデタッチされるため)
      // 切替 UI を非表示にする。
      currentDisplay = target ?? null;
      toolbar.setDisplays(toolbarDisplayList(), target?.id);
      if (target) {
        // 既定の先頭以外 (= 仮想) を選ぶ場合はホストへキャプチャ対象切替を要求する。
        if (target.id !== ev.displays[0]?.id) void selectDisplay(target.id);
        toolbar.setConnLabel(
          `${lastHost} · ${formatDisplayName(target.name)} ${target.width}x${target.height}`,
        );
        attachInput(canvas, { width: target.width, height: target.height }, { onCaptureChange });
        canvas.focus();
        // 接続直後に仮想ディスプレイをクライアントのモニタ解像度へ合わせる (RDP 風)。
        // 物理モードでは sendViewportNow が早期 return する (解像度を変えない)。
        sendViewportNow(true);
      } else {
        toolbar.setConnLabel(`${lastHost} · (no displays?)`);
      }
      // 設定を反映: 統計 HUD / マウスキャプチャ / フルスクリーン。
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
      // 解像度はフルで開始 (ホスト capture スレッドは前セッションの設定を保持しうるため明示)。
      void setResolution(0);
      showToast("connected", "success");
      break;
    }
    case "display_resized": {
      // ホストが解像度を変更した (RDP 風の追従)。入力スケールとデコーダを新解像度へ更新する。
      const idx = currentDisplays.findIndex((dd) => dd.id === ev.id);
      if (idx >= 0) {
        currentDisplays[idx] = { ...currentDisplays[idx], width: ev.width, height: ev.height };
      }
      if (currentDisplay && currentDisplay.id === ev.id) {
        currentDisplay = { ...currentDisplay, width: ev.width, height: ev.height };
      }
      resetDecoder(); // 新 SPS (解像度/レベル変化) で作り直すため未設定に戻す
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
