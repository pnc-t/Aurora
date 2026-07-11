// 接続中の操作ツールバー。
// 既定で常時表示 (上端の透明な帯に、角丸+影のピルを 1 枚浮かせて中央配置するフローティング型)。
// Ctrl+Alt+Shift+B で一時的に隠せる。
// 構成 (ピル内): [切断] | [全画面/キャプチャ/ファイル/統計] [画面切替▼] | [画質/解像度▼] | [pin] | [min/close]
// ピルはアイコンボタンのみの一定幅。テキストを持つコントロール (画面切替/画質/解像度) は
// どの配置でもアイコンをクリックするとバー脇のポップオーバーに開く。
// 各操作はフラットなアイコンボタン + ツールチップ。ピル本体 (.tb-pill) と透明な帯 (#topbar)、
// 構造スパンに data-tauri-drag-region を付け、ボタン/入力/select 以外の余白・隔て・帯の余白どこを
// 掴んでもウィンドウ移動できる。
// 接続先情報は省スペース化のためバーの title (ホバー tooltip) に退避し、行内には常駐させない。

import { formatDisplayName, type DisplayDto, type ToolbarPosition } from "../core/ipc";
import { CUSTOM_PRESET_ID, VIDEO_PRESETS, matchPreset } from "../core/video-presets";
import { mountWindowControls } from "./window-controls";
import { applyAriaLabels } from "../core/a11y";
import { shortcutLabel } from "../core/shortcuts";
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
  /**
   * ホストが音声対応かをツールバーに伝える (非対応ならミュートボタンを隠す)。
   * initialMuted で接続直後のミュート初期状態を渡す (settings.audio_muted。既定 false)。
   */
  setAudioAvailable: (available: boolean, initialMuted?: boolean) => void;
  setVideoParams: (fps: number, mbps: number) => void;
  toggleBar: () => void;
  /** ツールバーを確実に表示する (脱出時など)。ピン留めして自動格納されないようにする。 */
  revealBar: () => void;
  /** ツールバーの配置を切り替える (top/bottom = 横帯、left/right = 縦サイドバー)。 */
  setPosition: (pos: ToolbarPosition) => void;
  /** 接続状態を反映する。pinnedDefault でこのセッションの初期ピン留めを決める。 */
  setConnected: (connected: boolean, pinnedDefault?: boolean) => void;
};

// メニュー内の選択肢 1 行。ラベルはホスト由来文字列 (ディスプレイ名) があり得るため
// textContent で入れる (innerHTML 経由の XSS を避ける)。チェック表示は aria-checked に一元化する。
function makeMenuItem(label: string, onPick: () => void): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "tb-menu-item";
  btn.setAttribute("role", "menuitemradio");
  btn.setAttribute("aria-checked", "false");
  const check = document.createElement("span");
  check.className = "tb-menu-check";
  check.innerHTML = icon("check"); // 固定 SVG のみなので innerHTML で問題ない
  const labelSpan = document.createElement("span");
  labelSpan.className = "tb-menu-label";
  labelSpan.textContent = label;
  btn.append(check, labelSpan);
  btn.addEventListener("click", onPick);
  return btn;
}

