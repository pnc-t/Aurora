// リモート画面上に浮かべる半透明のオンスクリーンコントローラ (モバイル専用)。
// タッチ端末では映像を直接タップして操作すると指が映像を隠す・狙った場所を押しにくい・
// 修飾キーが打てないため、カーソル移動/クリック/修飾キーをこのコントローラへ集約する。
// 物理トラックパッド + マウスボタンに寄せて「なぞる場所」と「押すボタン」を別カードに分け、
// それぞれ独立してドラッグ配置できるようにする (パッドは左下、ボタンは右下が既定=両手持ち想定)。
// 表示中は映像への直接タップは別レイヤー (TouchInput.setPointerEnabled) 側で無効化される。

import { type InputEvent } from "../core/ipc";
import { icon } from "./icons";
import "./mobile-pad.css";

export type MobilePadOpts = {
  root: HTMLElement;
  /** 1 入力イベントをホストへ送る (呼び出し側が session_id を内包する)。 */
  sendInput: (event: InputEvent) => void;
  /** VK を 1 つ送る (down/up の組み立てはこのコンポーネント側で行う)。 */
  sendKey: (vk: number, down: boolean) => void;
  /** グリップのドラッグ終了時に、どちらのカードかを区別して画面比率位置 (0〜1) を通知する (永続化用)。 */
  onPositionChange: (which: "pad" | "buttons", pos: [number, number]) => void;
};

export type MobilePadHandle = {
  /** 両カードの表示/非表示を切り替える。false にするときは押下中のボタンと修飾キーを必ず離す。 */
  setVisible: (on: boolean) => void;
  /** 保存済みの比率位置 (0〜1) を反映する。null なら既定位置。範囲外はクランプする。 */
  setPositions: (pad: [number, number] | null, buttons: [number, number] | null) => void;
  /** このコンポーネント以外の経路 (ソフトキーボード) でキーが送られたことを通知する。
   *  短押しスティッキー修飾の自動解除に使う。 */
  notifyExternalKey: () => void;
  /** パッドのスティッキー Shift が有効 (once / locked) かどうか。ソフトキーボード側が
   *  一時 Shift を二重に掛けない判断に使う。 */
  isShiftHeld: () => boolean;
};

type Pt = { x: number; y: number };
type ClickButton = "Left" | "Middle" | "Right";

// VK 値。core/soft-keyboard.ts の VK 定数は private で import できないため、必要な分だけ複製する。
const VK_SHIFT = 0x10;
const VK_CONTROL = 0x11;
const VK_MENU = 0x12; // Alt
const VK_ESCAPE = 0x1b;
const VK_TAB = 0x09;
const VK_RETURN = 0x0d;
const VK_LWIN = 0x5b;
const VK_C = 0x43;
const VK_V = 0x56;

type ModKey = "ctrl" | "alt" | "shift" | "win";
type ModState = "off" | "once" | "locked";

const MOD_KEYS: ModKey[] = ["ctrl", "alt", "shift", "win"];
const MOD_VK: Record<ModKey, number> = { ctrl: VK_CONTROL, alt: VK_MENU, shift: VK_SHIFT, win: VK_LWIN };
const MOD_LABEL: Record<ModKey, string> = { ctrl: "Ctrl", alt: "Alt", shift: "Shift", win: "Win" };

// タップ判定のしきい値 (core/touch-input.ts の TAP_MAX_MS/MOVE_THRESHOLD_PX と同じ値)。
const TAP_MAX_MS = 300;
const MOVE_THRESHOLD_PX = 10;
// 修飾キーの長押しでスティッキーを "locked" に固定するまでの時間。
const MOD_LONG_PRESS_MS = 500;
// 2 本指ホイールの px→ノッチ換算。core/touch-input.ts の WHEEL_PX_PER_NOTCH と揃える。
const WHEEL_PX_PER_NOTCH = 100;
// 既定のグリップ位置 (画面比率)。両手持ち想定でパッドは左下、ボタンは右下に離して置く。
// 下端中央には既存の下部バーのハンドルがあるため、どちらもそこには重ねない。
const PAD_DEFAULT_POS: [number, number] = [0, 0.85];
const BTN_DEFAULT_POS: [number, number] = [1, 0.85];

