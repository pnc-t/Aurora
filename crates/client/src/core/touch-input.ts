// タッチ操作 (Android 等) を InputEvent に変換するブリッジ。既存の Input (core/input.ts,
// マウス/キーボード用) とは別クラスとして併存させ、同じ canvas に両方 attach される想定。
// touchend 後に WebView が合成マウスイベント (mousedown/mousemove/mouseup) を発火するため、
// 全ハンドラで e.preventDefault() して抑止しないと Input 側と二重送信になる。

import { type InputEvent } from "./ipc";

export type TouchMode = "trackpad" | "direct";

type DisplaySize = { width: number; height: number };

type Pt = { x: number; y: number };

export type TouchInputOpts = {
  /** 1 入力イベントをホストへ送る (呼び出し側が session_id を内包する)。 */
  sendInput: (event: InputEvent) => void;
  /** ズーム倍率が変わったとき UI へ通知する (省略可)。 */
  onZoomChange?: (scale: number) => void;
};

// タップ/長押し判定のしきい値。
const TAP_MAX_MS = 300;
const LONG_PRESS_MS = 500;
const MOVE_THRESHOLD_PX = 10;
// トラックパッド/ホイールの px→ノッチ換算。core/input.ts の WHEEL_PX_PER_NOTCH と揃える。
const WHEEL_PX_PER_NOTCH = 100;
// 2 本指ジェスチャの種別判定 (ズーム/ホイール) を確定させる最小移動量。
const GESTURE_DECIDE_PX = 10;
// 指間距離の変化率がこれを超えたらズームと判定する (それ以外はホイール)。
const ZOOM_RATE_THRESHOLD = 0.15;
const ZOOM_MIN = 1.0;
const ZOOM_MAX = 4.0;
// ズーム中にキャンバスが画面から完全に消えないよう残す最低オーバーラップ (px)。
const MIN_OVERLAP_PX = 40;

type ZoomBase = {
  scale: number;
  dist: number;
  // アンカー点 (2 本指の中点) の「変形前のキャンバス内ローカル座標」。
  anchorLocalX: number;
  anchorLocalY: number;
  staticLeft: number;
  staticTop: number;
  staticWidth: number;
  staticHeight: number;
};

/** タッチ操作をホストへの入力イベントに変換する。1 canvas = 1 インスタンス。 */
export class TouchInput {
  private canvas: HTMLCanvasElement | null = null;
  private display: DisplaySize | null = null;
  private mode: TouchMode = "trackpad";

  // 現在触れている指の位置 (identifier → client 座標)。
  private readonly touches = new Map<number, Pt>();
  // 2 本指ジェスチャの後、残り 1 本が単独ジェスチャとして誤発火しないようにする抑止フラグ。
  private suppressSingle = false;

  // --- 1 本指ジェスチャの状態 ---
  private singleActive = false;
  private singleStart: Pt = { x: 0, y: 0 };
  private singleLast: Pt = { x: 0, y: 0 };
  private singleStartTime = 0;
  private singleMoved = false;
  private singleButtonDown = false;
  private singleLongPressTimer: number | null = null;

  // --- 2 本指ジェスチャの状態 ---
  private multiActive = false;
  private multiIds: [number, number] | null = null;
  private multiStartMid: Pt = { x: 0, y: 0 };
  private multiStartDist = 0;
  private multiStartTime = 0;
  private multiDecided: "none" | "zoom" | "wheel" = "none";
  private multiMoved = false;
  private multiLastMid: Pt = { x: 0, y: 0 };
  private wheelAccX = 0;
  private wheelAccY = 0;
  private zoomBase: ZoomBase | null = null;

  // --- ズーム変形 (持続状態。resetZoom() まで維持) ---
  private zoomScale = 1;
  private zoomTx = 0;
  private zoomTy = 0;

  private readonly hStart = (e: TouchEvent) => this.onTouchStart(e);
  private readonly hMove = (e: TouchEvent) => this.onTouchMove(e);
  private readonly hEnd = (e: TouchEvent) => this.onTouchEnd(e);
  private readonly hCancel = (e: TouchEvent) => this.onTouchEnd(e, true);

  constructor(private readonly opts: TouchInputOpts) {}

