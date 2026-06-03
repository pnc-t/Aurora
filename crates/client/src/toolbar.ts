// 接続中の自動格納ツールバー (RDP/RustDesk 風)。
// 既定は画面外に隠れ、上端ホバーゾーン or ホットキーで表示。ピンで常時表示。
// 中身: disconnect / ディスプレイ切替 / 統計トグル / ファイル転送トグル / 接続ラベル / ピン。

import type { DisplayDto } from "./ipc";

type ToolbarOpts = {
  topbar: HTMLElement;
  hotzone: HTMLElement;
  onDisconnect: () => void;
  onSelectDisplay: (display: DisplayDto) => void;
  onToggleStats: () => void;
  onToggleFilePanel: () => void;
  onToggleFullscreen: () => void;
  onToggleCapture: () => void;
};

export type ToolbarHandle = {
  setConnLabel: (text: string) => void;
  setDisplays: (displays: DisplayDto[], selectedId?: number) => void;
  setStatsToggled: (on: boolean) => void;
  setFileToggled: (on: boolean) => void;
  setFullscreenToggled: (on: boolean) => void;
  setCaptureToggled: (on: boolean) => void;
  toggleBar: () => void;
  setConnected: (connected: boolean) => void;
};

export function mountToolbar(opts: ToolbarOpts): ToolbarHandle {
  opts.topbar.innerHTML = `
    <button type="button" id="tb-disconnect" class="danger">disconnect</button>
    <span id="display-wrap">display
      <select id="display-select"></select>
    </span>
    <button type="button" id="tb-capture" title="マウスキャプチャ (Ctrl+Alt+Shift+M)">capture</button>
    <button type="button" id="tb-fullscreen" title="フルスクリーン (Ctrl+Alt+Shift+F / F11)">fullscreen</button>
    <button type="button" id="tb-files">files</button>
    <button type="button" id="tb-stats" title="Ctrl+Alt+Shift+S">stats</button>
    <span class="spacer"></span>
    <span id="conn-label">—</span>
    <button type="button" id="tb-pin" title="ピン留め (常時表示)">📌</button>
  `;

  const disconnectBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-disconnect");
  const displayWrap = opts.topbar.querySelector<HTMLElement>("#display-wrap");
  const displaySelect = opts.topbar.querySelector<HTMLSelectElement>("#display-select");
  const filesBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-files");
  const statsBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-stats");
  const captureBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-capture");
  const fullscreenBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-fullscreen");
  const connLabel = opts.topbar.querySelector<HTMLElement>("#conn-label");
  const pinBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-pin");

  let displays: DisplayDto[] = [];
  let pinned = false;
  let connected = false;
  let hideTimer: number | null = null;

  function show(): void {
    if (hideTimer !== null) {
      window.clearTimeout(hideTimer);
      hideTimer = null;
    }
    opts.topbar.classList.add("visible");
  }
  function scheduleHide(): void {
    if (pinned || !connected) return;
    if (hideTimer !== null) window.clearTimeout(hideTimer);
    hideTimer = window.setTimeout(() => opts.topbar.classList.remove("visible"), 400);
  }
  function hideNow(): void {
    if (hideTimer !== null) {
      window.clearTimeout(hideTimer);
      hideTimer = null;
    }
    opts.topbar.classList.remove("visible");
  }

  // 上端ホバーで表示、バーから離れたら格納 (ピン時は維持)。
  opts.hotzone.addEventListener("mouseenter", () => {
    if (connected) show();
  });
  opts.topbar.addEventListener("mouseenter", show);
  opts.topbar.addEventListener("mouseleave", scheduleHide);

  disconnectBtn?.addEventListener("click", () => opts.onDisconnect());
  filesBtn?.addEventListener("click", () => opts.onToggleFilePanel());
  statsBtn?.addEventListener("click", () => opts.onToggleStats());
  captureBtn?.addEventListener("click", () => opts.onToggleCapture());
  fullscreenBtn?.addEventListener("click", () => opts.onToggleFullscreen());
  pinBtn?.addEventListener("click", () => {
    pinned = !pinned;
    pinBtn.classList.toggle("toggled", pinned);
    if (pinned) show();
    else scheduleHide();
  });
  displaySelect?.addEventListener("change", () => {
    const id = Number(displaySelect.value);
    const d = displays.find((x) => x.id === id);
    if (d) opts.onSelectDisplay(d);
  });

  return {
    setConnLabel(text: string): void {
      if (connLabel) connLabel.textContent = text;
    },
    setDisplays(list: DisplayDto[], selectedId?: number): void {
      displays = list;
      if (!displaySelect || !displayWrap) return;
      displaySelect.innerHTML = list
        .map((x) => `<option value="${x.id}">${x.name} (${x.width}x${x.height})</option>`)
        .join("");
      if (selectedId !== undefined) displaySelect.value = String(selectedId);
      // ディスプレイが複数あるときだけ切替 UI を出す。
      displayWrap.style.display = list.length > 1 ? "inline-flex" : "none";
    },
    setStatsToggled(on: boolean): void {
      statsBtn?.classList.toggle("toggled", on);
    },
    setFileToggled(on: boolean): void {
      filesBtn?.classList.toggle("toggled", on);
    },
    setFullscreenToggled(on: boolean): void {
      fullscreenBtn?.classList.toggle("toggled", on);
    },
    setCaptureToggled(on: boolean): void {
      captureBtn?.classList.toggle("toggled", on);
    },
    toggleBar(): void {
      if (!connected) return;
      if (opts.topbar.classList.contains("visible")) hideNow();
      else show();
    },
    setConnected(isConnected: boolean): void {
      connected = isConnected;
      if (isConnected) {
        // 接続直後は一度バーを見せて操作場所を知らせる。
        show();
        scheduleHide();
      } else {
        pinned = false;
        pinBtn?.classList.remove("toggled");
        hideNow();
      }
    },
  };
}