function clamp(v: number, min: number, max: number): number {
  return Math.min(Math.max(v, min), max);
}

function midpoint(a: Pt, b: Pt): Pt {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

function axisMax(viewport: number, size: number): number {
  return Math.max(0, viewport - size);
}

function clampPos(pos: [number, number] | null, fallback: [number, number]): [number, number] {
  return pos ? [clamp(pos[0], 0, 1), clamp(pos[1], 0, 1)] : fallback;
}

/**
 * 1 枚のカード (パッド or ボタン) の比率位置管理とグリップドラッグを束ねる。
 * 2 枚とも同じ「グリップで掴んで動かし、比率で覚える」仕組みが要るための共通化。
 */
type Card = {
  ratio: [number, number];
  /** 比率位置 (0〜1) を現在のビューポート/カードサイズで px に変換して当てる。 */
  applyRatio: (ratio: [number, number]) => void;
};

function setupCard(
  el: HTMLElement | null,
  gripEl: HTMLButtonElement | null,
  defaultPos: [number, number],
  which: "pad" | "buttons",
  onPositionChange: (which: "pad" | "buttons", pos: [number, number]) => void,
): Card {
  const card: Card = {
    ratio: defaultPos,
    applyRatio(ratio: [number, number]): void {
      card.ratio = ratio;
      if (!el) return;
      const w = el.offsetWidth;
      const h = el.offsetHeight;
      // 非表示中 (mp-hidden = display:none) は寸法が 0 になり、当てても意味のない座標になる。
      // 比率だけ覚えておき、表示された瞬間 (setVisible(true)) に測れる寸法で当て直す。
      if (w === 0 || h === 0) return;
      const vw = window.innerWidth;
      const vh = window.innerHeight;
      const x = clamp(ratio[0] * axisMax(vw, w), 0, axisMax(vw, w));
      const y = clamp(ratio[1] * axisMax(vh, h), 0, axisMax(vh, h));
      el.style.left = `${x}px`;
      el.style.top = `${y}px`;
    },
  };
  card.applyRatio(defaultPos);

  let gripDragId: number | null = null;
  let gripStartTouch: Pt = { x: 0, y: 0 };
  let gripStartPos: Pt = { x: 0, y: 0 };

  gripEl?.addEventListener(
    "touchstart",
    (e) => {
      e.preventDefault();
      if (gripDragId !== null || !el) return;
      const t = e.changedTouches[0];
      gripDragId = t.identifier;
      gripStartTouch = { x: t.clientX, y: t.clientY };
      const rect = el.getBoundingClientRect();
      gripStartPos = { x: rect.left, y: rect.top };
    },
    { passive: false },
  );

  gripEl?.addEventListener(
    "touchmove",
    (e) => {
      e.preventDefault();
      if (!el) return;
      const t = Array.from(e.changedTouches).find((c) => c.identifier === gripDragId);
      if (!t) return;
      const dx = t.clientX - gripStartTouch.x;
      const dy = t.clientY - gripStartTouch.y;
      const vw = window.innerWidth;
      const vh = window.innerHeight;
      const w = el.offsetWidth;
      const h = el.offsetHeight;
      const x = clamp(gripStartPos.x + dx, 0, axisMax(vw, w));
      const y = clamp(gripStartPos.y + dy, 0, axisMax(vh, h));
      el.style.left = `${x}px`;
      el.style.top = `${y}px`;
    },
    { passive: false },
  );

  function endGripDrag(e: TouchEvent): void {
    e.preventDefault();
    if (!el) return;
    const t = Array.from(e.changedTouches).find((c) => c.identifier === gripDragId);
    if (!t) return;
    gripDragId = null;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    const rect = el.getBoundingClientRect();
    const maxX = axisMax(vw, w);
    const maxY = axisMax(vh, h);
    const rx = maxX > 0 ? clamp(rect.left / maxX, 0, 1) : 0;
    const ry = maxY > 0 ? clamp(rect.top / maxY, 0, 1) : 0;
    card.ratio = [rx, ry];
    onPositionChange(which, card.ratio);
  }

  gripEl?.addEventListener("touchend", endGripDrag, { passive: false });
  gripEl?.addEventListener("touchcancel", endGripDrag, { passive: false });

  return card;
}

/** オンスクリーンコントローラをマウントする。 */
export function mountMobilePad(opts: MobilePadOpts): MobilePadHandle {
  opts.root.innerHTML = `
    <div class="mp-pad-card mp-hidden" id="mp-pad-card">
      <button type="button" class="mp-grip" id="mp-pad-grip" aria-label="パッドを移動">${icon("grip")}</button>
      <div class="mp-pad" id="mp-pad"></div>
    </div>
    <div class="mp-btn-card mp-hidden" id="mp-btn-card">
      <button type="button" class="mp-grip" id="mp-btn-grip" aria-label="ボタンを移動">${icon("grip")}</button>
      <div class="mp-btns">
        <button type="button" class="mp-btn" id="mp-l" title="左クリック" aria-label="左クリック">${icon("mouse-left")}</button>
        <button type="button" class="mp-btn" id="mp-m" title="中クリック" aria-label="中クリック">${icon("mouse-middle")}</button>
        <button type="button" class="mp-btn" id="mp-r" title="右クリック" aria-label="右クリック">${icon("mouse-right")}</button>
        <button type="button" class="mp-btn" id="mp-mods-toggle" title="修飾キー" aria-label="修飾キー">${icon("modifier")}</button>
      </div>
      <div class="mp-mods" id="mp-mods">
        <div class="mp-mod-row">
          <button type="button" class="mp-mod-btn" data-mod="ctrl">${MOD_LABEL.ctrl}</button>
          <button type="button" class="mp-mod-btn" data-mod="alt">${MOD_LABEL.alt}</button>
          <button type="button" class="mp-mod-btn" data-mod="shift">${MOD_LABEL.shift}</button>
          <button type="button" class="mp-mod-btn" data-mod="win">${MOD_LABEL.win}</button>
        </div>
        <div class="mp-quick-row">
          <button type="button" class="mp-quick-btn" id="mp-q-copy">Ctrl+C</button>
          <button type="button" class="mp-quick-btn" id="mp-q-paste">Ctrl+V</button>
          <button type="button" class="mp-quick-btn" id="mp-q-alttab">Alt+Tab</button>
          <button type="button" class="mp-quick-btn" id="mp-q-esc">Esc</button>
          <button type="button" class="mp-quick-btn" id="mp-q-enter">Enter</button>
          <button type="button" class="mp-quick-btn" id="mp-q-tab">Tab</button>
        </div>
      </div>
    </div>
  `;

  const padCardEl = opts.root.querySelector<HTMLElement>("#mp-pad-card");
  const padGripEl = opts.root.querySelector<HTMLButtonElement>("#mp-pad-grip");
  const padEl = opts.root.querySelector<HTMLElement>("#mp-pad");
  const btnCardEl = opts.root.querySelector<HTMLElement>("#mp-btn-card");
  const btnGripEl = opts.root.querySelector<HTMLButtonElement>("#mp-btn-grip");
  const lBtn = opts.root.querySelector<HTMLButtonElement>("#mp-l");
  const mBtn = opts.root.querySelector<HTMLButtonElement>("#mp-m");
  const rBtn = opts.root.querySelector<HTMLButtonElement>("#mp-r");
  const modsToggleBtn = opts.root.querySelector<HTMLButtonElement>("#mp-mods-toggle");
  const modsEl = opts.root.querySelector<HTMLElement>("#mp-mods");
  const qCopyBtn = opts.root.querySelector<HTMLButtonElement>("#mp-q-copy");
  const qPasteBtn = opts.root.querySelector<HTMLButtonElement>("#mp-q-paste");
  const qAltTabBtn = opts.root.querySelector<HTMLButtonElement>("#mp-q-alttab");
  const qEscBtn = opts.root.querySelector<HTMLButtonElement>("#mp-q-esc");
  const qEnterBtn = opts.root.querySelector<HTMLButtonElement>("#mp-q-enter");
  const qTabBtn = opts.root.querySelector<HTMLButtonElement>("#mp-q-tab");

  // mousedown の既定動作 (フォーカス移動) を許すと、ソフトキーボード表示中に隠し input が
  // blur してキーボードが意図せず閉じてしまう (mobile-controls.ts と同じ理由)。
  // touchstart は preventDefault しない (Android で合成 click まで消えボタンが反応しなくなる)。
  for (const btn of Array.from(opts.root.querySelectorAll<HTMLButtonElement>("button"))) {
    btn.addEventListener("mousedown", (e) => e.preventDefault());
  }

  // ================= 修飾キー (スティッキー) =================

  const modState: Record<ModKey, ModState> = { ctrl: "off", alt: "off", shift: "off", win: "off" };
  const modButtons = new Map<ModKey, HTMLButtonElement>();
  const modLongPressTimers = new Map<ModKey, number>();
  const modLongPressFired = new Set<ModKey>();

  function setModState(key: ModKey, state: ModState): void {
    modState[key] = state;
    const btn = modButtons.get(key);
    btn?.classList.toggle("mp-once", state === "once");
    btn?.classList.toggle("mp-locked", state === "locked");
  }

  /** "once" 状態の修飾キーを全て消費して off に戻す。L/M/R を離した時・定型キー送出・
   *  パッドのタップ・notifyExternalKey() の直後に呼ぶ。 */
  function consumeOnce(): void {
    for (const key of MOD_KEYS) {
      if (modState[key] === "once") {
        opts.sendKey(MOD_VK[key], false);
        setModState(key, "off");
      }
    }
  }

  function onModPointerDown(key: ModKey): void {
    modLongPressFired.delete(key);
    const startState = modState[key];
    // locked からの長押しは未定義のため開始しない (locked は短押しでのみ off に戻す)。
    if (startState === "locked") return;
    const timer = window.setTimeout(() => {
      modLongPressFired.add(key);
      // "once" は既に down 済みなので送らない。"off" からはここで down を送る。
      if (startState === "off") opts.sendKey(MOD_VK[key], true);
      setModState(key, "locked");
    }, MOD_LONG_PRESS_MS);
    modLongPressTimers.set(key, timer);
  }

  function onModPointerUp(key: ModKey): void {
    const timer = modLongPressTimers.get(key);
    if (timer !== undefined) {
      clearTimeout(timer);
      modLongPressTimers.delete(key);
    }
    if (modLongPressFired.has(key)) {
      // 長押しで既に locked へ確定済み。短押し扱いの二重処理はしない。
      modLongPressFired.delete(key);
      return;
    }
    const cur = modState[key];
    if (cur === "off") {
      opts.sendKey(MOD_VK[key], true);
      setModState(key, "once");
    } else {
      // "once" → off / "locked" → off はどちらも同じ (down 済みのキーを離す)。
      opts.sendKey(MOD_VK[key], false);
      setModState(key, "off");
    }
  }

  function onModPointerCancel(key: ModKey): void {
    const timer = modLongPressTimers.get(key);
    if (timer !== undefined) {
      clearTimeout(timer);
      modLongPressTimers.delete(key);
    }
    modLongPressFired.delete(key);
  }

  for (const key of MOD_KEYS) {
    const btn = opts.root.querySelector<HTMLButtonElement>(`.mp-mod-btn[data-mod="${key}"]`);
    if (!btn) continue;
    modButtons.set(key, btn);
    btn.addEventListener("pointerdown", (e) => {
      // L/M/R と同じ理由: 捕捉しないと指がボタン外へ滑った際に up/cancel が届かず、
      // ホスト側に修飾キーが押しっぱなしのまま残ってしまう。
      btn.setPointerCapture(e.pointerId);
      onModPointerDown(key);
    });
    btn.addEventListener("pointerup", () => onModPointerUp(key));
    btn.addEventListener("pointercancel", () => onModPointerCancel(key));
  }

  // ================= L/M/R クリックボタン =================
  //
  // 物理マウスボタンと同じ「押している間だけ down」に振る舞う。短く押して離せば単発クリック、
  // 素早く 2 回押せばダブルクリック、押さえたままパッドをなぞればドラッグが自然に成立するため、
  // 別途 DBL/ドラッグ専用ボタンは不要。

  const clickReleasers: (() => void)[] = [];

  function attachClickButton(btn: HTMLButtonElement | null, button: ClickButton): void {
    if (!btn) return;
    let pressed = false;
    btn.addEventListener("pointerdown", (e) => {
      pressed = true;
      // 押したまま指がボタンの外へ滑ると pointerup が来ず、押しっぱなしがホスト側に残ってしまう。
      // ポインタをこのボタンへ捕捉し、外れても up を確実に受け取れるようにする。
      btn.setPointerCapture(e.pointerId);
      opts.sendInput({ MouseButton: { button, down: true } });
    });
    const release = (): void => {
      if (!pressed) return;
      pressed = false;
      opts.sendInput({ MouseButton: { button, down: false } });
      consumeOnce();
    };
    btn.addEventListener("pointerup", release);
    btn.addEventListener("pointercancel", release);
    // setVisible(false) での強制解除用。consumeOnce は呼ばない (修飾はそちら側でまとめて処理する)。
    clickReleasers.push(() => {
      if (!pressed) return;
      pressed = false;
      opts.sendInput({ MouseButton: { button, down: false } });
    });
  }

  attachClickButton(lBtn, "Left");
  attachClickButton(mBtn, "Middle");
  attachClickButton(rBtn, "Right");

  let modsOpen = false;
  modsToggleBtn?.addEventListener("click", () => {
    modsOpen = !modsOpen;
    modsEl?.classList.toggle("open", modsOpen);
    modsToggleBtn.classList.toggle("toggled", modsOpen);
    // パネルの開閉で .mp-btn-card の高さが変わるため、同じ比率のまま再クランプしないと
    // 下端がビューポートからはみ出して最下段のボタンが押せなくなる。比率自体は変えない
    // (ユーザーが動かしたわけではないので onPositionChange による永続化も呼ばない)。
    btnCard.applyRatio(btnCard.ratio);
  });

  // ================= 定型キー =================

  function tapKey(vk: number): void {
    opts.sendKey(vk, true);
    opts.sendKey(vk, false);
  }

  /** 複数キーを down は指定順、up は逆順で送る (Ctrl+C 等のコンボ用)。 */
  function comboKey(vks: number[]): void {
    for (const vk of vks) opts.sendKey(vk, true);
    for (let i = vks.length - 1; i >= 0; i--) opts.sendKey(vks[i], false);
  }

  qCopyBtn?.addEventListener("click", () => {
    comboKey([VK_CONTROL, VK_C]);
    consumeOnce();
  });
  qPasteBtn?.addEventListener("click", () => {
    comboKey([VK_CONTROL, VK_V]);
    consumeOnce();
  });
  qAltTabBtn?.addEventListener("click", () => {
    comboKey([VK_MENU, VK_TAB]);
    consumeOnce();
  });
  // Win 単押し (スタートメニュー) の定型キーは置かない。修飾キー行の Win と同じラベルが
  // 2 つ並んで紛らわしいため。単押ししたいときは修飾キー行の Win を 2 回叩く (1 回目で down、
  // 2 回目で up が送られる = down→up と等価)。
  qEscBtn?.addEventListener("click", () => {
    tapKey(VK_ESCAPE);
    consumeOnce();
  });
  qEnterBtn?.addEventListener("click", () => {
    tapKey(VK_RETURN);
    consumeOnce();
  });
  qTabBtn?.addEventListener("click", () => {
    tapKey(VK_TAB);
    consumeOnce();
  });

  // ================= トラックパッド (なぞる場所) =================
  //
  // タッチイベントは passive: false で登録し、全ハンドラで preventDefault する。
  // 合成マウスイベント・ブラウザのスクロール/ピンチがジェスチャを奪うのを防ぐため
  // (core/touch-input.ts と同じ理由)。

  const padTouches = new Map<number, Pt>();
  let singleActive = false;
  let singleId: number | null = null;
  let singleStart: Pt = { x: 0, y: 0 };
  let singleLast: Pt = { x: 0, y: 0 };
  let singleStartTime = 0;
  let singleMoved = false;

  let multiActive = false;
  let multiIds: [number, number] | null = null;
  let multiLastMid: Pt = { x: 0, y: 0 };
  let wheelAccX = 0;
  let wheelAccY = 0;

  function abortSingle(): void {
    singleActive = false;
    singleId = null;
    singleMoved = false;
  }

  function updateWheel(mid: Pt): void {
    const dxStep = mid.x - multiLastMid.x;
    const dyStep = mid.y - multiLastMid.y;
    if (dxStep === 0 && dyStep === 0) return;
    // 縦優先。core/touch-input.ts の updateWheel と同じ規則: 横移動が縦の2倍を超えるときだけ
    // 横スクロールとして扱い、100px=1ノッチで蓄積する。横だけ core 側の deltaX 符号規約
    // (指を右へ動かす=deltaX 的には負) に合わせて反転し、縦は既に反転済みの規約と一致するため反転しない。
    const horizontal = Math.abs(dxStep) > Math.abs(dyStep) * 2;
    if (horizontal) {
      wheelAccX += -dxStep;
      const ticks = Math.trunc(wheelAccX / WHEEL_PX_PER_NOTCH);
      if (ticks === 0) return;
      wheelAccX -= ticks * WHEEL_PX_PER_NOTCH;
      opts.sendInput({ MouseWheel: { delta: ticks * 120, horizontal: true } });
    } else {
      wheelAccY += dyStep;
      const ticks = Math.trunc(wheelAccY / WHEEL_PX_PER_NOTCH);
      if (ticks === 0) return;
      wheelAccY -= ticks * WHEEL_PX_PER_NOTCH;
      opts.sendInput({ MouseWheel: { delta: ticks * 120, horizontal: false } });
    }
  }

  padEl?.addEventListener(
    "touchstart",
    (e) => {
      e.preventDefault();
      for (const t of Array.from(e.changedTouches)) {
        padTouches.set(t.identifier, { x: t.clientX, y: t.clientY });
      }
      if (padTouches.size === 1 && !multiActive) {
        for (const [id, pt] of padTouches) {
          singleActive = true;
          singleId = id;
          singleStart = pt;
          singleLast = pt;
          singleStartTime = performance.now();
          singleMoved = false;
        }
      } else if (padTouches.size === 2) {
        // 1本指操作の途中で2本目が触れたら、タップ/クリックを誤発火させないよう打ち切る。
        abortSingle();
        const ids = Array.from(padTouches.keys()) as [number, number];
        multiActive = true;
        multiIds = ids;
        const p1 = padTouches.get(ids[0]);
        const p2 = padTouches.get(ids[1]);
        multiLastMid = p1 && p2 ? midpoint(p1, p2) : { x: 0, y: 0 };
        wheelAccX = 0;
        wheelAccY = 0;
      }
      // 3本目以降は無視 (ジェスチャ対象を増やさない)。
    },
    { passive: false },
  );

  padEl?.addEventListener(
    "touchmove",
    (e) => {
      e.preventDefault();
      for (const t of Array.from(e.changedTouches)) {
        if (padTouches.has(t.identifier)) padTouches.set(t.identifier, { x: t.clientX, y: t.clientY });
      }
      if (multiActive && multiIds) {
        const p1 = padTouches.get(multiIds[0]);
        const p2 = padTouches.get(multiIds[1]);
        if (!p1 || !p2) return;
        const mid = midpoint(p1, p2);
        updateWheel(mid);
        multiLastMid = mid;
      } else if (singleActive && singleId !== null) {
        const cur = padTouches.get(singleId);
        if (!cur) return;
        const dx = Math.round(cur.x - singleLast.x);
        const dy = Math.round(cur.y - singleLast.y);
        if (dx !== 0 || dy !== 0) {
          opts.sendInput({ MouseMoveRelative: { dx, dy } });
        }
        singleLast = cur;
        if (Math.hypot(cur.x - singleStart.x, cur.y - singleStart.y) >= MOVE_THRESHOLD_PX) {
          singleMoved = true;
        }
      }
    },
    { passive: false },
  );

  function onPadTouchEnd(e: TouchEvent, cancelled: boolean): void {
    e.preventDefault();
    const endedIds = Array.from(e.changedTouches).map((t) => t.identifier);
    for (const id of endedIds) padTouches.delete(id);

    if (multiActive && multiIds) {
      if (endedIds.includes(multiIds[0]) || endedIds.includes(multiIds[1])) {
        multiActive = false;
        multiIds = null;
        wheelAccX = 0;
        wheelAccY = 0;
      }
    } else if (singleActive && singleId !== null && endedIds.includes(singleId)) {
      const duration = performance.now() - singleStartTime;
      // 移動量 10px 未満かつ 300ms 未満で離した場合だけタップ=左クリックとして扱う。
      if (!cancelled && !singleMoved && duration < TAP_MAX_MS) {
        opts.sendInput({ MouseButton: { button: "Left", down: true } });
        opts.sendInput({ MouseButton: { button: "Left", down: false } });
        consumeOnce();
      }
      abortSingle();
    }
  }

  padEl?.addEventListener("touchend", (e) => onPadTouchEnd(e, false), { passive: false });
  padEl?.addEventListener("touchcancel", (e) => onPadTouchEnd(e, true), { passive: false });

  // ================= カード (パッド/ボタン) の配置 =================

  const padCard = setupCard(padCardEl, padGripEl, PAD_DEFAULT_POS, "pad", opts.onPositionChange);
  const btnCard = setupCard(btnCardEl, btnGripEl, BTN_DEFAULT_POS, "buttons", opts.onPositionChange);

  // 画面回転/リサイズ後に両カードの比率位置を再計算し当て直す (回転後に画面外へ出るのを防ぐ)。
  // mount 時に 1 回だけ購読する。
  window.addEventListener("resize", () => {
    padCard.applyRatio(padCard.ratio);
    btnCard.applyRatio(btnCard.ratio);
  });

  return {
    setVisible(on: boolean): void {
      padCardEl?.classList.toggle("mp-hidden", !on);
      btnCardEl?.classList.toggle("mp-hidden", !on);
      if (on) {
        // mp-hidden を外して寸法が測れるようになった直後に当て直す (非表示中の
        // applyRatio/setPositions は寸法 0 で空振りしているため、ここで確定させる)。
        padCard.applyRatio(padCard.ratio);
        btnCard.applyRatio(btnCard.ratio);
        return;
      }
      // 押しっぱなしが残ると実害が出るため、非表示にする瞬間に必ず解除する。
      for (const release of clickReleasers) release();
      for (const key of MOD_KEYS) {
        if (modState[key] !== "off") {
          opts.sendKey(MOD_VK[key], false);
          setModState(key, "off");
        }
      }
    },
    setPositions(pad: [number, number] | null, buttons: [number, number] | null): void {
      padCard.applyRatio(clampPos(pad, PAD_DEFAULT_POS));
      btnCard.applyRatio(clampPos(buttons, BTN_DEFAULT_POS));
    },
    notifyExternalKey(): void {
      consumeOnce();
    },
    isShiftHeld(): boolean {
      return modState.shift !== "off";
    },
  };
}