  /** canvas へタッチハンドラを取り付ける。display はホストのディスプレイ解像度。 */
  attach(canvas: HTMLCanvasElement, display: DisplaySize): void {
    this.detach();
    this.canvas = canvas;
    this.display = display;
    // passive: false 必須。preventDefault で合成マウスイベント/スクロール/ピンチのブラウザ既定動作を止める。
    canvas.addEventListener("touchstart", this.hStart, { passive: false });
    canvas.addEventListener("touchmove", this.hMove, { passive: false });
    canvas.addEventListener("touchend", this.hEnd, { passive: false });
    canvas.addEventListener("touchcancel", this.hCancel, { passive: false });
  }

  detach(): void {
    const canvas = this.canvas;
    if (canvas) {
      canvas.removeEventListener("touchstart", this.hStart);
      canvas.removeEventListener("touchmove", this.hMove);
      canvas.removeEventListener("touchend", this.hEnd);
      canvas.removeEventListener("touchcancel", this.hCancel);
    }
    this.abortGestures();
    // 前回接続のズーム状態を canvas に残さない (再接続時に古い transform が見えてしまうため)。
    // UI 側は既に片付いている可能性があるので onZoomChange は発火しない。
    this.clearZoom(false);
    this.touches.clear();
    this.canvas = null;
    this.display = null;
  }

  /** ディスプレイ切替時に解像度を更新する。 */
  setDisplay(display: DisplaySize): void {
    this.display = display;
  }

  setMode(mode: TouchMode): void {
    if (this.mode === mode) return;
    // モード切替の瞬間にボタン押しっぱなし等が残らないよう、進行中のジェスチャは打ち切る。
    this.abortGestures();
    this.mode = mode;
  }

  getMode(): TouchMode {
    return this.mode;
  }

  /** ズームを等倍に戻す。 */
  resetZoom(): void {
    this.clearZoom(true);
  }

  /** ズーム状態と canvas の transform をクリアする。notify=true のときだけ onZoomChange を呼ぶ。 */
  private clearZoom(notify: boolean): void {
    this.zoomScale = 1;
    this.zoomTx = 0;
    this.zoomTy = 0;
    this.applyTransform();
    if (notify) this.opts.onZoomChange?.(1);
  }

  private applyTransform(): void {
    const canvas = this.canvas;
    if (!canvas) return;
    canvas.style.transformOrigin = "0 0";
    canvas.style.transform = `translate(${this.zoomTx}px, ${this.zoomTy}px) scale(${this.zoomScale})`;
  }

  /** 進行中の 1本指/2本指ジェスチャを、押しっぱなしを残さず中断する。 */
  private abortGestures(): void {
    if (this.singleLongPressTimer !== null) {
      clearTimeout(this.singleLongPressTimer);
      this.singleLongPressTimer = null;
    }
    if (this.singleButtonDown) {
      const button = this.mode === "direct" ? "Right" : "Left";
      this.opts.sendInput({ MouseButton: { button, down: false } });
    }
    this.singleActive = false;
    this.singleButtonDown = false;
    this.singleMoved = false;
    this.multiActive = false;
    this.multiIds = null;
    this.multiDecided = "none";
    this.zoomBase = null;
    this.wheelAccX = 0;
    this.wheelAccY = 0;
    this.suppressSingle = false;
  }

  private onTouchStart(e: TouchEvent): void {
    e.preventDefault();
    for (const t of Array.from(e.changedTouches)) {
      this.touches.set(t.identifier, { x: t.clientX, y: t.clientY });
    }
    const count = this.touches.size;
    if (count === 1) {
      this.beginSingle();
    } else if (count === 2) {
      // 1本指操作の途中で2本目が触れたケース: 単発ジェスチャを打ち切ってから2本指ジェスチャへ切替える。
      if (this.singleActive) this.endSingleAbrupt();
      this.beginMulti();
    }
    // 3本目以降は無視 (ジェスチャ対象を増やさない)。
  }

  private onTouchMove(e: TouchEvent): void {
    e.preventDefault();
    for (const t of Array.from(e.changedTouches)) {
      if (this.touches.has(t.identifier)) this.touches.set(t.identifier, { x: t.clientX, y: t.clientY });
    }
    if (this.multiActive) {
      this.onMultiMove();
    } else if (this.singleActive && this.touches.size === 1) {
      this.onSingleMove();
    }
  }

