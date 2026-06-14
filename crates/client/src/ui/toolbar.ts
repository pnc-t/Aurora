// 接続中の操作ツールバー。
// 既定で常時表示 (上端の透明な帯に、角丸+影のピルを 1 枚浮かせて中央配置するフローティング型)。
// Ctrl+Alt+Shift+B で一時的に隠せる。
// 構成 (ピル内): [切断] | [全画面/キャプチャ/ファイル/統計] [画面切替] | [プリセット(+カスタム時のみ fps/Mbps)] | [pin] | [min/close]
// 各操作はフラットなアイコンボタン + ツールチップ。ピル本体 (.tb-pill) と透明な帯 (#topbar)、
// 構造スパンに data-tauri-drag-region を付け、ボタン/入力/select 以外の余白・隔て・帯の余白どこを
// 掴んでもウィンドウ移動できる。
// 接続先情報は省スペース化のためバーの title (ホバー tooltip) に退避し、行内には常駐させない。

import { formatDisplayName, type DisplayDto } from "../core/ipc";
import { CUSTOM_PRESET_ID, findPreset, matchPreset, presetOptionsHtml } from "../core/video-presets";
import { mountWindowControls } from "./window-controls";
import { icon } from "./icons";
import "./toolbar.css";

type ToolbarOpts = {
  topbar: HTMLElement;
  onDisconnect: () => void;
  onSelectDisplay: (display: DisplayDto) => void;
  onToggleStats: () => void;
  onToggleFilePanel: () => void;
  onToggleFullscreen: () => void;
  onToggleCapture: () => void;
  onSetVideoParams: (fps: number, mbps: number) => void;
};

export type ToolbarHandle = {
  setConnLabel: (text: string) => void;
  setDisplays: (displays: DisplayDto[], selectedId?: number) => void;
  setStatsToggled: (on: boolean) => void;
  setFileToggled: (on: boolean) => void;
  setFullscreenToggled: (on: boolean) => void;
  setCaptureToggled: (on: boolean) => void;
  setVideoParams: (fps: number, mbps: number) => void;
  toggleBar: () => void;
  setConnected: (connected: boolean) => void;
};

