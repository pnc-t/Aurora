// リモート画面上に浮かべる半透明のオンスクリーンコントローラ (モバイル専用)。
// タッチ端末では映像を直接タップして操作すると指が映像を隠す・狙った場所を押しにくいため、
// カーソル移動/クリックをこのコントローラへ集約する。修飾キー/ファンクションキー/矢印は
// 画面下部固定の操作用キーボード (ui/mobile-keyboard.ts) 側に集約済みのため、このコントローラの
// 責務はカーソル移動 + クリック + 操作用キーボードの開閉に限る。
// 物理トラックパッド + マウスボタンに寄せて「なぞる場所」と「押すボタン」を別カードに分け、
// それぞれ独立してドラッグ配置できるようにする (パッドは左下、ボタンは右下が既定=両手持ち想定)。
// 表示中は映像への直接タップは別レイヤー (TouchInput.setPointerEnabled) 側で無効化される。

import { type InputEvent } from "../core/ipc";
import { type StickyModifiers } from "../core/sticky-modifiers";
import { icon } from "./icons";
import "./mobile-pad.css";

export type MobilePadOpts = {
  root: HTMLElement;
  /** 1 入力イベントをホストへ送る (呼び出し側が session_id を内包する)。 */
  sendInput: (event: InputEvent) => void;
  /** グリップのドラッグ終了時に、どちらのカードかを区別して画面比率位置 (0〜1) を通知する (永続化用)。 */
  onPositionChange: (which: "pad" | "buttons", pos: [number, number]) => void;
  /** 修飾キーの共有ストア。自前キーボード (ui/mobile-keyboard.ts) と状態を共有するため、
   *  このコンポーネントは状態を自前で持たない。 */
  mods: StickyModifiers;
  /** 「修飾キー」ボタンが押された。操作用キーボード (ui/mobile-keyboard.ts) の開閉に使う。 */
  onToggleKeyboard: () => void;
};

export type MobilePadHandle = {
  /** 両カードの表示/非表示を切り替える。false にするときは押下中のボタンと修飾キーを必ず離す。 */
  setVisible: (on: boolean) => void;
  /** 保存済みの比率位置 (0〜1) を反映する。null なら既定位置。範囲外はクランプする。 */
  setPositions: (pad: [number, number] | null, buttons: [number, number] | null) => void;
  /** 操作用キーボードの開閉状態をボタンの見た目へ反映する。 */
  setKeyboardOn: (on: boolean) => void;
};

type Pt = { x: number; y: number };
type ClickButton = "Left" | "Middle" | "Right";

// タップ判定のしきい値 (core/touch-input.ts の TAP_MAX_MS/MOVE_THRESHOLD_PX と同じ値)。
// スティックのタップと 2 本指タップ (右クリック) の判定に使う。
const TAP_MAX_MS = 300;
const MOVE_THRESHOLD_PX = 10;
// パッド面の 1 本指タップだけは上記より緩い値を使う。実機評価で「タップしても最初のタッチが
// 反応せずタップ&ドラッグに入れない」となったため。指を置いて離すまでは 300ms を超えやすく、
// 小さなパッド面では指も 10px 程度は滑るので、そのままだとクリックも送られず「直前のタップ」
// としても記録されず (= タップ&ドラッグの起点が永久に作れない) という二重の失敗になっていた。
const PAD_TAP_MAX_MS = 450;
const PAD_TAP_MOVE_MAX_PX = 14;
// タップ&ドラッグ (laptop のタッチパッドと同じ「タップ直後に触れっぱなしでドラッグ」) の判定は
// 「1回目のタップを離してから2回目に触れ直すまでの猶予」だけで行う。Windows の精密タッチパッドが
// ダブルクリック速度 (既定 500ms) でこれを判定していることに揃えた値。
// **位置のしきい値は意図的に持たない**: 物理タッチパッドは 2 回目の接地がパッド上のどこでも
// よく、同じ場所を要求するのは画面を直接触るダブルタップ (core/touch-input.ts の
// DBLTAP_MAX_MOVE_PX) の作法。実機の指の動きを CDP で実測したところ、2 回目の接地は 1 回目から
// 31〜34px ずれる試行が多く、30px 制限では半数が落ちていた (幅 164px のパッドで 30px 以内は
// 厳しすぎる)。時間だけで判定すれば「タップしてから普通の速さで触れ直す」が確実に成立する。
const TAPDRAG_GAP_MAX_MS = 500;
// 2 本指ホイールの px→ノッチ換算。core/touch-input.ts の WHEEL_PX_PER_NOTCH と揃える。
const WHEEL_PX_PER_NOTCH = 100;
// スティックモードの土台半径に対する不感帯の比率。指を置いただけで動き出さないようにする。
const STICK_DEADZONE_RATIO = 0.12;
// スティック最大速度 [px/秒] (デッドゾーン外の正規化量が 1 のとき)。傾き量に比例 (線形)。
// 以前は指数 1.8 の加速カーブだったが、実機で外周付近の急加速が狙った位置で止められず
// 使いにくいとの評価だったため線形にした (最大速度も 1400→1000 に下げて全体を落ち着かせた)。
const STICK_MAX_SPEED = 1000;
// パッド面の余白 (px)。スティック半径をこの分だけ内側に取り、土台が枠に接しないようにする。
const STICK_EDGE_MARGIN = 6;