  private onTouchEnd(e: TouchEvent, cancelled = false): void {
    e.preventDefault();
    const endedIds = Array.from(e.changedTouches).map((t) => t.identifier);
    for (const id of endedIds) this.touches.delete(id);

    if (this.multiActive && this.multiIds) {
      const endedOne = endedIds.includes(this.multiIds[0]) || endedIds.includes(this.multiIds[1]);
      if (endedOne) {
        this.endMulti(cancelled);
        // 残った指がもう1本あっても、2本指ジェスチャの続きとして誤反応させない。
        this.suppressSingle = this.touches.size > 0;
      }
    } else if (this.singleActive && endedIds.length > 0) {
      this.endSingle(cancelled);
    }

    if (this.touches.size === 0) this.suppressSingle = false;
  }

  // ================= 1本指ジェスチャ =================

  private beginSingle(): void {
    if (this.suppressSingle) return;
    const p = this.firstTouch();
    if (!p) return;
    this.singleActive = true;
    this.singleStart = p;
    this.singleLast = p;
    this.singleStartTime = performance.now();
    this.singleMoved = false;
    this.singleButtonDown = false;
    if (this.singleLongPressTimer !== null) clearTimeout(this.singleLongPressTimer);
    this.singleLongPressTimer = window.setTimeout(() => this.onLongPress(), LONG_PRESS_MS);
  }

  private onLongPress(): void {
    this.singleLongPressTimer = null;
    if (!this.singleActive || this.singleMoved) return;
    this.singleButtonDown = true;
    if (this.mode === "direct") {
      const host = this.toHostPoint(this.singleLast);
      if (host) this.opts.sendInput({ MouseMove: host });
      this.opts.sendInput({ MouseButton: { button: "Right", down: true } });
    } else {
      this.opts.sendInput({ MouseButton: { button: "Left", down: true } });
    }
  }

  private onSingleMove(): void {
    const cur = this.firstTouch();
    if (!cur) return;
    const distFromStart = Math.hypot(cur.x - this.singleStart.x, cur.y - this.singleStart.y);
    if (distFromStart >= MOVE_THRESHOLD_PX) {
      this.singleMoved = true;
      if (this.singleLongPressTimer !== null) {
        clearTimeout(this.singleLongPressTimer);
        this.singleLongPressTimer = null;
      }
    }

    if (this.mode === "trackpad") {
      const rect = this.rect();
      const display = this.display;
      if (rect && display) {
        const sx = display.width / rect.width;
        const sy = display.height / rect.height;
        const dx = Math.round((cur.x - this.singleLast.x) * sx);
        const dy = Math.round((cur.y - this.singleLast.y) * sy);
        if (dx !== 0 || dy !== 0) this.opts.sendInput({ MouseMoveRelative: { dx, dy } });
      }
    } else {
      const host = this.toHostPoint(cur);
      if (host) this.opts.sendInput({ MouseMove: host });
    }
    this.singleLast = cur;
  }

  private endSingle(cancelled: boolean): void {
    if (this.singleLongPressTimer !== null) {
      clearTimeout(this.singleLongPressTimer);
      this.singleLongPressTimer = null;
    }
    const duration = performance.now() - this.singleStartTime;
    if (this.singleButtonDown) {
      const button = this.mode === "direct" ? "Right" : "Left";
      this.opts.sendInput({ MouseButton: { button, down: false } });
    } else if (!cancelled && duration < TAP_MAX_MS && !this.singleMoved) {
      if (this.mode === "direct") {
        const host = this.toHostPoint(this.singleLast);
        if (host) this.opts.sendInput({ MouseMove: host });
      }
      this.opts.sendInput({ MouseButton: { button: "Left", down: true } });
      this.opts.sendInput({ MouseButton: { button: "Left", down: false } });
    }
    this.singleActive = false;
    this.singleButtonDown = false;
    this.singleMoved = false;
  }