export function mountToolbar(opts: ToolbarOpts): ToolbarHandle {
  opts.topbar.innerHTML = `
    <div class="tb-pill" data-tauri-drag-region>
      <div class="tb-group" data-tauri-drag-region>
        <button type="button" id="tb-disconnect" class="tb-btn danger" title="切断">${icon("power")}</button>
      </div>
      <span class="tb-sep" data-tauri-drag-region></span>
      <div class="tb-group" data-tauri-drag-region>
        <button type="button" id="tb-fullscreen" class="tb-btn" title="フルスクリーン (F11 / Ctrl+Alt+Shift+F)">${icon("fullscreen")}</button>
        <button type="button" id="tb-capture" class="tb-btn" title="マウスキャプチャ (Ctrl+Alt+Shift+M)">${icon("mouse")}</button>
        <button type="button" id="tb-files" class="tb-btn" title="ファイル転送">${icon("files")}</button>
        <button type="button" id="tb-stats" class="tb-btn" title="統計 HUD (Ctrl+Alt+Shift+S)">${icon("stats")}</button>
      </div>
      <span id="display-wrap" class="tb-group" title="表示するディスプレイ" data-tauri-drag-region>
        <span class="tb-ico" data-tauri-drag-region>${icon("display")}</span>
        <select id="display-select"></select>
      </span>
      <span class="tb-sep" data-tauri-drag-region></span>
      <span class="tb-video" title="画質プリセット / 送出 fps 上限 (0=無制限) / ビットレート上限" data-tauri-drag-region>
        <select id="tb-preset" title="画質プリセット">${presetOptionsHtml()}</select>
        <span id="tb-custom" class="tb-custom">
          <input type="text" inputmode="numeric" id="tb-fps" />fps
          <input type="text" inputmode="numeric" id="tb-mbps" />Mbps
        </span>
      </span>
      <span class="tb-sep" data-tauri-drag-region></span>
      <button type="button" id="tb-pin" class="tb-btn" title="バーをピン留め (常時表示) / Ctrl+Alt+Shift+B">${icon("pin")}</button>
      <span id="tb-winctl" class="window-controls"></span>
    </div>
  `;
  // 透明な帯 (ピル周囲の余白) もウィンドウ移動の掴みどころにする。
  opts.topbar.setAttribute("data-tauri-drag-region", "");

  // フレームレスのため接続中もウィンドウ操作 (min/close) をバーに持つ。
  const winctl = opts.topbar.querySelector<HTMLElement>("#tb-winctl");
  if (winctl) mountWindowControls(winctl);

  const disconnectBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-disconnect");
  const displayWrap = opts.topbar.querySelector<HTMLElement>("#display-wrap");
  const displaySelect = opts.topbar.querySelector<HTMLSelectElement>("#display-select");
  const filesBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-files");
  const statsBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-stats");
  const captureBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-capture");
  const fullscreenBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-fullscreen");
  const pinBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-pin");
  const presetSelect = opts.topbar.querySelector<HTMLSelectElement>("#tb-preset");
  const customWrap = opts.topbar.querySelector<HTMLElement>("#tb-custom");
  const fpsInput = opts.topbar.querySelector<HTMLInputElement>("#tb-fps");
  const mbpsInput = opts.topbar.querySelector<HTMLInputElement>("#tb-mbps");

  // fps/Mbps の手動入力欄はプリセット=カスタムのときだけ出す (常時表示は情報重複で幅を食う)。
  function applyCustomVisibility(presetId: string): void {
    if (customWrap) customWrap.style.display = presetId === CUSTOM_PRESET_ID ? "inline-flex" : "none";
  }

  function syncPreset(fps: number, mbps: number): void {
    const id = matchPreset(fps, mbps);
    if (presetSelect) presetSelect.value = id;
    applyCustomVisibility(id);
  }

  function emitVideoParams(): void {
    const fps = Math.max(0, Math.floor(Number(fpsInput?.value) || 0));
    const mbps = Math.max(1, Math.floor(Number(mbpsInput?.value) || 1));
    if (fpsInput) fpsInput.value = String(fps);
    if (mbpsInput) mbpsInput.value = String(mbps);
    syncPreset(fps, mbps);
    opts.onSetVideoParams(fps, mbps);
  }
  fpsInput?.addEventListener("change", emitVideoParams);
  mbpsInput?.addEventListener("change", emitVideoParams);
  // プリセット選択で fps/Mbps 欄を埋めて即適用 (カスタムは手動入力を維持)。
  presetSelect?.addEventListener("change", () => {
    applyCustomVisibility(presetSelect.value);
    const p = findPreset(presetSelect.value);
    if (!p) return; // カスタム: 入力欄をそのまま使う
    if (fpsInput) fpsInput.value = String(p.fps);
    if (mbpsInput) mbpsInput.value = String(p.mbps);
    opts.onSetVideoParams(p.fps, p.mbps);
  });
  // 初期表示 (まだ setVideoParams 前) でも custom 欄の出し分けを確定させる。
  applyCustomVisibility(presetSelect?.value ?? "");

  let displays: DisplayDto[] = [];
  let connected = false;
  // ピン留め (常時表示)。既定は false = 自動格納し、上端ホバーで一時表示する。
  let pinned = false;
  // 上端ホバーで一時表示中か (ピン留め時は無関係)。
  let revealed = false;

  function barHeight(): number {
    // transform で隠していてもレイアウト高は取れる。未確定時は概算。
    return opts.topbar.offsetHeight || 48;
  }

  function applyVisibility(): void {
    const visible = connected && (pinned || revealed);
    opts.topbar.classList.toggle("visible", visible);
    if (visible) {
      // 実測したバー高を CSS 変数へ (映像エリア/統計 HUD をこの分だけ下げる)。
      document.documentElement.style.setProperty("--topbar-h", `${opts.topbar.offsetHeight}px`);
    }
    pinBtn?.classList.toggle("toggled", pinned);
    // bar-open: バー表示中は常に映像エリア/統計 HUD を確保し、キャプチャに重ならないようにする
    // (ピン留め・上端ホバー表示のどちらでも同じ挙動)。
    document.body.classList.toggle("bar-open", visible);
  }

  function setPinned(on: boolean): void {
    pinned = on;
    if (pinned) revealed = false;
    applyVisibility();
  }

  // 上端付近にマウスがある間だけ表示する (Y 座標で判定)。
  // mouseleave ではなく座標判定にすることで、バーを掴んでウィンドウをドラッグ移動する
  // 間 (カーソルは上端に留まる) もバーが消えない。ネイティブドラッグ中は mousemove が
  // 来なくても revealed は維持される。
  window.addEventListener("mousemove", (e) => {
    if (!connected || pinned) return;
    const want = e.clientY <= barHeight() + 6;
    if (want !== revealed) {
      revealed = want;
      applyVisibility();
    }
  });

  disconnectBtn?.addEventListener("click", () => opts.onDisconnect());
  filesBtn?.addEventListener("click", () => opts.onToggleFilePanel());
  statsBtn?.addEventListener("click", () => opts.onToggleStats());
  captureBtn?.addEventListener("click", () => opts.onToggleCapture());
  fullscreenBtn?.addEventListener("click", () => opts.onToggleFullscreen());
  pinBtn?.addEventListener("click", () => setPinned(!pinned));
  displaySelect?.addEventListener("change", () => {
    const id = Number(displaySelect.value);
    const d = displays.find((x) => x.id === id);
    if (d) opts.onSelectDisplay(d);
  });

  return {
    setConnLabel(text: string): void {
      // 行内に常駐させず、バーのホバー tooltip として接続先情報を保持する。
      opts.topbar.title = text;
    },
    setDisplays(list: DisplayDto[], selectedId?: number): void {
      displays = list;
      if (!displaySelect || !displayWrap) return;
      // 表示名はホスト由来 (リモート) の文字列なので innerHTML に流さず、DOM + textContent
      // で組んで XSS を防ぐ (悪意あるホストが Display.name に <img onerror> 等を仕込む経路を塞ぐ)。
      displaySelect.replaceChildren(
        ...list.map((x) => {
          const opt = document.createElement("option");
          opt.value = String(x.id);
          opt.textContent = `${formatDisplayName(x.name)} (${x.width}x${x.height})`;
          return opt;
        }),
      );
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
    setVideoParams(fps: number, mbps: number): void {
      if (fpsInput) fpsInput.value = String(fps);
      if (mbpsInput) mbpsInput.value = String(mbps);
      syncPreset(fps, mbps);
    },
    /** Ctrl+Alt+Shift+B: ピン留めのトグル (常時表示 ⇔ 自動格納)。 */
    toggleBar(): void {
      if (!connected) return;
      setPinned(!pinned);
    },
    setConnected(isConnected: boolean): void {
      connected = isConnected;
      // 既定はピン留め (常時表示・映像とは重ならない)。ピンを外すと自動格納 (RDP の接続バー風)。
      pinned = isConnected;
      revealed = false;
      applyVisibility();
    },
  };
}
