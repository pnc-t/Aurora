// リモート画面下部の操作バー (モバイル専用)。
// デスクトップの `ui/toolbar.ts` はウィンドウ (data-tauri-drag-region 等) を前提にしており
// タッチ操作にも合わないため、モバイル向けに作り直す。`mobile-main.ts` 側でこの Handle を
// `ToolbarHandle` のアダプタから呼び出す (`ToolbarHandle` 自体は満たさない別レイヤー)。
//
// バーは常時表示せず、ハンドルをタップしたときだけせり上がる (映像を覆う面積を減らす)。
// 配置 (上下左右) はデスクトップのツールバーと同じ `ToolbarPosition` を共有する。

import { formatDisplayName, type DisplayDto, type ToolbarPosition } from "../core/ipc";
import { icon } from "./icons";
import "./mobile-controls.css";

export type MobileControlsOpts = {
  root: HTMLElement;
  onDisconnect: () => void;
  onToggleKeyboard: () => void;
  onSelectDisplay: (d: DisplayDto) => void;
  onResetZoom: () => void;
  /** オンスクリーンコントローラの表示を切り替える。 */
  onTogglePad: () => void;
};

export type MobileControlsHandle = {
  setDisplays: (displays: DisplayDto[], selectedId?: number) => void;
  setConnected: (on: boolean) => void;
  setKeyboardOpen: (on: boolean) => void;
  setZoom: (scale: number) => void;
  /** バーの配置を切り替える (デスクトップの ToolbarHandle.setPosition に相当)。既定は "bottom"。 */
  setPosition: (pos: ToolbarPosition) => void;
  /** オンスクリーンコントローラの表示状態をボタンの見た目へ反映する。 */
  setPadOn: (on: boolean) => void;
};

/** 引き出したバーを自動格納するまでの無操作時間。 */
const AUTO_HIDE_MS = 5000;