  /** 2本目の指が触れて割り込まれた場合の単発ジェスチャの後始末 (タップ/長押し判定はしない)。 */
  private endSingleAbrupt(): void {
    if (this.singleLongPressTimer !== null) {
      clearTimeout(this.singleLongPressTimer);
      this.singleLongPressTimer = null;
    }
    if (this.singleButtonDown) {
      const button = this.mode === "direct" ? "Right" : "Left";
      this.opts.sendInput({ MouseButton: { button, down: false } });
    }
    this.singleActive = false;
    this.singleButtonDown = false;
    this.singleMoved = false;
  }

  private firstTouch(): Pt | null {
    for (const p of this.touches.values()) return p;
    return null;
  }

  // ================= 2本指ジェスチャ =================

  private beginMulti(): void {
    const ids = Array.from(this.touches.keys());
    if (ids.length < 2) return;
    const id1 = ids[0];
    const id2 = ids[1];
    const p1 = this.touches.get(id1);
    const p2 = this.touches.get(id2);
    if (!p1 || !p2) return;
    this.multiActive = true;
    this.multiIds = [id1, id2];
    this.multiStartDist = distance(p1, p2);
    this.multiStartMid = midpoint(p1, p2);
    this.multiLastMid = this.multiStartMid;
    this.multiStartTime = performance.now();
    this.multiDecided = "none";
    this.multiMoved = false;
    this.wheelAccX = 0;
    this.wheelAccY = 0;
    this.zoomBase = null;
  }

  private onMultiMove(): void {
    const ids = this.multiIds;
    if (!ids) return;
    const p1 = this.touches.get(ids[0]);
    const p2 = this.touches.get(ids[1]);
    if (!p1 || !p2) return;
    const dist = distance(p1, p2);
    const mid = midpoint(p1, p2);

    if (this.multiDecided === "none") {
      const movedFromStart = Math.hypot(mid.x - this.multiStartMid.x, mid.y - this.multiStartMid.y);
      if (movedFromStart < GESTURE_DECIDE_PX) return;
      // 種別判定は一度だけ行い、以後ジェスチャが終わるまで変えない (判定の揺れによる誤操作を防ぐ)。
      this.multiMoved = true;
      const rate = this.multiStartDist > 0 ? Math.abs(dist - this.multiStartDist) / this.multiStartDist : 0;
      if (rate > ZOOM_RATE_THRESHOLD) {
        this.multiDecided = "zoom";
        this.beginZoom(mid, dist);
      } else {
        this.multiDecided = "wheel";
      }
    }

    if (this.multiDecided === "zoom") {
      this.updateZoom(mid, dist);
    } else if (this.multiDecided === "wheel") {
      this.updateWheel(mid);
    }
    this.multiLastMid = mid;
  }

  private endMulti(cancelled: boolean): void {
    const duration = performance.now() - this.multiStartTime;
    if (!cancelled && this.multiDecided === "none" && duration < TAP_MAX_MS && !this.multiMoved) {
      this.opts.sendInput({ MouseButton: { button: "Right", down: true } });
      this.opts.sendInput({ MouseButton: { button: "Right", down: false } });
    }
    // ズーム変形自体は resetZoom() まで維持する (仕様通り)。ジェスチャの一時状態のみ後始末する。
    this.multiActive = false;
    this.multiIds = null;
    this.multiDecided = "none";
    this.multiMoved = false;
    this.zoomBase = null;
    this.wheelAccX = 0;
    this.wheelAccY = 0;
  }

  private beginZoom(mid: Pt, dist: number): void {
    const rect = this.rect();
    if (!rect) return;
    const baseScale = this.zoomScale;
    const staticLeft = rect.left - this.zoomTx;
    const staticTop = rect.top - this.zoomTy;
    const staticWidth = rect.width / baseScale;
    const staticHeight = rect.height / baseScale;
    this.zoomBase = {
      scale: baseScale,
      dist,
      anchorLocalX: (mid.x - staticLeft) / baseScale,
      anchorLocalY: (mid.y - staticTop) / baseScale,
      staticLeft,
      staticTop,
      staticWidth,
      staticHeight,
    };
  }

