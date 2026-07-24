// リモート画面下部の常時表示バー (モバイル専用)。
// デスクトップの `ui/toolbar.ts` はウィンドウ (data-tauri-drag-region 等) を前提にしており
// タッチ操作にも合わないため、モバイル向けに作り直す。`mobile-main.ts` 側でこの Handle を
// `ToolbarHandle` のアダプタから呼び出す (`ToolbarHandle` 自体は満たさない別レイヤー)。

import { formatDisplayName, type DisplayDto } from "../core/ipc";
import { CUSTOM_PRESET_ID, VIDEO_PRESETS, matchPreset } from "../core/video-presets";
import type { TouchMode } from "../core/touch-input";
import { VK } from "../core/soft-keyboard";
import { icon } from "./icons";
import "./mobile-controls.css";

export type MobileControlsOpts = {
  root: HTMLElement;
  onDisconnect: () => void;
  onToggleTouchMode: (mode: TouchMode) => void;
  onToggleKeyboard: () => void;
  onToggleStats: () => void;
  onSelectDisplay: (d: DisplayDto) => void;
  onVideoParams: (fps: number, mbps: number) => void;
  onModifiersChanged: (vks: number[]) => void;
  onPressKey: (vk: number) => void;
  onResetZoom: () => void;
};

export type MobileControlsHandle = {
  setConnLabel: (text: string) => void;
  setDisplays: (displays: DisplayDto[], selectedId?: number) => void;
  setVideoParams: (fps: number, mbps: number) => void;
  setConnected: (on: boolean) => void;
  setTouchMode: (mode: TouchMode) => void;
  setKeyboardOpen: (on: boolean) => void;
  setStatsToggled: (on: boolean) => void;
  setZoom: (scale: number) => void;
  /** 現在スティッキー押下中の修飾キー VK を返す。 */
  stickyModifiers: () => number[];
  /** スティッキー修飾を解除して表示を戻す。 */
  clearModifiers: () => void;
};

// スティッキートグルの修飾キー (押下状態を維持し、次の単発キーに合成される)。
const MODIFIER_KEYS: { vk: number; label: string }[] = [
  { vk: VK.CONTROL, label: "Ctrl" },
  { vk: VK.MENU, label: "Alt" },
  { vk: VK.SHIFT, label: "Shift" },
  { vk: VK.LWIN, label: "Win" },
];

// 単発キー (押した瞬間 down→up を送って終わる)。
const SINGLE_KEYS: { vk: number; label: string }[] = [
  { vk: VK.ESCAPE, label: "Esc" },
  { vk: VK.TAB, label: "Tab" },
  { vk: VK.RETURN, label: "Enter" },
  { vk: VK.BACK, label: "⌫" },
  { vk: VK.DELETE, label: "Del" },
  { vk: VK.LEFT, label: "←" },
  { vk: VK.UP, label: "↑" },
  { vk: VK.DOWN, label: "↓" },
  { vk: VK.RIGHT, label: "→" },
];

