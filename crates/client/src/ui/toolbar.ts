// 接続中の操作ツールバー。
// 既定で常時表示 (上端の透明な帯に、角丸+影のピルを 1 枚浮かせて中央配置するフローティング型)。
// Ctrl+Alt+Shift+B で一時的に隠せる。
// 構成 (ピル内): [切断] | [全画面/キャプチャ/ファイル/統計] [画面切替] | [プリセット(+カスタム時のみ fps/Mbps)] | [pin] | [min/close]
// 各操作はフラットなアイコンボタン + ツールチップ。ピル本体 (.tb-pill) と透明な帯 (#topbar)、
// 構造スパンに data-tauri-drag-region を付け、ボタン/入力/select 以外の余白・隔て・帯の余白どこを
// 掴んでもウィンドウ移動できる。
// 接続先情報は省スペース化のためバーの title (ホバー tooltip) に退避し、行内には常駐させない。

import { formatDisplayName, type DisplayDto, type ToolbarPosition } from "../core/ipc";
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
  onToggleMute: (muted: boolean) => void;
  onSetVideoParams: (fps: number, mbps: number) => void;
  /** 送出する映像の最大高さ (0 = フル解像度)。 */
  onSetResolution: (maxHeight: number) => void;
};

export type ToolbarHandle = {
  setConnLabel: (text: string) => void;
  setDisplays: (displays: DisplayDto[], selectedId?: number) => void;
  setStatsToggled: (on: boolean) => void;
  setFileToggled: (on: boolean) => void;
  setFullscreenToggled: (on: boolean) => void;
  setCaptureToggled: (on: boolean) => void;
  /** ホストが音声対応かをツールバーに伝える (非対応ならミュートボタンを隠す)。 */
  setAudioAvailable: (available: boolean) => void;
  setVideoParams: (fps: number, mbps: number) => void;
  toggleBar: () => void;
  /** ツールバーの配置を切り替える (top/bottom = 横帯、left/right = 縦サイドバー)。 */
  setPosition: (pos: ToolbarPosition) => void;
  /** 接続状態を反映する。pinnedDefault でこのセッションの初期ピン留めを決める。 */
  setConnected: (connected: boolean, pinnedDefault?: boolean) => void;
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
        <button type="button" id="tb-audio" class="tb-btn" title="音声ミュート">${icon("volume")}</button>
      </div>
      <div id="display-wrap" class="tb-popgroup" data-pop="display" title="表示するディスプレイ" data-tauri-drag-region>
        <button type="button" class="tb-btn tb-pop-trigger" id="tb-display-trigger" title="ディスプレイ">${icon("display")}</button>
        <span class="tb-pop-panel" data-tauri-drag-region>
          <span class="tb-ico" data-tauri-drag-region>${icon("display")}</span>
          <select id="display-select"></select>
        </span>
      </div>
      <span class="tb-sep" data-tauri-drag-region></span>
      <div class="tb-popgroup" data-pop="quality" data-tauri-drag-region>
        <button type="button" class="tb-btn tb-pop-trigger" id="tb-quality-trigger" title="画質 / 解像度">${icon("tune")}</button>
        <span class="tb-video tb-pop-panel" title="画質プリセット / 送出 fps 上限 (0=無制限) / ビットレート上限" data-tauri-drag-region>
          <select id="tb-preset" title="画質プリセット">${presetOptionsHtml()}</select>
          <span id="tb-custom" class="tb-custom">
            <input type="text" inputmode="numeric" id="tb-fps" />fps
            <input type="text" inputmode="numeric" id="tb-mbps" />Mbps
          </span>
          <select id="tb-resolution" title="解像度 (送出する映像の高さ・アスペクト比維持)">
            <option value="0">フル</option>
            <option value="1080">1080p</option>
            <option value="720">720p</option>
          </select>
        </span>
      </div>
      <span class="tb-sep" data-tauri-drag-region></span>
      <button type="button" id="tb-pin" class="tb-btn" title="バーをピン留め (常時表示) / Ctrl+Alt+Shift+B">${icon("pin")}</button>
      <button type="button" id="tb-hide" class="tb-btn" title="バーを隠す (端にカーソルを合わせても出さない) / Ctrl+Alt+Shift+B で再表示">${icon("eye-off")}</button>
      <span id="tb-winctl" class="window-controls"></span>
    </div>
  `;
  // 注: 以前は帯 (#topbar) 全体を data-tauri-drag-region にしていたが、全幅の帯がポインタを
  // 食い、上端でリモート操作ができず (クリックがウィンドウ移動になる) という不具合になった。
  // 帯は CSS で pointer-events:none にして素通しさせ、ドラッグ移動はピル (.tb-pill 内の
  // data-tauri-drag-region 余白) でのみ行う。よってここで #topbar には付けない。

  // フレームレスのため接続中もウィンドウ操作 (min/close) をバーに持つ。
  const winctl = opts.topbar.querySelector<HTMLElement>("#tb-winctl");
  if (winctl) mountWindowControls(winctl);

  const disconnectBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-disconnect");
  const displayWrap = opts.topbar.querySelector<HTMLElement>("#display-wrap");
  const displaySelect = opts.topbar.querySelector<HTMLSelectElement>("#display-select");
  const filesBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-files");
  const statsBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-stats");
  const audioBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-audio");
  const captureBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-capture");
  const fullscreenBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-fullscreen");
  const pinBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-pin");
  const hideBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-hide");
  const presetSelect = opts.topbar.querySelector<HTMLSelectElement>("#tb-preset");
  const resolutionSelect = opts.topbar.querySelector<HTMLSelectElement>("#tb-resolution");
  const customWrap = opts.topbar.querySelector<HTMLElement>("#tb-custom");
  const fpsInput = opts.topbar.querySelector<HTMLInputElement>("#tb-fps");
  const mbpsInput = opts.topbar.querySelector<HTMLInputElement>("#tb-mbps");

  // 縦バー (left/right) 時のポップオーバー。テキストを持つコントロール (画質/解像度/
  // ディスプレイ) はバーを横長にするため、縦配置ではアイコン (.tb-pop-trigger) に畳み、
  // クリックでバー脇にパネル (.tb-pop-panel) を開く。横帯 (top/bottom) ではトリガを隠して
  // パネルをそのままインライン表示する (見た目は従来どおり・CSS 側で出し分け)。
  const popgroups = Array.from(opts.topbar.querySelectorAll<HTMLElement>(".tb-popgroup"));
  function closePopovers(except?: HTMLElement): void {
    for (const g of popgroups) if (g !== except) g.classList.remove("pop-open");
  }
  for (const g of popgroups) {
    const trigger = g.querySelector<HTMLElement>(".tb-pop-trigger");
    trigger?.addEventListener("click", (e) => {
      e.stopPropagation();
      const willOpen = !g.classList.contains("pop-open");
      closePopovers(g);
      g.classList.toggle("pop-open", willOpen);
    });
  }
  // パネル外をクリックしたら全て閉じる (パネル内の select 操作では閉じない)。
  document.addEventListener("pointerdown", (e) => {
    if (!(e.target as HTMLElement).closest(".tb-popgroup")) closePopovers();
  });

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
  // 音声ミュート状態。ホストが音声非対応ならボタン自体を隠す (audioAvailable=false)。
  let muted = false;
  let audioAvailable = false;
  // ミュートボタンは既定で非表示。host_info で音声対応が分かったら出す。
  if (audioBtn) audioBtn.style.display = "none";

  function applyAudioBtn(): void {
    if (!audioBtn) return;
    audioBtn.style.display = audioAvailable ? "inline-flex" : "none";
    audioBtn.classList.toggle("toggled", muted);
    audioBtn.title = muted ? "音声ミュート解除" : "音声ミュート";
    audioBtn.innerHTML = icon(muted ? "mute" : "volume");
  }
  // ピン留め (常時表示)。既定は false = 自動格納し、端ホバーで一時表示する。
  let pinned = false;
  // 端ホバーで一時表示中か (ピン留め時は無関係)。
  let revealed = false;
  // 自動表示の無効化 (隠すボタン)。true の間は端にカーソルを合わせても出さない。
  // 再表示はピン留め (Ctrl+Alt+Shift+B もしくは pin ボタン) のみ。
  let autoHideLocked = false;
  // ツールバーの配置。CSS の位置/向きクラスはこの値から付け替える。
  let position: ToolbarPosition = "top";
  // 既定の配置クラス (pos-top) を最初から付けておく (setPosition 前でも CSS が効くように)。
  // 注: applyPosition は position を参照するので、必ず position 初期化後に呼ぶ
  // (関数巻き上げに頼って宣言前に呼ぶと TDZ で ReferenceError になる)。
  applyPosition();

  // 配置クラスを #topbar と body に反映する。left/right は縦並び (vertical)。
  function applyPosition(): void {
    const cls = `pos-${position}`;
    opts.topbar.classList.remove("pos-top", "pos-bottom", "pos-left", "pos-right", "vertical");
    opts.topbar.classList.add(cls);
    if (position === "left" || position === "right") opts.topbar.classList.add("vertical");
    document.body.classList.remove("bar-top", "bar-bottom", "bar-left", "bar-right");
    document.body.classList.add(`bar-${position}`);
  }

  // バーの「厚み」。top/bottom は高さ、left/right は幅で端ホバー判定する。
  function barExtent(): number {
    if (position === "left" || position === "right") return opts.topbar.offsetWidth || 56;
    return opts.topbar.offsetHeight || 48;
  }

  function applyVisibility(): void {
    const visible = connected && (pinned || revealed);
    opts.topbar.classList.toggle("visible", visible);
    if (visible) {
      // 実測したバー高を CSS 変数へ (統計 HUD を top 配置のときこの分だけ下げる)。
      document.documentElement.style.setProperty("--topbar-h", `${opts.topbar.offsetHeight}px`);
    }
    pinBtn?.classList.toggle("toggled", pinned);
    hideBtn?.classList.toggle("toggled", autoHideLocked);
    // bar-open: バー表示中は統計 HUD などをバーと重ならない位置へ寄せる
    // (ピン留め・端ホバー表示のどちらでも同じ挙動)。
    document.body.classList.toggle("bar-open", visible);
  }

  function setPinned(on: boolean): void {
    pinned = on;
    if (pinned) {
      // ピン留めするなら自動表示の無効化は解除し、その場で出す。
      autoHideLocked = false;
      revealed = false;
    }
    applyVisibility();
  }

  // 「隠す」ボタン: ピン留めを外し、自動表示も無効化して即座に格納する。
  function setAutoHideLocked(on: boolean): void {
    autoHideLocked = on;
    if (on) {
      pinned = false;
      revealed = false;
    }
    applyVisibility();
  }

  // 配置に応じた端付近にマウスがある間だけ表示する (座標で判定)。
  // mouseleave ではなく座標判定にすることで、バーを掴んでウィンドウをドラッグ移動する
  // 間 (カーソルは端に留まる) もバーが消えない。ネイティブドラッグ中は mousemove が
  // 来なくても revealed は維持される。
  window.addEventListener("mousemove", (e) => {
    if (!connected || pinned || autoHideLocked) return;
    const ext = barExtent() + 6;
    let want: boolean;
    switch (position) {
      case "bottom":
        want = e.clientY >= window.innerHeight - ext;
        break;
      case "left":
        want = e.clientX <= ext;
        break;
      case "right":
        want = e.clientX >= window.innerWidth - ext;
        break;
      default:
        want = e.clientY <= ext;
    }
    if (want !== revealed) {
      revealed = want;
      applyVisibility();
    }
  });

  disconnectBtn?.addEventListener("click", () => opts.onDisconnect());
  filesBtn?.addEventListener("click", () => opts.onToggleFilePanel());
  statsBtn?.addEventListener("click", () => opts.onToggleStats());
  audioBtn?.addEventListener("click", () => {
    muted = !muted;
    applyAudioBtn();
    opts.onToggleMute(muted);
  });
  captureBtn?.addEventListener("click", () => opts.onToggleCapture());
  resolutionSelect?.addEventListener("change", () => {
    opts.onSetResolution(Math.max(0, Math.floor(Number(resolutionSelect.value) || 0)));
  });
  fullscreenBtn?.addEventListener("click", () => opts.onToggleFullscreen());
  pinBtn?.addEventListener("click", () => setPinned(!pinned));
  hideBtn?.addEventListener("click", () => setAutoHideLocked(!autoHideLocked));
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
    setAudioAvailable(available: boolean): void {
      audioAvailable = available;
      // 新規接続のたびにミュートは解除状態から始める (前セッションの状態を持ち越さない)。
      muted = false;
      applyAudioBtn();
    },
    setVideoParams(fps: number, mbps: number): void {
      if (fpsInput) fpsInput.value = String(fps);
      if (mbpsInput) mbpsInput.value = String(mbps);
      syncPreset(fps, mbps);
    },
    setPosition(pos: ToolbarPosition): void {
      position = pos;
      // 配置が変わるとポップオーバーの開き方向 (左右) も変わるため一旦閉じる。
      closePopovers();
      applyPosition();
      applyVisibility();
    },
    /** Ctrl+Alt+Shift+B: ピン留めのトグル (常時表示 ⇔ 自動格納)。
     *  自動表示を無効化中ならまず再表示できるようにピン留めする。 */
    toggleBar(): void {
      if (!connected) return;
      setPinned(!pinned);
    },
    setConnected(isConnected: boolean, pinnedDefault = false): void {
      connected = isConnected;
      // 新規接続のたびに解像度はフルから始める (ホスト capture スレッドは
      // セッションをまたいで設定を保持するため、UI と確実に揃える)。
      if (isConnected && resolutionSelect) resolutionSelect.value = "0";
      // 接続のたびに設定の既定ピン留めから始める (未ピンなら自動格納・端ホバーで一時表示)。
      pinned = isConnected && pinnedDefault;
      autoHideLocked = false;
      revealed = false;
      applyVisibility();
    },
  };
}
