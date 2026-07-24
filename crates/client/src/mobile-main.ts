// モバイル (Android) 単一 WebView のエントリ (orchestrator)。
// デスクトップ版 (`capture-main.ts` / `launcher-main.ts`) は「ランチャー窓」と「capture 窓」を
// 別ウィンドウとして持つが、モバイルはウィンドウを複数生成できない前提のため、1 つの WebView
// 内で launcher ビュー ⇄ remote ビューを DOM 表示切替で往復する。マルチタブも非対応で、
// 常に 1 セッションのみを保持する。
//
// `SessionHost` / `ToolbarHandle` は `core/session.ts` がデスクトップ UI 前提で定義した契約だが、
// Session 自体 (接続・再接続・映像・入力配線のロジック) はプラットフォーム非依存なのでそのまま
// 再利用する。ここではモバイルに存在しない概念 (タブ・ウィンドウリサイズ・ファイル転送・
// フルスクリーン制御等) をすべて no-op にしたアダプタを作って埋める。

import "./mobile-main.css";
import {
  DEFAULT_SETTINGS,
  initPerfFlag,
  loadSettings,
  saveSettings,
  sendInput,
  stageSession,
  type ClientSettings,
  type DisplayDto,
  type HostEntry,
} from "./core/ipc";
import { Session, type SessionHost, type StatsEvent } from "./core/session";
import type { ToolbarHandle } from "./ui/toolbar";
import { mountMobileLauncher, type MobileLauncherHandle } from "./ui/mobile-launcher";
import { mountMobileControls, type MobileControlsHandle } from "./ui/mobile-controls";
import { initToast, showToast, type ToastKind } from "./ui/toast";
import { clearStats, initStatsOverlay, isStatsVisible, toggleStats, updateStats } from "./ui/stats-overlay";
import { hideReconnectBadge, initReconnectBadge, showReconnectBadge } from "./ui/reconnect-badge";
import { initCaptureOverlay, renderStatusOverlay } from "./ui/capture-overlay";
import { installGlobalErrorLogging, logger } from "./core/log";
import { TouchInput } from "./core/touch-input";
import { SoftKeyboard } from "./core/soft-keyboard";

// 未捕捉例外/Promise reject を Rust ログ (client.log) へ転送する。
installGlobalErrorLogging();

const launcherView = document.getElementById("launcher-view") as HTMLElement;
const remoteView = document.getElementById("remote-view") as HTMLElement;
const canvas = document.getElementById("screen") as HTMLCanvasElement;
const statusEl = document.getElementById("conn-status") as HTMLElement;
const statsHud = document.getElementById("stats-hud") as HTMLElement;
const toastContainer = document.getElementById("toast-container") as HTMLElement;
const reconnectBadgeEl = document.getElementById("reconnect-badge") as HTMLElement;
const controlsRoot = document.getElementById("mobile-controls") as HTMLElement;
const keyboardRoot = document.getElementById("soft-keyboard-root") as HTMLElement;

initToast(toastContainer);
initStatsOverlay(statsHud);
initReconnectBadge(reconnectBadgeEl);
initCaptureOverlay(statusEl);

// --- 設定 (このシェルの唯一のセッションが参照する) ---
let settings: ClientSettings | null = null;
function getSettings(): ClientSettings {
  return settings ?? DEFAULT_SETTINGS;
}

// --- 単一セッション (モバイルはマルチタブ非対応) ---
let session: Session | null = null;

function showLauncherView(): void {
  remoteView.classList.add("hidden");
  launcher.show();
}
function showRemoteView(): void {
  launcher.hide();
  remoteView.classList.remove("hidden");
}

// --- タッチ入力: 座標変換の唯一の情報源はホストの現在ディスプレイ解像度 (toolbar.setDisplays 経由) ---
const touchInput = new TouchInput({
  sendInput: (ev) => {
    if (session) void sendInput(session.id, ev).catch((e) => logger.warn("sendInput failed", e));
  },
  onZoomChange: (scale) => controls.setZoom(scale),
});