export function mountToolbar(opts: ToolbarOpts): ToolbarHandle {
  opts.topbar.innerHTML = `
    <div class="tb-pill" data-tauri-drag-region>
      <div class="tb-group" data-tauri-drag-region>
        <button type="button" id="tb-disconnect" class="tb-btn danger" title="切断">${icon("power")}</button>
      </div>
      <span class="tb-sep" data-tauri-drag-region></span>
      <div class="tb-group" data-tauri-drag-region>
        <button type="button" id="tb-fullscreen" class="tb-btn" title="フルスクリーン (${shortcutLabel("toggle-fullscreen")})">${icon("fullscreen")}</button>
        <button type="button" id="tb-capture" class="tb-btn" title="マウスキャプチャ (${shortcutLabel("toggle-capture")})">${icon("mouse")}</button>
        <button type="button" id="tb-files" class="tb-btn" title="ファイル転送">${icon("files")}</button>
        <button type="button" id="tb-stats" class="tb-btn" title="統計 HUD (${shortcutLabel("toggle-stats")})">${icon("stats")}</button>
        <button type="button" id="tb-audio" class="tb-btn" title="音声ミュート">${icon("volume")}</button>
      </div>
      <div id="display-wrap" class="tb-popgroup" data-pop="display" title="表示するディスプレイ" data-tauri-drag-region>
        <button type="button" class="tb-btn tb-pop-trigger" id="tb-display-trigger" title="ディスプレイ">${icon("display")}</button>
        <div class="tb-pop-panel tb-menu" id="tb-display-menu" role="menu"></div>
      </div>
      <span class="tb-sep" data-tauri-drag-region></span>
      <div class="tb-popgroup" data-pop="quality" data-tauri-drag-region>
        <button type="button" class="tb-btn tb-pop-trigger" id="tb-quality-trigger" title="画質">${icon("tune")}</button>
        <div class="tb-pop-panel tb-menu" role="menu">
          <div id="tb-quality-items"></div>
          <div id="tb-custom" class="tb-custom" title="送出 fps 上限 (0=無制限) / ビットレート上限">
            <input type="text" inputmode="numeric" id="tb-fps" />fps
            <input type="text" inputmode="numeric" id="tb-mbps" />Mbps
          </div>
        </div>
      </div>
      <div class="tb-popgroup" data-pop="resolution">
        <button type="button" class="tb-btn tb-pop-trigger" id="tb-resolution-trigger" title="解像度 (送出する映像の高さ・アスペクト比維持)">${icon("resolution")}</button>
        <div class="tb-pop-panel tb-menu" id="tb-resolution-menu" role="menu"></div>
      </div>
      <span class="tb-sep" data-tauri-drag-region></span>
      <button type="button" id="tb-pin" class="tb-btn" title="バーをピン留め (常時表示) / ${shortcutLabel("toggle-bar")}">${icon("pin")}</button>
      <button type="button" id="tb-hide" class="tb-btn" title="バーを隠す (端にカーソルを合わせても出さない) / ${shortcutLabel("toggle-bar")} で再表示">${icon("eye-off")}</button>
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

  // アイコンのみのボタン群に aria-label を補う (SR 読み上げ用)。
  applyAriaLabels(opts.topbar);

  const disconnectBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-disconnect");
  const displayWrap = opts.topbar.querySelector<HTMLElement>("#display-wrap");
  const displayMenu = opts.topbar.querySelector<HTMLElement>("#tb-display-menu");
  const filesBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-files");
  const statsBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-stats");
  const audioBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-audio");
  const captureBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-capture");
  const fullscreenBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-fullscreen");
  const pinBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-pin");
  const hideBtn = opts.topbar.querySelector<HTMLButtonElement>("#tb-hide");
  const qualityItemsWrap = opts.topbar.querySelector<HTMLElement>("#tb-quality-items");
  const resolutionMenu = opts.topbar.querySelector<HTMLElement>("#tb-resolution-menu");
  const customWrap = opts.topbar.querySelector<HTMLElement>("#tb-custom");
  const fpsInput = opts.topbar.querySelector<HTMLInputElement>("#tb-fps");
  const mbpsInput = opts.topbar.querySelector<HTMLInputElement>("#tb-mbps");

  // ポップオーバー。テキストを持つコントロール (画質/解像度/ディスプレイ) はピルを
  // 長くするため、全配置でアイコン (.tb-pop-trigger) に畳み、クリックでバー脇に
  // パネル (.tb-pop-panel) を開く (開く向きは CSS が配置ごとに決める)。パネル自体が
  // 選択肢メニューなので、開けば一段で選べる (中間の select 展開は不要)。
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
  // パネル外をクリックしたら全て閉じる (パネル内のメニュー項目クリックでは各 pick が閉じる)。
  document.addEventListener("pointerdown", (e) => {
    if (!(e.target as HTMLElement).closest(".tb-popgroup")) closePopovers();
  });

  // 画質メニュー: プリセット項目 + 末尾カスタム。現在値は aria-checked で一元管理する。
  const qualityItems: { id: string; btn: HTMLButtonElement }[] = [];
  function syncQualityChecks(activeId: string): void {
    for (const it of qualityItems) it.btn.setAttribute("aria-checked", String(it.id === activeId));
  }
  // fps/Mbps の手動入力欄はプリセット=カスタムのときだけ出す (常時表示は情報重複で幅を食う)。
  function applyCustomVisibility(presetId: string): void {
    if (customWrap) customWrap.style.display = presetId === CUSTOM_PRESET_ID ? "flex" : "none";
  }
  function syncPreset(fps: number, mbps: number): void {
    const id = matchPreset(fps, mbps);
    syncQualityChecks(id);
    applyCustomVisibility(id);
  }
  if (qualityItemsWrap) {
    for (const p of VIDEO_PRESETS) {
      const btn = makeMenuItem(p.label, () => {
        if (fpsInput) fpsInput.value = String(p.fps);
        if (mbpsInput) mbpsInput.value = String(p.mbps);
        opts.onSetVideoParams(p.fps, p.mbps);
        syncPreset(p.fps, p.mbps);
        closePopovers();
      });
      qualityItemsWrap.appendChild(btn);
      qualityItems.push({ id: p.id, btn });
    }
    // カスタム行はメニューを閉じない: 適用は下の fps/Mbps 入力の change (emitVideoParams) が担う。
    const customBtn = makeMenuItem("カスタム", () => {
      syncQualityChecks(CUSTOM_PRESET_ID);
      applyCustomVisibility(CUSTOM_PRESET_ID);
    });
    qualityItemsWrap.appendChild(customBtn);
    qualityItems.push({ id: CUSTOM_PRESET_ID, btn: customBtn });
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

  // 解像度メニュー: 固定 3 択。現在値は mountToolbar スコープの変数で保持する。
  let resolutionValue = 0;
  const resolutionItems: { value: number; btn: HTMLButtonElement }[] = [];
  function syncResolutionChecks(): void {
    for (const it of resolutionItems) it.btn.setAttribute("aria-checked", String(it.value === resolutionValue));
  }
  if (resolutionMenu) {
    const options: { value: number; label: string }[] = [
      { value: 0, label: "フル" },
      { value: 1080, label: "1080p" },
      { value: 720, label: "720p" },
    ];
    for (const r of options) {
      const btn = makeMenuItem(r.label, () => {
        resolutionValue = r.value;
        opts.onSetResolution(r.value);
        syncResolutionChecks();
        closePopovers();
      });
      resolutionMenu.appendChild(btn);
      resolutionItems.push({ value: r.value, btn });
    }
    syncResolutionChecks();
  }

  // ディスプレイメニュー: setDisplays() のたびに項目を作り直す (ホスト一覧は接続毎に変わる)。
  let displayItems: { id: number; btn: HTMLButtonElement }[] = [];
  function syncDisplayChecks(activeId: number | undefined): void {
    for (const it of displayItems) it.btn.setAttribute("aria-checked", String(it.id === activeId));
  }

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

  // 端ホバー判定: カーソルが「ウィンドウ端〜帯の内側の縁」の間にあるか。
  // 帯はタブストリップと同じ端に来たとき CSS でその厚み分 inset される (tabs.css) ため、
  // 固定の厚みでなく帯の実位置 (offsetTop/offsetLeft = transform 非適用のレイアウト位置)
  // から判定する。これで格納中でも表示時と同じ領域で反応する。
  function nearBarEdge(e: MouseEvent): boolean {
    const tb = opts.topbar;
    switch (position) {
      case "bottom":
        return e.clientY >= tb.offsetTop - 6;
      case "left":
        return e.clientX <= tb.offsetLeft + tb.offsetWidth + 6;
      case "right":
        return e.clientX >= tb.offsetLeft - 6;
      default:
        return e.clientY <= tb.offsetTop + tb.offsetHeight + 6;
    }
  }

  function applyVisibility(): void {
    const visible = connected && (pinned || revealed);
    // バーが視界から消えるとき (自動格納/隠すボタン/ホットキー/再接続) は開いている
    // ポップオーバーも必ず畳む。パネルは帯にぶら下がる絶対配置なので、開いたまま帯だけ
    // スライドするとパネルがタブストリップの下からはみ出して浮いて見える。
    if (!visible) closePopovers();
    opts.topbar.classList.toggle("visible", visible);
    if (visible) {
      // 「上端からバー下端まで」を CSS 変数へ (統計 HUD を top 配置のときこの分だけ下げる)。
      // タブストリップと同じ端では帯が inset される (offsetTop > 0) ため、高さでなく
      // 下端位置で渡さないと HUD がピルに重なる。
      const tb = opts.topbar;
      document.documentElement.style.setProperty("--topbar-h", `${tb.offsetTop + tb.offsetHeight}px`);
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
    const want = nearBarEdge(e);
    // ポップオーバー (画質/ディスプレイ) はバーの外側に開くため、そこへカーソルを
    // 移すと端ホバー域を外れる。開いている間は畳まない (閉じれば次の mousemove で
    // 通常判定に戻る)。これが無いとパネル操作中にバーごと格納される。
    if (!want && popgroups.some((g) => g.classList.contains("pop-open"))) return;
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
  fullscreenBtn?.addEventListener("click", () => opts.onToggleFullscreen());
  pinBtn?.addEventListener("click", () => setPinned(!pinned));
  hideBtn?.addEventListener("click", () => setAutoHideLocked(!autoHideLocked));

  return {
    setConnLabel(text: string): void {
      // 行内に常駐させず、バーのホバー tooltip として接続先情報を保持する。
      opts.topbar.title = text;
    },
    setDisplays(list: DisplayDto[], selectedId?: number): void {
      if (!displayMenu || !displayWrap) return;
      // 表示名はホスト由来 (リモート) の文字列なので innerHTML に流さず makeMenuItem の
      // textContent 経由で組んで XSS を防ぐ (悪意あるホストが Display.name に仕込む経路を塞ぐ)。
      displayItems = list.map((x) => {
        const label = `${formatDisplayName(x.name)} (${x.width}x${x.height})`;
        const btn = makeMenuItem(label, () => {
          opts.onSelectDisplay(x);
          syncDisplayChecks(x.id);
          closePopovers();
        });
        return { id: x.id, btn };
      });
      displayMenu.replaceChildren(...displayItems.map((it) => it.btn));
      syncDisplayChecks(selectedId);
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
    setAudioAvailable(available: boolean, initialMuted = false): void {
      audioAvailable = available;
      // 新規接続のたびに設定の初期ミュート状態から始める (前セッションの状態を持ち越さない)。
      // 実際のミュート適用 (ホストへの set_audio_muted 送出) は session 側が既に行っている
      // ため、ここは表示 (ボタンの見た目) だけを揃える。
      muted = initialMuted;
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
    /** 脱出時に呼ぶ。トグルでなく確実に表示させたいのでピン留めする (未接続なら無視)。 */
    revealBar(): void {
      if (!connected) return;
      setPinned(true);
    },
    setConnected(isConnected: boolean, pinnedDefault = false): void {
      connected = isConnected;
      // 新規接続のたびに解像度はフルから始める (ホスト capture スレッドは
      // セッションをまたいで設定を保持するため、UI と確実に揃える)。
      if (isConnected) {
        resolutionValue = 0;
        syncResolutionChecks();
      }
      // 接続のたびに設定の既定ピン留めから始める (未ピンなら自動格納・端ホバーで一時表示)。
      pinned = isConnected && pinnedDefault;
      autoHideLocked = false;
      revealed = false;
      applyVisibility();
    },
  };
}