  private updateZoom(mid: Pt, dist: number): void {
    const b = this.zoomBase;
    if (!b) return;
    const newScale = clamp(b.scale * (dist / b.dist), ZOOM_MIN, ZOOM_MAX);
    let newTx = mid.x - b.staticLeft - b.anchorLocalX * newScale;
    let newTy = mid.y - b.staticTop - b.anchorLocalY * newScale;

    // パンのクランプ: キャンバスが表示領域から完全に外れないよう、最低限のオーバーラップを残す。
    const viewport = this.viewportRect();
    const renderedWidth = b.staticWidth * newScale;
    const renderedHeight = b.staticHeight * newScale;
    const minTx = viewport.left + MIN_OVERLAP_PX - b.staticLeft - renderedWidth;
    const maxTx = viewport.right - MIN_OVERLAP_PX - b.staticLeft;
    const minTy = viewport.top + MIN_OVERLAP_PX - b.staticTop - renderedHeight;
    const maxTy = viewport.bottom - MIN_OVERLAP_PX - b.staticTop;
    newTx = clamp(newTx, Math.min(minTx, maxTx), Math.max(minTx, maxTx));
    newTy = clamp(newTy, Math.min(minTy, maxTy), Math.max(minTy, maxTy));

    this.zoomScale = newScale;
    this.zoomTx = newTx;
    this.zoomTy = newTy;
    this.applyTransform();
    this.opts.onZoomChange?.(newScale);
  }

  private updateWheel(mid: Pt): void {
    const dxStep = mid.x - this.multiLastMid.x;
    const dyStep = mid.y - this.multiLastMid.y;
    if (dxStep === 0 && dyStep === 0) return;
    // 縦優先。横移動が縦の2倍を超えるときだけ横スクロールとして扱う。
    const horizontal = Math.abs(dxStep) > Math.abs(dyStep) * 2;
    // 縦横とも指の動きに追従する natural scrolling を採用する (100px=1ノッチの蓄積方式は
    // core/input.ts のホイールと同じ)。ただし符号は core/input.ts:208 の
    // `raw = horizontal ? e.deltaX : -e.deltaY` に合わせる必要がある。deltaX は正=右スクロール
    // なので、指を右へ動かした (dxStep>0、natural scrolling ではコンテンツが右へ追従=表示は
    // 左スクロール) 場合は deltaX 的には負に相当する。よって横だけ反転する (縦は deltaY 側で
    // 既に符号反転済みの規約と一致するため反転不要)。
    if (horizontal) {
      this.wheelAccX += -dxStep;
      const ticks = Math.trunc(this.wheelAccX / WHEEL_PX_PER_NOTCH);
      if (ticks === 0) return;
      this.wheelAccX -= ticks * WHEEL_PX_PER_NOTCH;
      this.opts.sendInput({ MouseWheel: { delta: ticks * 120, horizontal: true } });
    } else {
      this.wheelAccY += dyStep;
      const ticks = Math.trunc(this.wheelAccY / WHEEL_PX_PER_NOTCH);
      if (ticks === 0) return;
      this.wheelAccY -= ticks * WHEEL_PX_PER_NOTCH;
      this.opts.sendInput({ MouseWheel: { delta: ticks * 120, horizontal: false } });
    }
  }

  // ================= 座標変換 =================

  private rect(): DOMRect | null {
    return this.canvas ? this.canvas.getBoundingClientRect() : null;
  }

  /** ズーム中のパンクランプ基準になる表示領域。親要素があればその矩形、無ければビューポート全体。 */
  private viewportRect(): { left: number; top: number; right: number; bottom: number } {
    const parent = this.canvas?.parentElement;
    if (parent) {
      const r = parent.getBoundingClientRect();
      return { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
    }
    return { left: 0, top: 0, right: window.innerWidth, bottom: window.innerHeight };
  }

  private toHostPoint(p: Pt): { x: number; y: number } | null {
    const rect = this.rect();
    const display = this.display;
    if (!rect || !display || rect.width === 0 || rect.height === 0) return null;
    const sx = display.width / rect.width;
    const sy = display.height / rect.height;
    const x = clamp(Math.round((p.x - rect.left) * sx), 0, display.width - 1);
    const y = clamp(Math.round((p.y - rect.top) * sy), 0, display.height - 1);
    return { x, y };
  }
}

function distance(a: Pt, b: Pt): number {
  return Math.hypot(a.x - b.x, a.y - b.y);
}

function midpoint(a: Pt, b: Pt): Pt {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

function clamp(v: number, min: number, max: number): number {
  return Math.min(Math.max(v, min), max);
}
