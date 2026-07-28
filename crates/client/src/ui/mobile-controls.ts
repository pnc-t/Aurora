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
  /** 音声ボタンのタップ (ミュート切替)。引数は切替後の状態 (デスクトップ toolbar.ts と同じ規約)。 */
  onToggleMute: (muted: boolean) => void;
  /** 音量ポップオーバーのスライダー操作 (0.0〜1.0)。 */
  onSetVolume: (volume: number) => void;
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
  /** ホストが音声対応かどうか (非対応ならボタン自体を隠す) と接続直後のミュート初期状態。 */
  setAudioAvailable: (available: boolean, initialMuted?: boolean) => void;
  /** 音量ポップオーバーのスライダー位置を設定値へ揃える (onSetVolume は発火させない)。 */
  setAudioVolume: (volume: number) => void;
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
          <button type="button" class="mc-btn" id="mc-keyboard" title="キーボード">${icon("keyboard")}</button>
          <button type="button" class="mc-btn" id="mc-pad" title="コントローラ">${icon("mouse")}</button>
          <button type="button" class="mc-btn" id="mc-display" title="ディスプレイ" style="display:none">${icon("display")}</button>
          <button type="button" class="mc-btn" id="mc-audio" title="音声ミュート (長押しで音量)" style="display:none">${icon("volume")}</button>
          <button type="button" class="mc-btn" id="mc-zoom-reset" title="ズームをリセット" style="display:none">${icon("zoom-reset")}</button>
        </div>
      </div>
    </div>
    <div class="mc-display-pop" id="mc-display-pop"></div>
    <div class="mc-audio-pop" id="mc-audio-pop">
      <input type="range" id="mc-volume" min="0" max="100" step="1" value="100" aria-label="再生音量" />
    </div>
  `;

  const handleBtn = opts.root.querySelector<HTMLButtonElement>("#mc-handle");
  const rootEl = opts.root.querySelector<HTMLElement>("#mc-root");
  const disconnectBtn = opts.root.querySelector<HTMLButtonElement>("#mc-disconnect");
  const keyboardBtn = opts.root.querySelector<HTMLButtonElement>("#mc-keyboard");
  const padBtn = opts.root.querySelector<HTMLButtonElement>("#mc-pad");
  const displayBtn = opts.root.querySelector<HTMLButtonElement>("#mc-display");
  const displayPop = opts.root.querySelector<HTMLElement>("#mc-display-pop");
  const zoomResetBtn = opts.root.querySelector<HTMLButtonElement>("#mc-zoom-reset");
  const audioBtn = opts.root.querySelector<HTMLButtonElement>("#mc-audio");
  const audioPop = opts.root.querySelector<HTMLElement>("#mc-audio-pop");
  const volumeInput = opts.root.querySelector<HTMLInputElement>("#mc-volume");

  // --- 引き出し / 自動格納 ---
  //
  // 「バー以外をタップしたら閉じる」方式は採らない。タッチ入力は直接タップ式 (タップした座標へ
  // カーソルを移動してクリック) に固定されているため、閉じるつもりのタップがそのままホストへの
  // クリックとして送られてしまう。閉じる手段はハンドルの再タップと無操作時の自動格納だけにする。
  let expanded = false;
  let hideTimer: number | null = null;
  let displayPopVisible = false;
  let audioPopVisible = false;

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
    // ディスプレイ選択/音量ポップオーバーを開いている間は操作の途中なので、勝手に消さない。
    if (displayPopVisible || audioPopVisible) return;
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

  let audioAvailable = false;
  // ミュート状態はこの UI が持ち、タップのたびに反転させて opts.onToggleMute(muted) へ渡す
  // (デスクトップ toolbar.ts と同じパターン)。実際のホストへの適用は呼び出し元が行う。
  let muted = false;
  function applyAudioBtn(): void {
    if (!audioBtn) return;
    audioBtn.classList.toggle("toggled", muted);
    audioBtn.innerHTML = icon(muted ? "mute" : "volume");
  }
  function setAudioPopVisible(on: boolean): void {
    if (on && !audioAvailable) return;
    audioPopVisible = on;
    audioPop?.classList.toggle("visible", on);
    scheduleAutoHide();
  }

  function setExpanded(on: boolean): void {
    expanded = on;
    rootEl?.classList.toggle("expanded", on);
    handleBtn?.classList.toggle("toggled", on);
    // 格納時は展開中のサブパネルも畳む (次に引き出したとき素の状態から始める)。
    if (!on) {
      setDisplayPopVisible(false);
      setAudioPopVisible(false);
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

  // 音声ボタン: タップ=ミュート切替 / 長押し=音量ポップオーバー表示。長押しが成立した click は
  // ミュート切替を発火させない (長押し終了時に指を離すことで発生する click を打ち消す)。
  const AUDIO_LONG_PRESS_MS = 450;
  let audioPressTimer: number | null = null;
  let audioLongPressed = false;
  function cancelAudioPressTimer(): void {
    if (audioPressTimer !== null) {
      clearTimeout(audioPressTimer);
      audioPressTimer = null;
    }
  }
  audioBtn?.addEventListener("mousedown", (e) => e.preventDefault());
  audioBtn?.addEventListener("pointerdown", () => {
    if (!audioAvailable) return;
    audioLongPressed = false;
    audioPressTimer = window.setTimeout(() => {
      audioPressTimer = null;
      audioLongPressed = true;
      setAudioPopVisible(true);
    }, AUDIO_LONG_PRESS_MS);
  });
  audioBtn?.addEventListener("pointerup", cancelAudioPressTimer);
  audioBtn?.addEventListener("pointercancel", cancelAudioPressTimer);
  audioBtn?.addEventListener("click", () => {
    if (audioLongPressed) {
      // 長押しで開いた直後の click は消費するだけ (ミュートは切り替えない)。
      audioLongPressed = false;
      return;
    }
    if (audioPopVisible) {
      // ポップオーバーが開いている間のタップは閉じる操作として扱う。
      setAudioPopVisible(false);
      return;
    }
    muted = !muted;
    applyAudioBtn();
    opts.onToggleMute(muted);
    scheduleAutoHide();
  });
  volumeInput?.addEventListener("input", () => {
    const v = Math.max(0, Math.min(100, Number(volumeInput.value) || 0));
    opts.onSetVolume(v / 100);
    scheduleAutoHide();
  });

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
      setAudioPopVisible(false);
      if (rootEl) rootEl.dataset.pos = pos;
    },
    setPadOn(on: boolean): void {
      padBtn?.classList.toggle("toggled", on);
    },
    setAudioAvailable(available: boolean, initialMuted = false): void {
      audioAvailable = available;
      if (audioBtn) audioBtn.style.display = available ? "" : "none";
      if (!available) setAudioPopVisible(false);
      muted = initialMuted;
      applyAudioBtn();
    },
    setAudioVolume(volume: number): void {
      // 接続時の初期化用。onSetVolume は発火させない (無限ループ防止、デスクトップ toolbar.ts と同じ)。
      if (volumeInput) volumeInput.value = String(Math.round(volume * 100));
    },
  };
}
