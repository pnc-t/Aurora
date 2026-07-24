// リモート画面下部の操作バー (モバイル専用)。
// デスクトップの `ui/toolbar.ts` はウィンドウ (data-tauri-drag-region 等) を前提にしており
// タッチ操作にも合わないため、モバイル向けに作り直す。`mobile-main.ts` 側でこの Handle を
// `ToolbarHandle` のアダプタから呼び出す (`ToolbarHandle` 自体は満たさない別レイヤー)。
//
// バーは常時表示せず、下端のハンドルをタップしたときだけせり上がる (映像を覆う面積を減らす)。

import { formatDisplayName, type DisplayDto } from "../core/ipc";
import { VK } from "../core/soft-keyboard";
import { icon } from "./icons";
import "./mobile-controls.css";

export type MobileControlsOpts = {
  root: HTMLElement;
  onDisconnect: () => void;
  onToggleKeyboard: () => void;
  onSelectDisplay: (d: DisplayDto) => void;
  onModifiersChanged: (vks: number[]) => void;
  onPressKey: (vk: number) => void;
  onResetZoom: () => void;
};

export type MobileControlsHandle = {
  setConnLabel: (text: string) => void;
  setDisplays: (displays: DisplayDto[], selectedId?: number) => void;
  setConnected: (on: boolean) => void;
  setKeyboardOpen: (on: boolean) => void;
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

/** 引き出したバーを自動格納するまでの無操作時間。 */
const AUTO_HIDE_MS = 5000;

export function mountMobileControls(opts: MobileControlsOpts): MobileControlsHandle {
  opts.root.innerHTML = `
    <button type="button" class="mc-handle" id="mc-handle" aria-label="操作バー"></button>
    <div class="mc-root" id="mc-root">
      <div class="mc-label" id="mc-label"></div>
      <div class="mc-bar">
        <button type="button" class="mc-btn" id="mc-disconnect" title="切断">${icon("power")}</button>
        <button type="button" class="mc-btn" id="mc-keyboard" title="キーボード">${icon("edit")}</button>
        <button type="button" class="mc-btn" id="mc-modtoggle" title="修飾キー">${icon("chevron")}</button>
        <button type="button" class="mc-btn" id="mc-more" title="その他">${icon("more")}</button>
      </div>
      <div class="mc-mods" id="mc-mods"></div>
    </div>
    <div class="mc-more-panel" id="mc-more-panel">
      <div class="mc-more-row" id="mc-display-row" style="display:none">
        <label for="mc-display-select">ディスプレイ</label>
        <select id="mc-display-select"></select>
      </div>
      <button type="button" class="mc-zoom-reset" id="mc-zoom-reset" style="display:none">ズームをリセット</button>
    </div>
  `;

  const handleBtn = opts.root.querySelector<HTMLButtonElement>("#mc-handle");
  const rootEl = opts.root.querySelector<HTMLElement>("#mc-root");
  const disconnectBtn = opts.root.querySelector<HTMLButtonElement>("#mc-disconnect");
  const keyboardBtn = opts.root.querySelector<HTMLButtonElement>("#mc-keyboard");
  const modToggleBtn = opts.root.querySelector<HTMLButtonElement>("#mc-modtoggle");
  const moreBtn = opts.root.querySelector<HTMLButtonElement>("#mc-more");
  const labelEl = opts.root.querySelector<HTMLElement>("#mc-label");
  const modsEl = opts.root.querySelector<HTMLElement>("#mc-mods");
  const morePanel = opts.root.querySelector<HTMLElement>("#mc-more-panel");
  const displayRow = opts.root.querySelector<HTMLElement>("#mc-display-row");
  const displaySelect = opts.root.querySelector<HTMLSelectElement>("#mc-display-select");
  const zoomResetBtn = opts.root.querySelector<HTMLButtonElement>("#mc-zoom-reset");

  // --- 引き出し / 自動格納 ---
  //
  // 「バー以外をタップしたら閉じる」方式は採らない。タッチ入力は直接タップ式 (タップした座標へ
  // カーソルを移動してクリック) に固定されているため、閉じるつもりのタップがそのままホストへの
  // クリックとして送られてしまう。閉じる手段はハンドルの再タップと無操作時の自動格納だけにする。
  let expanded = false;
  let hideTimer: number | null = null;
  let modsVisible = false;
  let moreVisible = false;

  function cancelAutoHide(): void {
    if (hideTimer !== null) {
      clearTimeout(hideTimer);
      hideTimer = null;
    }
  }

  /** 無操作タイマーを張り直す。バー内の操作のたびに呼ぶ。 */
  function scheduleAutoHide(): void {
    cancelAutoHide();
    if (!expanded) return;
    // 修飾キー段やその他パネルを開いている間は操作の途中なので、勝手に消さない。
    if (modsVisible || moreVisible) return;
    hideTimer = window.setTimeout(() => setExpanded(false), AUTO_HIDE_MS);
  }

  function setModsVisible(on: boolean): void {
    modsVisible = on;
    modsEl?.classList.toggle("visible", on);
    modToggleBtn?.classList.toggle("toggled", on);
  }

  function setMoreVisible(on: boolean): void {
    moreVisible = on;
    morePanel?.classList.toggle("visible", on);
    moreBtn?.classList.toggle("toggled", on);
  }

  function setExpanded(on: boolean): void {
    expanded = on;
    rootEl?.classList.toggle("expanded", on);
    handleBtn?.classList.toggle("toggled", on);
    // 格納時は展開中のサブパネルも畳む (次に引き出したとき素の状態から始める)。
    if (!on) {
      setModsVisible(false);
      setMoreVisible(false);
    }
    scheduleAutoHide();
  }

  handleBtn?.addEventListener("click", () => setExpanded(!expanded));

  disconnectBtn?.addEventListener("click", () => opts.onDisconnect());

  keyboardBtn?.addEventListener("click", () => {
    opts.onToggleKeyboard();
    scheduleAutoHide();
  });

  modToggleBtn?.addEventListener("click", () => {
    setModsVisible(!modsVisible);
    scheduleAutoHide();
  });

  moreBtn?.addEventListener("click", () => {
    setMoreVisible(!moreVisible);
    scheduleAutoHide();
  });

  displaySelect?.addEventListener("change", () => {
    const id = Number(displaySelect.value);
    const d = displays.find((x) => x.id === id);
    if (d) opts.onSelectDisplay(d);
    scheduleAutoHide();
  });

  zoomResetBtn?.addEventListener("click", () => {
    opts.onResetZoom();
    scheduleAutoHide();
  });

  // 「その他」パネルの中身は両方とも条件付き表示なので、1 つも出せるものが無いときは
  // 入口のボタンごと隠す (空のパネルが開くのを防ぐ)。
  let hasDisplayChoice = false;
  let hasZoomReset = false;
  function syncMoreAvailability(): void {
    const available = hasDisplayChoice || hasZoomReset;
    if (moreBtn) moreBtn.style.display = available ? "" : "none";
    if (!available && moreVisible) setMoreVisible(false);
  }
  syncMoreAvailability();

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
        scheduleAutoHide();
      });
      modsEl.appendChild(btn);
      modBtns.set(m.vk, btn);
    }
    for (const k of SINGLE_KEYS) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "mc-key";
      btn.textContent = k.label;
      btn.addEventListener("click", () => {
        opts.onPressKey(k.vk);
        scheduleAutoHide();
      });
      modsEl.appendChild(btn);
    }
  }

  let displays: DisplayDto[] = [];

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
      hasDisplayChoice = list.length > 1;
      displayRow.style.display = hasDisplayChoice ? "flex" : "none";
      syncMoreAvailability();
    },
    setConnected(on: boolean): void {
      opts.root.style.display = on ? "" : "none";
      // 切断時は引き出し状態を畳んでおく (次の接続をハンドルだけの状態から始める)。
      if (!on) {
        cancelAutoHide();
        setExpanded(false);
      }
    },
    setKeyboardOpen(on: boolean): void {
      keyboardBtn?.classList.toggle("toggled", on);
    },
    setZoom(scale: number): void {
      hasZoomReset = Math.abs(scale - 1) > 0.01;
      if (zoomResetBtn) zoomResetBtn.style.display = hasZoomReset ? "" : "none";
      syncMoreAvailability();
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