// --- ソフトキーボード (修飾キー + 単発特殊キーのフロートパネル) ---
const softKeyboard = new SoftKeyboard({
  sendKey: (vk, down) => session?.sendGrabbedKey(vk, down),
  onConsumeModifiers: () => controls.clearModifiers(),
  getStickyModifiers: () => controls.stickyModifiers(),
});
softKeyboard.mount(keyboardRoot);

// --- ToolbarHandle アダプタ: desktop 専用 UI (タブ/ウィンドウ/ファイル転送/音声/クリップボード等)
// はモバイル対象外のため no-op にし、意味のある操作だけ mobile-controls / TouchInput へ橋渡しする ---
let currentDisplay: DisplayDto | null = null;
// 接続状態。TouchInput の attach は「接続済み」かつ「ディスプレイ解像度が判明」の両方が揃った
// 時点で行う必要があり、この 2 条件が別々のタイミングで満たされるためここで保持する
// (session.ts の onHostInfo は setConnected → setDisplays の順で呼ぶが、その呼び出し順に
// 依存せずに動くようにするため、両方の setter から同じ同期処理を呼ぶ)。
let toolbarConnected = false;

/** 接続済み・ディスプレイ判明の両方が揃ったときだけ TouchInput を attach する。 */
function syncTouchAttach(): void {
  if (toolbarConnected && currentDisplay) {
    touchInput.attach(canvas, { width: currentDisplay.width, height: currentDisplay.height });
  }
}

const toolbar: ToolbarHandle = {
  setConnLabel(text): void {
    controls.setConnLabel(text);
  },
  setDisplays(displays, selectedId): void {
    currentDisplay = displays.find((d) => d.id === selectedId) ?? displays[0] ?? null;
    // TouchInput の座標変換 (トラックパッド/直接タップいずれも) はディスプレイ解像度が唯一の情報源。
    if (currentDisplay) touchInput.setDisplay({ width: currentDisplay.width, height: currentDisplay.height });
    controls.setDisplays(displays, selectedId);
    syncTouchAttach();
  },
  setStatsToggled(on): void {
    controls.setStatsToggled(on);
  },
  // ファイル転送はモバイル対象外 (スコープ外)。
  setFileToggled: () => {},
  // ウィンドウが無く、全画面表示は WebView の既定表示そのものなので切替対象が無い。
  setFullscreenToggled: () => {},
  // マウスキャプチャ (ポインタロック) の概念が無く、タッチ操作は常時有効なため表示対象が無い。
  setCaptureToggled: () => {},
  // 音声 UI はスコープ外。
  setAudioAvailable: () => {},
  setAudioVolume: () => {},
  // クリップボード同期 UI はスコープ外。
  setClipboardSyncState: () => {},
  setVideoParams(fps, mbps): void {
    controls.setVideoParams(fps, mbps);
  },
  // モバイルの下部バーは常時表示のみで、デスクトップのような自動格納/ピン留めが無い。
  toggleBar: () => {},
  revealBar: () => {},
  // ツールバー配置 (上下左右) は画面端に沿わせるデスクトップ専用の概念。下部固定のみ。
  setPosition: () => {},
  setConnected(connected): void {
    controls.setConnected(connected);
    toolbarConnected = connected;
    if (connected) {
      syncTouchAttach();
    } else {
      touchInput.detach();
      softKeyboard.close();
      controls.setKeyboardOpen(false);
    }
  },
  // ショートカット割当 UI はデスクトップ専用 (キーボードショートカット設定) でモバイルには無い。
  refreshShortcutLabels: () => {},
};