type PadMode = "pad" | "stick";
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

/**
 * カード配置に使える高さ。操作用キーボード (ui/mobile-keyboard.ts) が開いている間はその分を除く。
 * 除かないとカードがキーボードの裏に回り込み、「修飾キーを押しながらパッドでクリックする」という
 * 併用そのものができなくなる。高さは ui/mobile-keyboard.ts が `--mkb-height` として出している。
 */
function usableHeight(): number {
  const raw = getComputedStyle(document.documentElement).getPropertyValue("--mkb-height");
  return Math.max(0, window.innerHeight - (Number.parseFloat(raw) || 0));
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
      const vh = usableHeight();
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
      const vh = usableHeight();
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
    const vh = usableHeight();
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
      <div class="mp-pad" id="mp-pad">
        <div class="mp-stick-base mp-stick-hidden" id="mp-stick-base"></div>
        <div class="mp-stick-knob mp-stick-hidden" id="mp-stick-knob"></div>
      </div>
      <div class="mp-pad-actions">
        <button type="button" class="mp-btn" id="mp-mode" title="スティックに切替" aria-label="スティックに切替">${icon("touchpad")}</button>
        <button type="button" class="mp-btn" id="mp-keyboard" title="操作キーボード" aria-label="操作キーボード">${icon("modifier")}</button>
      </div>
    </div>
    <div class="mp-btn-card mp-hidden" id="mp-btn-card">
      <button type="button" class="mp-grip" id="mp-btn-grip" aria-label="ボタンを移動">${icon("grip")}</button>
      <div class="mp-btns">
        <button type="button" class="mp-btn" id="mp-l" title="左クリック" aria-label="左クリック">${icon("mouse-left")}</button>
        <button type="button" class="mp-btn" id="mp-m" title="中クリック" aria-label="中クリック">${icon("mouse-middle")}</button>
        <button type="button" class="mp-btn" id="mp-r" title="右クリック" aria-label="右クリック">${icon("mouse-right")}</button>
      </div>
    </div>
  `;

  const padCardEl = opts.root.querySelector<HTMLElement>("#mp-pad-card");
  const padGripEl = opts.root.querySelector<HTMLButtonElement>("#mp-pad-grip");
  const padEl = opts.root.querySelector<HTMLElement>("#mp-pad");
  const stickBaseEl = opts.root.querySelector<HTMLElement>("#mp-stick-base");
  const stickKnobEl = opts.root.querySelector<HTMLElement>("#mp-stick-knob");
  const modeBtn = opts.root.querySelector<HTMLButtonElement>("#mp-mode");
  const keyboardBtn = opts.root.querySelector<HTMLButtonElement>("#mp-keyboard");
  const btnCardEl = opts.root.querySelector<HTMLElement>("#mp-btn-card");
  const btnGripEl = opts.root.querySelector<HTMLButtonElement>("#mp-btn-grip");
  const lBtn = opts.root.querySelector<HTMLButtonElement>("#mp-l");
  const mBtn = opts.root.querySelector<HTMLButtonElement>("#mp-m");
  const rBtn = opts.root.querySelector<HTMLButtonElement>("#mp-r");

  // mousedown の既定動作 (フォーカス移動) を止め、タップ後にフォーカスリングが残らないようにする。
  // touchstart は preventDefault しない (Android で合成 click まで消えボタンが反応しなくなる)。
  for (const btn of Array.from(opts.root.querySelectorAll<HTMLButtonElement>("button"))) {
    btn.addEventListener("mousedown", (e) => e.preventDefault());
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
      // once 状態の修飾キーはこのクリックへ適用して消費する (Ctrl+クリック等の組み合わせを
      // 成立させるため)。locked はここでは消費しない (StickyModifiers.consumeOnce の仕様)。
      opts.mods.consumeOnce();
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

  keyboardBtn?.addEventListener("click", () => opts.onToggleKeyboard());

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
  // タップ&ドラッグ (トラックパッドモードのみ): 直前のタップから TAPDRAG_GAP_MAX_MS 以内に
  // 触れ直したら、その場で左ボタンを押して離すまで保持する。位置は見ない (定数のコメント参照)。
  let tapDragActive = false;
  let lastPadTapTime = 0;

  let multiActive = false;
  let multiIds: [number, number] | null = null;
  let multiStartTime = 0;
  let multiStartMid: Pt = { x: 0, y: 0 };
  let multiLastMid: Pt = { x: 0, y: 0 };
  let multiMoved = false;
  let wheelAccX = 0;
  let wheelAccY = 0;

  /** タップドラッグ中に保持している左ボタンを強制的に離す (consumeOnce は呼ばない。
   *  clickReleasers と同じ理由: 押しっぱなし解消が目的で、通常のクリック確定ではないため)。 */
  function releaseTapDrag(): void {
    if (!tapDragActive) return;
    tapDragActive = false;
    opts.sendInput({ MouseButton: { button: "Left", down: false } });
  }

  function abortSingle(): void {
    releaseTapDrag();
    singleActive = false;
    singleId = null;
    singleMoved = false;
  }

  // ================= スティックモード (傾けている間ずっと動き続ける) =================
  //
  // 固定中心方式: パッド面中央に土台+ノブを常設し、指の位置をノブへ反映しつつ、
  // 中心からの距離に応じた速度で rAF ループが移動量を送り続ける。トラックパッドの
  // 「なぞった分だけ動く」とは別の入力系統のため、状態も分けて持つ。

  let padMode: PadMode = "pad";
  let stickTouchId: number | null = null;
  let stickCenter: Pt = { x: 0, y: 0 };
  let stickRadius = 0;
  let stickStartTime = 0;
  let stickExceededDeadzone = false;
  let stickVX = 0;
  let stickVY = 0;
  let stickAccX = 0;
  let stickAccY = 0;
  let stickRafId: number | null = null;
  let stickLastFrameTime = 0;

  /** パッド面の実寸から土台/ノブの半径・中心を再計算し、DOM の位置/サイズへ反映する。 */
  function layoutStick(): void {
    if (!padEl || !stickBaseEl || !stickKnobEl) return;
    const w = padEl.clientWidth;
    const h = padEl.clientHeight;
    // 非表示中は寸法が 0 になり無意味な値になるため、表示された時点 (setVisible/resize) で
    // 呼び直される前提でここでは何もしない。
    if (w === 0 || h === 0) return;
    stickRadius = Math.max(0, Math.min(w, h) / 2 - STICK_EDGE_MARGIN);
    stickCenter = { x: w / 2, y: h / 2 };
    const baseSize = stickRadius * 2;
    stickBaseEl.style.width = `${baseSize}px`;
    stickBaseEl.style.height = `${baseSize}px`;
    stickBaseEl.style.left = `${stickCenter.x - stickRadius}px`;
    stickBaseEl.style.top = `${stickCenter.y - stickRadius}px`;
    const knobSize = Math.max(16, stickRadius * 0.5);
    stickKnobEl.style.width = `${knobSize}px`;
    stickKnobEl.style.height = `${knobSize}px`;
    stickKnobEl.style.left = `${stickCenter.x - knobSize / 2}px`;
    stickKnobEl.style.top = `${stickCenter.y - knobSize / 2}px`;
  }

  function setStickKnobOffset(dx: number, dy: number): void {
    if (!stickKnobEl) return;
    stickKnobEl.style.transform = `translate(${dx}px, ${dy}px)`;
  }

  function stopStickLoop(): void {
    if (stickRafId !== null) {
      cancelAnimationFrame(stickRafId);
      stickRafId = null;
    }
  }

  function startStickLoop(): void {
    stopStickLoop();
    stickLastFrameTime = performance.now();
    const step = (now: number): void => {
      const dt = (now - stickLastFrameTime) / 1000;
      stickLastFrameTime = now;
      if (stickVX !== 0 || stickVY !== 0) {
        // 小数のまま累積し、整数分だけ切り出して送る。切り捨てたままにすると、ゆっくり
        // 傾けたときの速度 (< 1px/フレーム) が永久に送信されなくなるため。
        stickAccX += stickVX * dt;
        stickAccY += stickVY * dt;
        const dx = Math.trunc(stickAccX);
        const dy = Math.trunc(stickAccY);
        if (dx !== 0 || dy !== 0) {
          stickAccX -= dx;
          stickAccY -= dy;
          opts.sendInput({ MouseMoveRelative: { dx, dy } });
        }
      }
      stickRafId = requestAnimationFrame(step);
    };
    stickRafId = requestAnimationFrame(step);
  }

  function startStick(id: number): void {
    stickTouchId = id;
    stickStartTime = performance.now();
    stickExceededDeadzone = false;
    stickVX = 0;
    stickVY = 0;
    stickAccX = 0;
    stickAccY = 0;
    layoutStick();
    setStickKnobOffset(0, 0);
    startStickLoop();
  }

  function updateStickFromTouch(cur: Pt): void {
    if (!padEl) return;
    const rect = padEl.getBoundingClientRect();
    const rawDx = cur.x - rect.left - stickCenter.x;
    const rawDy = cur.y - rect.top - stickCenter.y;
    const dist = Math.hypot(rawDx, rawDy);
    const clampedDist = Math.min(dist, stickRadius);
    const ux = dist > 0 ? rawDx / dist : 0;
    const uy = dist > 0 ? rawDy / dist : 0;
    setStickKnobOffset(ux * clampedDist, uy * clampedDist);

    const deadzone = STICK_DEADZONE_RATIO * stickRadius;
    if (dist >= deadzone) stickExceededDeadzone = true;

    const range = stickRadius - deadzone;
    const n = range > 0 ? clamp((clampedDist - deadzone) / range, 0, 1) : 0;
    if (n <= 0) {
      stickVX = 0;
      stickVY = 0;
      return;
    }
    const speed = STICK_MAX_SPEED * n;
    stickVX = ux * speed;
    stickVY = uy * speed;
  }

  /** 2本目の指が触れた等、タップ判定なしでスティック操作を打ち切る。 */
  function abortStickTouch(): void {
    stopStickLoop();
    stickTouchId = null;
    stickVX = 0;
    stickVY = 0;
    stickAccX = 0;
    stickAccY = 0;
    setStickKnobOffset(0, 0);
  }

  function endStick(cancelled: boolean): void {
    stopStickLoop();
    const duration = performance.now() - stickStartTime;
    // トラックパッドのタップ判定と同じ扱い: デッドゾーンを超えず短時間で離したときだけ左クリック。
    if (!cancelled && !stickExceededDeadzone && duration < TAP_MAX_MS) {
      opts.sendInput({ MouseButton: { button: "Left", down: true } });
      opts.sendInput({ MouseButton: { button: "Left", down: false } });
      opts.mods.consumeOnce();
    }
    stickTouchId = null;
    stickVX = 0;
    stickVY = 0;
    stickAccX = 0;
    stickAccY = 0;
    setStickKnobOffset(0, 0);
  }

  function updateModeButton(): void {
    if (!modeBtn) return;
    if (padMode === "pad") {
      modeBtn.innerHTML = icon("touchpad");
      modeBtn.title = "スティックに切替";
      modeBtn.setAttribute("aria-label", "スティックに切替");
    } else {
      modeBtn.innerHTML = icon("stick");
      modeBtn.title = "タッチパッドに切替";
      modeBtn.setAttribute("aria-label", "タッチパッドに切替");
    }
  }

  function setPadMode(mode: PadMode): void {
    if (mode === padMode) return;
    // 進行中のジェスチャを残したままモードを切り替えると、押しっぱなし相当の入力が
    // 新モードに引き継がれてカーソルが暴走するため、両モードの状態を必ず中断してから切り替える。
    abortSingle();
    if (multiActive) {
      multiActive = false;
      multiIds = null;
      wheelAccX = 0;
      wheelAccY = 0;
    }
    abortStickTouch();
    padTouches.clear();
    padMode = mode;
    stickBaseEl?.classList.toggle("mp-stick-hidden", mode !== "stick");
    stickKnobEl?.classList.toggle("mp-stick-hidden", mode !== "stick");
    if (mode === "stick") layoutStick();
    updateModeButton();
  }

  modeBtn?.addEventListener("click", () => setPadMode(padMode === "pad" ? "stick" : "pad"));

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
          if (padMode === "stick") {
            startStick(id);
          } else {
            const now = performance.now();
            // 直前のタップから TAPDRAG_GAP_MAX_MS 以内の touchstart なら、触れた場所を問わず
            // タップ&ドラッグ (laptop のタッチパッドと同じ)。その場で左ボタンを押し、離すまで
            // 保持する。3回目以降が連鎖してドラッグにならないよう判定に使ったら即クリアする
            // (core/touch-input.ts の dblDrag 判定と同じ考え方)。
            const isTapDrag = lastPadTapTime > 0 && now - lastPadTapTime <= TAPDRAG_GAP_MAX_MS;
            singleActive = true;
            singleId = id;
            singleStart = pt;
            singleLast = pt;
            singleStartTime = now;
            singleMoved = false;
            if (isTapDrag) {
              lastPadTapTime = 0;
              tapDragActive = true;
              opts.sendInput({ MouseButton: { button: "Left", down: true } });
            } else {
              tapDragActive = false;
            }
          }
        }
      } else if (padTouches.size === 2) {
        // 1本指操作の途中で2本目が触れたら、タップ/クリックを誤発火させないよう打ち切る
        // (タップドラッグ中なら押しっぱなしの左ボタンも abortSingle 内で必ず離す)。
        if (padMode === "stick") abortStickTouch();
        else abortSingle();
        const ids = Array.from(padTouches.keys()) as [number, number];
        multiActive = true;
        multiIds = ids;
        const p1 = padTouches.get(ids[0]);
        const p2 = padTouches.get(ids[1]);
        const mid = p1 && p2 ? midpoint(p1, p2) : { x: 0, y: 0 };
        multiStartMid = mid;
        multiLastMid = mid;
        multiStartTime = performance.now();
        multiMoved = false;
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
        // 中点が MOVE_THRESHOLD_PX 以上動いたら、ホイールへ確定したジェスチャとして扱い
        // 2本指タップ=右クリック (onPadTouchEnd) を送らせない (core/touch-input.ts の
        // endMulti/multiMoved と同じ判定規則)。
        const midMoved = Math.hypot(mid.x - multiStartMid.x, mid.y - multiStartMid.y);
        if (!multiMoved && midMoved >= MOVE_THRESHOLD_PX) {
          multiMoved = true;
        }
        updateWheel(mid);
        multiLastMid = mid;
      } else if (padMode === "stick" && stickTouchId !== null) {
        const cur = padTouches.get(stickTouchId);
        if (!cur) return;
        updateStickFromTouch(cur);
      } else if (singleActive && singleId !== null) {
        const cur = padTouches.get(singleId);
        if (!cur) return;
        const dx = Math.round(cur.x - singleLast.x);
        const dy = Math.round(cur.y - singleLast.y);
        if (dx !== 0 || dy !== 0) {
          opts.sendInput({ MouseMoveRelative: { dx, dy } });
        }
        singleLast = cur;
        if (Math.hypot(cur.x - singleStart.x, cur.y - singleStart.y) >= PAD_TAP_MOVE_MAX_PX) {
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
        const duration = performance.now() - multiStartTime;
        // 2本指タップ=右クリック。トラックパッドモードのみ (スティックモードでは仕様どおり
        // 有効にしない)。ホイールへ確定済み (multiMoved) のジェスチャでは送らない。
        if (padMode === "pad" && !cancelled && !multiMoved && duration < TAP_MAX_MS) {
          opts.sendInput({ MouseButton: { button: "Right", down: true } });
          opts.sendInput({ MouseButton: { button: "Right", down: false } });
          opts.mods.consumeOnce();
        }
        multiActive = false;
        multiIds = null;
        wheelAccX = 0;
        wheelAccY = 0;
      }
    } else if (padMode === "stick" && stickTouchId !== null && endedIds.includes(stickTouchId)) {
      endStick(cancelled);
    } else if (singleActive && singleId !== null && endedIds.includes(singleId)) {
      if (tapDragActive) {
        // タップドラッグ中に指を離した (touchcancel でも): 保持していた左ボタンを必ず離す。
        tapDragActive = false;
        opts.sendInput({ MouseButton: { button: "Left", down: false } });
        opts.mods.consumeOnce();
      } else {
        const duration = performance.now() - singleStartTime;
        // 移動量 PAD_TAP_MOVE_MAX_PX 未満かつ PAD_TAP_MAX_MS 未満で離した場合だけタップ=
        // 左クリックとして扱う。
        if (!cancelled && !singleMoved && duration < PAD_TAP_MAX_MS) {
          opts.sendInput({ MouseButton: { button: "Left", down: true } });
          opts.sendInput({ MouseButton: { button: "Left", down: false } });
          // L/M/R ボタンの release と同じ理由: once 状態の修飾キーをこのタップへ適用して消費する。
          opts.mods.consumeOnce();
          // タップ&ドラッグ判定用に、このタップの終了時刻を憶えておく。ドラッグとして消費した
          // 直後 (isTapDrag 成立時) は 3 回目の連鎖を防ぐため上の touchstart 側で既にクリア済み、
          // ここに来るのは純粋な単発タップのときだけ。
          lastPadTapTime = performance.now();
        }
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
    layoutStick();
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
        layoutStick();
        return;
      }
      // 押しっぱなしが残ると実害が出るため、非表示にする瞬間に必ず解除する。
      // スティックの rAF ループも、非表示のまま回り続けないようここで必ず止める。
      abortStickTouch();
      abortSingle(); // タップドラッグ中の左ボタン保持もここで必ず離す。
      for (const release of clickReleasers) release();
      opts.mods.releaseAll();
    },
    setPositions(pad: [number, number] | null, buttons: [number, number] | null): void {
      padCard.applyRatio(clampPos(pad, PAD_DEFAULT_POS));
      btnCard.applyRatio(clampPos(buttons, BTN_DEFAULT_POS));
    },
    setKeyboardOn(on: boolean): void {
      keyboardBtn?.classList.toggle("toggled", on);
      // 開閉で配置可能な高さ (usableHeight) が変わるため、同じ比率のまま置き直してカードを
      // キーボードの上へ逃がす。ユーザーが動かしたわけではないので比率自体は変えず、
      // onPositionChange による永続化も呼ばない。
      padCard.applyRatio(padCard.ratio);
      btnCard.applyRatio(btnCard.ratio);
    },
  };
}