export function mountMobileControls(opts: MobileControlsOpts): MobileControlsHandle {
  opts.root.innerHTML = `
    <div class="mc-root">
      <div class="mc-label" id="mc-label"></div>
      <div class="mc-bar">
        <button type="button" class="mc-btn" id="mc-disconnect" title="切断">${icon("power")}</button>
        <button type="button" class="mc-btn" id="mc-touchmode" title="入力方式">${icon("mouse")}</button>
        <button type="button" class="mc-btn" id="mc-keyboard" title="キーボード">${icon("edit")}</button>
        <button type="button" class="mc-btn" id="mc-modtoggle" title="修飾キー">${icon("chevron")}</button>
        <button type="button" class="mc-btn" id="mc-stats" title="統計 HUD">${icon("stats")}</button>
        <button type="button" class="mc-btn" id="mc-more" title="その他">${icon("more")}</button>
      </div>
      <div class="mc-mods" id="mc-mods"></div>
    </div>
    <div class="mc-more-panel" id="mc-more-panel">
      <div class="mc-more-row" id="mc-display-row" style="display:none">
        <label for="mc-display-select">ディスプレイ</label>
        <select id="mc-display-select"></select>
      </div>
      <div class="mc-more-row">
        <label for="mc-quality-select">画質</label>
        <select id="mc-quality-select"></select>
      </div>
      <button type="button" class="mc-zoom-reset" id="mc-zoom-reset" style="display:none">ズームをリセット</button>
    </div>
  `;

  const disconnectBtn = opts.root.querySelector<HTMLButtonElement>("#mc-disconnect");
  const touchModeBtn = opts.root.querySelector<HTMLButtonElement>("#mc-touchmode");
  const keyboardBtn = opts.root.querySelector<HTMLButtonElement>("#mc-keyboard");
  const modToggleBtn = opts.root.querySelector<HTMLButtonElement>("#mc-modtoggle");
  const statsBtn = opts.root.querySelector<HTMLButtonElement>("#mc-stats");
  const moreBtn = opts.root.querySelector<HTMLButtonElement>("#mc-more");
  const labelEl = opts.root.querySelector<HTMLElement>("#mc-label");
  const modsEl = opts.root.querySelector<HTMLElement>("#mc-mods");
  const morePanel = opts.root.querySelector<HTMLElement>("#mc-more-panel");
  const displayRow = opts.root.querySelector<HTMLElement>("#mc-display-row");
  const displaySelect = opts.root.querySelector<HTMLSelectElement>("#mc-display-select");
  const qualitySelect = opts.root.querySelector<HTMLSelectElement>("#mc-quality-select");
  const zoomResetBtn = opts.root.querySelector<HTMLButtonElement>("#mc-zoom-reset");

  // 画質プリセット (末尾にカスタムを足す。値は "fps:mbps" 形式で select に載せる)。
  if (qualitySelect) {
    for (const p of VIDEO_PRESETS) {
      const o = document.createElement("option");
      o.value = `${p.fps}:${p.mbps}`;
      o.textContent = p.label;
      qualitySelect.appendChild(o);
    }
    const custom = document.createElement("option");
    custom.value = CUSTOM_PRESET_ID;
    custom.textContent = "カスタム";
    custom.disabled = true; // モバイル簡易版は手入力欄を持たないため選択不可 (現在値の表示のみ)
    qualitySelect.appendChild(custom);
    qualitySelect.addEventListener("change", () => {
      const v = qualitySelect.value;
      if (v === CUSTOM_PRESET_ID) return;
      const [fps, mbps] = v.split(":").map(Number);
      opts.onVideoParams(fps, mbps);
    });
  }

  disconnectBtn?.addEventListener("click", () => opts.onDisconnect());

  let touchMode: TouchMode = "trackpad";
  touchModeBtn?.addEventListener("click", () => {
    touchMode = touchMode === "trackpad" ? "direct" : "trackpad";
    applyTouchModeBtn();
    opts.onToggleTouchMode(touchMode);
  });
  function applyTouchModeBtn(): void {
    if (!touchModeBtn) return;
    touchModeBtn.classList.toggle("toggled", touchMode === "direct");
    touchModeBtn.title = touchMode === "trackpad" ? "入力方式: トラックパッド" : "入力方式: 直接タップ";
  }
  applyTouchModeBtn();

  let keyboardOpen = false;
  keyboardBtn?.addEventListener("click", () => opts.onToggleKeyboard());

  let modsVisible = false;
  modToggleBtn?.addEventListener("click", () => {
    modsVisible = !modsVisible;
    modsEl?.classList.toggle("visible", modsVisible);
    modToggleBtn.classList.toggle("toggled", modsVisible);
  });

  statsBtn?.addEventListener("click", () => opts.onToggleStats());

  let moreVisible = false;
  function setMoreVisible(on: boolean): void {
    moreVisible = on;
    morePanel?.classList.toggle("visible", on);
    moreBtn?.classList.toggle("toggled", on);
  }
  moreBtn?.addEventListener("click", () => setMoreVisible(!moreVisible));

  displaySelect?.addEventListener("change", () => {
    const id = Number(displaySelect.value);
    const d = displays.find((x) => x.id === id);
    if (d) opts.onSelectDisplay(d);
  });

  zoomResetBtn?.addEventListener("click", () => opts.onResetZoom());

  // --- 修飾キー / 単発キー段 ---
  const stickyVks = new Set<number>();
  const modBtns = new Map<number, HTMLButtonElement>();
  if (modsEl) {
    for (const m of MODIFIER_KEYS) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "mc-key";
      btn.textContent = m.label;
      btn.addEventListener("click", () => {
        if (stickyVks.has(m.vk)) stickyVks.delete(m.vk);
        else stickyVks.add(m.vk);
        btn.classList.toggle("active", stickyVks.has(m.vk));
        opts.onModifiersChanged([...stickyVks]);
      });
      modsEl.appendChild(btn);
      modBtns.set(m.vk, btn);
    }
    for (const k of SINGLE_KEYS) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "mc-key";
      btn.textContent = k.label;
      btn.addEventListener("click", () => opts.onPressKey(k.vk));
      modsEl.appendChild(btn);
    }
  }

  let displays: DisplayDto[] = [];
  let connected = false;

  return {
    setConnLabel(text: string): void {
      if (labelEl) labelEl.textContent = text;
    },
    setDisplays(list: DisplayDto[], selectedId?: number): void {
      displays = list;
      if (!displaySelect || !displayRow) return;
      // 表示名はホスト由来の文字列なので option.textContent 経由で組み innerHTML には流さない。
      displaySelect.replaceChildren(
        ...list.map((d) => {
          const o = document.createElement("option");
          o.value = String(d.id);
          o.textContent = `${formatDisplayName(d.name)} (${d.width}x${d.height})`;
          return o;
        }),
      );
      if (selectedId !== undefined) displaySelect.value = String(selectedId);
      displayRow.style.display = list.length > 1 ? "flex" : "none";
    },
    setVideoParams(fps: number, mbps: number): void {
      if (!qualitySelect) return;
      const id = matchPreset(fps, mbps);
      qualitySelect.value = id === CUSTOM_PRESET_ID ? CUSTOM_PRESET_ID : `${fps}:${mbps}`;
    },
    setConnected(on: boolean): void {
      connected = on;
      opts.root.style.display = connected ? "" : "none";
      if (!connected) {
        setMoreVisible(false);
        modsVisible = false;
        modsEl?.classList.remove("visible");
        modToggleBtn?.classList.remove("toggled");
      }
    },
    setTouchMode(mode: TouchMode): void {
      touchMode = mode;
      applyTouchModeBtn();
    },
    setKeyboardOpen(on: boolean): void {
      keyboardOpen = on;
      keyboardBtn?.classList.toggle("toggled", keyboardOpen);
    },
    setStatsToggled(on: boolean): void {
      statsBtn?.classList.toggle("toggled", on);
    },
    setZoom(scale: number): void {
      if (zoomResetBtn) zoomResetBtn.style.display = Math.abs(scale - 1) > 0.01 ? "" : "none";
    },
    stickyModifiers(): number[] {
      return [...stickyVks];
    },
    clearModifiers(): void {
      stickyVks.clear();
      for (const btn of modBtns.values()) btn.classList.remove("active");
      opts.onModifiersChanged([]);
    },
  };
}