// --- Session ↔ 共有 UI の窓口 ---
const host: SessionHost = {
  canvas,
  get toolbar(): ToolbarHandle {
    return toolbar;
  },
  getSettings,
  // モバイルは常に単一セッションのみを保持するため、タブ切替のようなアクティブ判定が不要。
  isActive: () => true,
  renderStatus: (s) => {
    renderStatusOverlay(s.status, s.statusText, {
      onClose: s.statusShowClose ? () => s.closeByUser() : undefined,
      onReconnect: s.status === "error" ? () => s.manualReconnect() : undefined,
    });
  },
  toast: (_s, text, kind?: ToastKind) => showToast(text, kind),
  updateStats: (s, ev: StatsEvent) => updateStats(ev, s.videoCodec),
  clearStats: () => clearStats(),
  // ファイル転送はモバイル対象外 (スコープ外)。
  fileResult: () => {},
  fileProgress: () => {},
  reconnectBadge: (_s, text) => showReconnectBadge(text),
  clearReconnectBadge: () => hideReconnectBadge(),
  // タブが無いので見た目更新は不要。
  refreshTab: () => {},
  removed: (s) => {
    // 非同期の切断経路 (自動再接続の打ち切り等) では、既に閉じられた古い Session の removed が
    // 新しい (乗り換え後の) Session を張った後に遅れて届くことがある。session !== s なら
    // 自分はもう「現行セッション」ではないので、新セッションの UI を巻き込んで壊さないよう
    // 何もしない。
    if (session !== s) return;
    session = null;
    touchInput.detach();
    softKeyboard.close();
    clearStats();
    hideReconnectBadge();
    showLauncherView();
  },
  // ウィンドウが無いのでリサイズ可否の概念が無い。
  setResizable: () => {},
  persistVideoParams(fps, mbps): void {
    // fps/Mbps と同じパターンで即時保存する。他窓への同報 (aurora://settings-changed) は
    // モバイルでは単一 WebView のため不要 (デスクトップの複数窓同期はここでは対象外)。
    settings = { ...getSettings(), fps, max_mbps: mbps };
    void saveSettings(settings).catch((e) => logger.warn("save settings failed", e));
  },
};

// --- 下部操作バー ---
const controls: MobileControlsHandle = mountMobileControls({
  root: controlsRoot,
  onDisconnect: () => session?.closeByUser(),
  onToggleTouchMode: (mode) => touchInput.setMode(mode),
  onToggleKeyboard: () => {
    if (softKeyboard.isOpen()) softKeyboard.close();
    else softKeyboard.open();
    controls.setKeyboardOpen(softKeyboard.isOpen());
  },
  onToggleStats: () => {
    toggleStats();
    controls.setStatsToggled(isStatsVisible());
  },
  onSelectDisplay: (d) => session?.switchDisplay(d),
  onVideoParams: (fps, mbps) => session?.setVideoParams(fps, mbps),
  // スティッキー修飾の表示は mobile-controls 側で完結するため、ここでは何もしない
  // (soft-keyboard は getStickyModifiers() で都度問い合わせる)。
  onModifiersChanged: () => {},
  onPressKey: (vk) => softKeyboard.press(vk),
  onResetZoom: () => touchInput.resetZoom(),
});

// --- ランチャー ---
const launcher: MobileLauncherHandle = mountMobileLauncher({
  root: launcherView,
  onConnect: async (entry, password, save) => {
    await connectTo(entry, password, save);
  },
});

/**
 * ホストへ接続する。接続中に別ホストへ繋ごうとした場合は現行セッションを先に切断する
 * (モバイルは 1 セッションのみ保持)。
 */
async function connectTo(entry: HostEntry, password: string | null, save: boolean): Promise<void> {
  if (session) {
    session.closeByUser();
    session = null;
  }
  const id = await stageSession(password);
  const s = new Session(id, entry, save, host);
  session = s;
  // 接続進捗 (スピナー/エラー) は remote-view 内の #conn-status に出るため、ここで表示切替する。
  showRemoteView();
  try {
    await s.start();
  } catch (e) {
    renderStatusOverlay("error", `接続に失敗しました: ${String(e)}`, {
      onClose: () => s.closeByUser(),
      onReconnect: () => s.manualReconnect(),
    });
    throw e;
  }
  s.activate();
}

// --- 画面回転 / ビューポート変化 → アクティブ Session の追従 ---
window.addEventListener("resize", () => session?.onWindowResize());
window.addEventListener("orientationchange", () => session?.onWindowResize());

// --- 起動 ---
void (async () => {
  // 計測フラグ (AURORA_PERF) を最初に取得しておく (decoder の vstat 集計が参照する)。
  await initPerfFlag();
  try {
    settings = await loadSettings();
  } catch {
    settings = null; // getSettings() の既定にフォールバック
  }
  showLauncherView();
  logger.info("mobile window ready");
})();