export function mountMobileControls(opts: MobileControlsOpts): MobileControlsHandle {
  opts.root.innerHTML = `
    <div class="mc-root" id="mc-root" data-pos="bottom">
      <button type="button" class="mc-handle" id="mc-handle" aria-label="操作バー"></button>
      <div class="mc-panel">
        <div class="mc-bar">
          <button type="button" class="mc-btn" id="mc-disconnect" title="切断">${icon("power")}</button>
          <button type="button" class="mc-btn" id="mc-keyboard" title="キーボード">${icon("edit")}</button>
          <button type="button" class="mc-btn" id="mc-pad" title="コントローラ">${icon("mouse")}</button>
          <button type="button" class="mc-btn" id="mc-display" title="ディスプレイ" style="display:none">${icon("display")}</button>
          <button type="button" class="mc-btn" id="mc-zoom-reset" title="ズームをリセット" style="display:none">${icon("refresh")}</button>
        </div>
      </div>
    </div>
    <div class="mc-display-pop" id="mc-display-pop"></div>
  `;

  const handleBtn = opts.root.querySelector<HTMLButtonElement>("#mc-handle");
  const rootEl = opts.root.querySelector<HTMLElement>("#mc-root");
  const disconnectBtn = opts.root.querySelector<HTMLButtonElement>("#mc-disconnect");
  const keyboardBtn = opts.root.querySelector<HTMLButtonElement>("#mc-keyboard");
  const padBtn = opts.root.querySelector<HTMLButtonElement>("#mc-pad");
  const displayBtn = opts.root.querySelector<HTMLButtonElement>("#mc-display");
  const displayPop = opts.root.querySelector<HTMLElement>("#mc-display-pop");
  // ズームリセット専用のアイコンは用意していないため、意味が近い「リセット」の refresh を流用する。
  const zoomResetBtn = opts.root.querySelector<HTMLButtonElement>("#mc-zoom-reset");

  // --- 引き出し / 自動格納 ---
  //
  // 「バー以外をタップしたら閉じる」方式は採らない。タッチ入力は直接タップ式 (タップした座標へ
  // カーソルを移動してクリック) に固定されているため、閉じるつもりのタップがそのままホストへの
  // クリックとして送られてしまう。閉じる手段はハンドルの再タップと無操作時の自動格納だけにする。
  let expanded = false;
  let hideTimer: number | null = null;
  let displayPopVisible = false;

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
    // ディスプレイ選択ポップオーバーを開いている間は操作の途中なので、勝手に消さない。
    if (displayPopVisible) return;
    hideTimer = window.setTimeout(() => setExpanded(false), AUTO_HIDE_MS);
  }

  let hasDisplayChoice = false;
  function setDisplayPopVisible(on: boolean): void {
    // 表示先が 1 台しかないとき (ボタン自体が隠れている) は開けない。
    if (on && !hasDisplayChoice) return;
    displayPopVisible = on;
    displayPop?.classList.toggle("visible", on);
    displayBtn?.classList.toggle("toggled", on);
    scheduleAutoHide();
  }

  function setExpanded(on: boolean): void {
    expanded = on;
    rootEl?.classList.toggle("expanded", on);
    handleBtn?.classList.toggle("toggled", on);
    // 格納時は展開中のサブパネルも畳む (次に引き出したとき素の状態から始める)。
    if (!on) {
      setDisplayPopVisible(false);
    }
    scheduleAutoHide();
  }

  handleBtn?.addEventListener("click", () => setExpanded(!expanded));

  disconnectBtn?.addEventListener("click", () => opts.onDisconnect());

  // mousedown の既定動作 (フォーカス移動) は他のモバイル UI ボタンと同様に止める
  // (意図しないフォーカス変化を防ぐ)。click は素通しする。
  // touchstart は preventDefault しない (Android では合成 click まで消えボタンが反応しなくなる)。
  keyboardBtn?.addEventListener("mousedown", (e) => e.preventDefault());

  keyboardBtn?.addEventListener("click", () => {
    opts.onToggleKeyboard();
    scheduleAutoHide();
  });

  // キーボードボタンと同じ理由 (タップ後にフォーカスリングが残るのを防ぐ)。
  padBtn?.addEventListener("mousedown", (e) => e.preventDefault());

  padBtn?.addEventListener("click", () => {
    opts.onTogglePad();
    scheduleAutoHide();
  });

  displayBtn?.addEventListener("click", () => setDisplayPopVisible(!displayPopVisible));

  zoomResetBtn?.addEventListener("click", () => {
    opts.onResetZoom();
    scheduleAutoHide();
  });

  let displays: DisplayDto[] = [];
  let displayItems: { id: number; btn: HTMLButtonElement }[] = [];
  function syncDisplayActive(selectedId?: number): void {
    for (const it of displayItems) it.btn.classList.toggle("active", it.id === selectedId);
  }

  return {
    setDisplays(list: DisplayDto[], selectedId?: number): void {
      displays = list;
      hasDisplayChoice = displays.length > 1;
      if (displayPop) {
        // 表示名はホスト由来の文字列なので textContent で入れる (innerHTML に流さず XSS を防ぐ)。
        displayItems = displays.map((d) => {
          const btn = document.createElement("button");
          btn.type = "button";
          btn.className = "mc-display-item";
          btn.textContent = `${formatDisplayName(d.name)} (${d.width}x${d.height})`;
          btn.addEventListener("click", () => {
            opts.onSelectDisplay(d);
            syncDisplayActive(d.id);
            setDisplayPopVisible(false);
          });
          return { id: d.id, btn };
        });
        displayPop.replaceChildren(...displayItems.map((it) => it.btn));
        syncDisplayActive(selectedId);
      }
      // ディスプレイが複数あるときだけ切替ボタンを出す。
      if (displayBtn) displayBtn.style.display = hasDisplayChoice ? "" : "none";
      if (!hasDisplayChoice) setDisplayPopVisible(false);
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
      const hasZoomReset = Math.abs(scale - 1) > 0.01;
      if (zoomResetBtn) zoomResetBtn.style.display = hasZoomReset ? "" : "none";
    },
    setPosition(pos: ToolbarPosition): void {
      // 配置が変わるとポップオーバーの開く向きも変わるため、開いていれば一旦閉じる。
      setDisplayPopVisible(false);
      if (rootEl) rootEl.dataset.pos = pos;
    },
    setPadOn(on: boolean): void {
      padBtn?.classList.toggle("toggled", on);
    },
  };
}
