// タッチ操作 (Android 等) を InputEvent に変換するブリッジ。既存の Input (core/input.ts,
// マウス/キーボード用) とは別クラスとして併存させ、同じ canvas に両方 attach される想定。
// touchend 後に WebView が合成マウスイベント (mousedown/mousemove/mouseup) を発火するため、
// 全ハンドラで e.preventDefault() して抑止しないと Input 側と二重送信になる。

import { type InputEvent } from "./ipc";

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
// 2本目の指の着地を待つ猶予。ピンチは両指が完全同時には着地しないため、1本目だけが
// 先に動くと精密モード (MouseMove 送信) へ入ってしまい、直後に2本目が来てピンチへ
// 切り替わっても送信済み MouseMove は取り消せずカーソルが飛ぶ。2本目の着地は通常
// 50ms 以内なので、80ms 待てば取りこぼさず、単発操作の体感遅延も無視できる。
const MULTI_GRACE_MS = 80;
// 精密モード (ドラッグ/長押し) のカーソルを指の位置から上へずらす量 (CSS px)。
// 指の直下だとカーソル/着地点が指に隠れて見えないため、視認できる位置まで逃がす。
const CURSOR_OFFSET_PX = 40;
// トラックパッド/ホイールの px→ノッチ換算。core/input.ts の WHEEL_PX_PER_NOTCH と揃える。
const WHEEL_PX_PER_NOTCH = 100;
// 2 本指ジェスチャの種別判定 (ズーム/ホイール) を確定させる最小移動量。
const GESTURE_DECIDE_PX = 10;
// 指間距離の変化率がこれを超えたらズームと判定する (それ以外はホイール)。
const ZOOM_RATE_THRESHOLD = 0.15;
const ZOOM_MIN = 1.0;
const ZOOM_MAX = 4.0;

type ZoomBase = {
  scale: number;
  dist: number;
  // アンカー点 (2 本指の中点) の「変形前のキャンバス内ローカル座標」。
  anchorLocalX: number;
  anchorLocalY: number;
  staticLeft: number;
  staticTop: number;
  // 変形前 (scale=1, tx=ty=0 相当) の映像矩形 (client 座標)。object-fit:contain の
  // レターボックス込みで、パンのクランプはキャンバス要素ではなくこちらを基準にする
  // (要素基準だと黒帯ぶんまで動かせてしまう)。
  staticVideoLeft: number;
  staticVideoTop: number;
  staticVideoWidth: number;
  staticVideoHeight: number;
};

/** タッチ操作をホストへの入力イベントに変換する。1 canvas = 1 インスタンス。 */
export class TouchInput {
  private canvas: HTMLCanvasElement | null = null;
  private display: DisplaySize | null = null;
  // オフセットカーソルの DOM。canvas はピンチズームで transform が掛かるため、その子にすると
  // カーソルまで一緒に拡大/移動してしまう。canvas の親 (#screen-wrap) 側に置いて独立させる。
  private cursorEl: HTMLDivElement | null = null;

  // 現在触れている指の位置 (identifier → client 座標)。
  private readonly touches = new Map<number, Pt>();
  // 2 本指ジェスチャの後、残り 1 本が単独ジェスチャとして誤発火しないようにする抑止フラグ。
  private suppressSingle = false;
  // オンスクリーンコントローラ表示中は、映像を直接タップしてもクリックが飛ばないようにする。
  // ズーム/ホイールは残すため、無効化するのはポインタ操作 (1本指ジェスチャと2本指タップ=右クリック) だけ。
  private pointerEnabled = true;

  // --- 1 本指ジェスチャの状態 ---
  private singleActive = false;
  private singleStart: Pt = { x: 0, y: 0 };
  private singleStartTime = 0;
  private singleLast: Pt = { x: 0, y: 0 };
  private singleMoved = false;
  private singleLongPressTimer: number | null = null;
  // "pending" = まだ短いタップ/ドラッグ/長押しのどれか未確定。移動しきい値超過 or 長押しタイマー
  // 発火で "drag"/"longpress" (= 精密モード = カーソル表示) へ一度だけ遷移し、指を離すまで固定する
  // (途中でモードが変わると微調整中にカーソルが消える/クリック種別が変わり誤操作になるため)。
  private singleMode: "pending" | "drag" | "longpress" = "pending";
  // 精密モード中、指を離した瞬間に送るクリック先 (カーソル位置が映像外で無効なときは null)。
  private singleCursorHost: { x: number; y: number } | null = null;

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

    // カーソルは canvas の子ではなく親 (#screen-wrap) へ置く。canvas にはピンチズームの
    // transform が掛かるため、子にするとカーソルまで一緒に拡大縮小/平行移動してしまい
    // 「指位置からオフセットした場所」という意味が崩れる。
    const cursor = document.createElement("div");
    cursor.className = "touch-cursor";
    (canvas.parentElement ?? canvas).appendChild(cursor);
    this.cursorEl = cursor;
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
    this.cursorEl?.remove();
    this.cursorEl = null;
  }

  /** ディスプレイ切替時に解像度を更新する。 */
  setDisplay(display: DisplaySize): void {
    this.display = display;
  }

  /** ズームを等倍に戻す。 */
  resetZoom(): void {
    this.clearZoom(true);
  }

  /**
   * 映像の直接タップによるポインタ操作 (1本指ジェスチャ・2本指タップの右クリック) の有効/無効を
   * 切り替える。オンスクリーンコントローラ表示中は false にして、コントローラ側へ操作を一本化する。
   * ピンチズームと2本指スワイプ (ホイール) はこのフラグの影響を受けない。
   */
  setPointerEnabled(on: boolean): void {
    if (this.pointerEnabled === on) return;
    this.pointerEnabled = on;
    // 無効化の瞬間に進行中の単発ジェスチャが残っていると、カーソルが出たままになったり
    // 離した瞬間にクリックが飛んだりする。クリックを確定させずに後始末する。
    if (!on && this.singleActive) this.endSingleAbrupt();
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

  /** 進行中の 1本指/2本指ジェスチャを中断する。 */
  private abortGestures(): void {
    if (this.singleLongPressTimer !== null) {
      clearTimeout(this.singleLongPressTimer);
      this.singleLongPressTimer = null;
    }
    // ボタン down/up は指を離した瞬間にまとめて送る設計 (精密モード中も押しっぱなしにはしない)
    // なので、中断時に別途ボタンを離す処理は不要。状態をリセットするだけでよい。
    this.singleActive = false;
    this.singleMoved = false;
    this.singleMode = "pending";
    this.singleCursorHost = null;
    this.hideCursor();
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
    // コントローラ表示中は映像タップでのクリックを無効化する (誤操作防止)。ズーム/ホイールは
    // 2本指ジェスチャ側のためこのガードの影響を受けない。
    if (!this.pointerEnabled) return;
    const p = this.firstTouch();
    if (!p) return;
    this.singleActive = true;
    this.singleStart = p;
    this.singleStartTime = performance.now();
    this.singleLast = p;
    this.singleMoved = false;
    this.singleMode = "pending";
    this.singleCursorHost = null;
    if (this.singleLongPressTimer !== null) clearTimeout(this.singleLongPressTimer);
    this.singleLongPressTimer = window.setTimeout(() => this.onLongPress(), LONG_PRESS_MS);
  }

  private onLongPress(): void {
    this.singleLongPressTimer = null;
    if (!this.singleActive || this.singleMoved || this.singleMode !== "pending") return;
    // 長押しは精密モードへ入るだけで、ここではまだ右クリックを送らない (離すまで送らないのが新仕様)。
    this.singleMode = "longpress";
    this.updatePrecisionCursor(this.singleLast);
  }

  private onSingleMove(): void {
    const cur = this.firstTouch();
    if (!cur) return;
    const distFromStart = Math.hypot(cur.x - this.singleStart.x, cur.y - this.singleStart.y);
    if (this.singleMode === "pending") {
      if (distFromStart >= MOVE_THRESHOLD_PX) {
        this.singleMoved = true;
        if (this.singleLongPressTimer !== null) {
          clearTimeout(this.singleLongPressTimer);
          this.singleLongPressTimer = null;
        }
      }
      // MULTI_GRACE_MS 経過前は、移動量の記録だけ行いドラッグへは確定しない (2本目の指を
      // 待つ)。経過後、その時点で移動しきい値を超えていればドラッグへ確定する。以後は
      // 指を離すまでこのモードのまま (途中で判定を変えない)。
      if (this.singleMoved && performance.now() - this.singleStartTime >= MULTI_GRACE_MS) {
        this.singleMode = "drag";
      }
    }
    this.singleLast = cur;

    if (this.singleMode === "drag" || this.singleMode === "longpress") {
      this.updatePrecisionCursor(cur);
    }
    // pending (= 短いタップになるかもしれない段階) では、まだカーソルもホストの
    // MouseMove も出さない。ここで送ってしまうと「カーソルは出さない」というタップの
    // 仕様が崩れ、微小なジッタでもホスト側マウスが動いてしまう。
  }

  private endSingle(cancelled: boolean): void {
    if (this.singleLongPressTimer !== null) {
      clearTimeout(this.singleLongPressTimer);
      this.singleLongPressTimer = null;
    }
    if (this.singleMode === "pending") {
      // 押下時間は条件にしない。pending のまま指を動かさず離せた時点で、長押し (LONG_PRESS_MS)
      // には届かず精密モードにも入っていないことが確定しているため、必ずタップとして扱ってよい。
      // 上限を課すと「ゆっくり押して離す」が長押しにもタップにもならない死角になり、
      // 押したのに何も起きない状態になる。
      const movedWithinGrace =
        this.singleMoved && performance.now() - this.singleStartTime < MULTI_GRACE_MS;
      if (!cancelled && (!this.singleMoved || movedWithinGrace)) {
        const host = this.toHostPoint(this.singleLast);
        if (host) this.opts.sendInput({ MouseMove: host });
        this.opts.sendInput({ MouseButton: { button: "Left", down: true } });
        this.opts.sendInput({ MouseButton: { button: "Left", down: false } });
      } else if (!cancelled) {
        // 猶予は「2本目の指の着地を待つ」ためのものであり、待った結果として単発操作を
        // 無効にしてはならない。ここに来るのは猶予経過後に touchmove が途絶えた場合だけで、
        // もう1回でも touchmove が来ていれば drag へ確定し着地点はカーソル位置になる。
        // 同じ物理操作の着地点が指の位置とカーソル位置 (40px差) で割れるのは一貫性を欠くため、
        // drag と同じ確定処理に揃える。
        this.updatePrecisionCursor(this.singleLast);
        if (this.singleCursorHost) {
          this.opts.sendInput({ MouseButton: { button: "Left", down: true } });
          this.opts.sendInput({ MouseButton: { button: "Left", down: false } });
        }
      }
    } else if (!cancelled && this.singleCursorHost) {
      // 精密モード (ドラッグ/長押し) は離した瞬間にカーソル位置でクリックを確定する。
      // カーソル位置が映像外 (singleCursorHost===null) のときは押せる場所が無いので何もしない。
      const button = this.singleMode === "longpress" ? "Right" : "Left";
      this.opts.sendInput({ MouseButton: { button, down: true } });
      this.opts.sendInput({ MouseButton: { button, down: false } });
    }
    this.singleActive = false;
    this.singleMoved = false;
    this.singleMode = "pending";
    this.singleCursorHost = null;
    this.hideCursor();
  }

  /** 2本目の指が触れて割り込まれた場合の単発ジェスチャの後始末 (クリックは確定させない)。 */
  private endSingleAbrupt(): void {
    if (this.singleLongPressTimer !== null) {
      clearTimeout(this.singleLongPressTimer);
      this.singleLongPressTimer = null;
    }
    // 精密モード中でもボタンを押しっぱなしにはしていないので、離す処理は不要。
    this.singleActive = false;
    this.singleMoved = false;
    this.singleMode = "pending";
    this.singleCursorHost = null;
    this.hideCursor();
  }

  /**
   * 精密モード (ドラッグ/長押し) 中、指の位置からオフセットしたカーソルを描画し、
   * その位置へ MouseMove を送る。指を離すまでボタンは送らない。
   */
  private updatePrecisionCursor(fingerPt: Pt): void {
    const el = this.cursorEl;
    const parent = this.canvas?.parentElement;
    if (!el || !parent) return;
    const cursorPt = this.offsetCursorPoint(fingerPt);
    const parentRect = parent.getBoundingClientRect();
    el.style.left = `${cursorPt.x - parentRect.left}px`;
    el.style.top = `${cursorPt.y - parentRect.top}px`;
    el.classList.add("visible");
    el.classList.toggle("right", this.singleMode === "longpress");

    const host = this.toHostPoint(cursorPt);
    this.singleCursorHost = host;
    el.classList.toggle("invalid", host === null);
    if (host) this.opts.sendInput({ MouseMove: host });
  }

  /** カーソルを隠し、次のジェスチャに古い見た目 (right/invalid) を持ち越さないよう修飾クラスも外す。 */
  private hideCursor(): void {
    this.cursorEl?.classList.remove("visible", "right", "invalid");
  }

  /**
   * 指の位置から `CURSOR_OFFSET_PX` 上にずらしたカーソル点 (client 座標) を返す。
   * 画面最上部付近をタップすると、素直にオフセットすると映像の外 (黒帯/画面外) に
   * カーソルが出てしまい toHostPoint が null を返し続けて何も押せなくなる。指の Y が
   * 映像上端に近いときはオフセット量を縮め、カーソルが必ず映像内に収まるようにする。
   */
  private offsetCursorPoint(fingerPt: Pt): Pt {
    const v = this.videoRect();
    if (!v) return fingerPt;
    const available = fingerPt.y - v.top;
    const offset = Math.max(0, Math.min(CURSOR_OFFSET_PX, available));
    return { x: fingerPt.x, y: fingerPt.y - offset };
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
    // 2本指タップ (右クリック) もコントローラ表示中は無効化する。ズーム/ホイール判定
    // (multiDecided が zoom/wheel) はこの分岐に来ないため影響を受けない。
    if (this.pointerEnabled && !cancelled && this.multiDecided === "none" && duration < TAP_MAX_MS && !this.multiMoved) {
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
    const display = this.display;
    if (!rect || !display) return;
    const baseScale = this.zoomScale;
    const staticLeft = rect.left - this.zoomTx;
    const staticTop = rect.top - this.zoomTy;
    const staticWidth = rect.width / baseScale;
    const staticHeight = rect.height / baseScale;
    // videoRect() と同じ contain 計算を、変形前のキャンバス矩形 (staticWidth/Height) に対して
    // 行う。これで「現在の transform 抜きの映像矩形」が求まり、クランプの基準にできる。
    const containScale = Math.min(staticWidth / display.width, staticHeight / display.height);
    const staticVideoWidth = display.width * containScale;
    const staticVideoHeight = display.height * containScale;
    const staticVideoLeft = staticLeft + (staticWidth - staticVideoWidth) / 2;
    const staticVideoTop = staticTop + (staticHeight - staticVideoHeight) / 2;
    this.zoomBase = {
      scale: baseScale,
      dist,
      // アンカー (2本指の中点) の要素ローカル座標。transform-origin:0 0 で translate→scale を
      // 掛けているのでローカル点 L の画面位置は `staticLeft + tx + scale*L`。逆変換すると
      // `L = (画面座標 - staticLeft - tx) / scale` で、`staticLeft = rect.left - tx` だから
      // 結局「現在の描画矩形からの相対 ÷ 現在の倍率」に等しい。tx を引き忘れると、一度ズーム/
      // パンして tx≠0 になった後の 2 回目以降のピンチでアンカーが tx/scale ぶんずれ、その差が
      // 一気に適用されて画面が飛ぶ。
      anchorLocalX: (mid.x - rect.left) / baseScale,
      anchorLocalY: (mid.y - rect.top) / baseScale,
      staticLeft,
      staticTop,
      staticVideoLeft,
      staticVideoTop,
      staticVideoWidth,
      staticVideoHeight,
    };
  }

  private updateZoom(mid: Pt, dist: number): void {
    const b = this.zoomBase;
    if (!b) return;
    const newScale = clamp(b.scale * (dist / b.dist), ZOOM_MIN, ZOOM_MAX);
    if (newScale <= ZOOM_MIN) {
      // 等倍まで戻したら平行移動も原点へ戻す。クランプ範囲内に留めるだけだと、等倍なのに映像が
      // 中央からずれた位置に居座り「ズームアウトしたら飛んでいった」という体感になる。等倍時は
      // object-fit: contain 本来の配置に戻すのが正しい状態。
      this.zoomScale = ZOOM_MIN;
      this.zoomTx = 0;
      this.zoomTy = 0;
      this.applyTransform();
      this.opts.onZoomChange?.(ZOOM_MIN);
      return;
    }
    let newTx = mid.x - b.staticLeft - b.anchorLocalX * newScale;
    let newTy = mid.y - b.staticTop - b.anchorLocalY * newScale;

    // パンのクランプ: 黒帯まで含むキャンバス要素ではなく、実際に映像が描かれている矩形
    // (staticVideo*) が表示領域を常に覆うようにする。映像のほうが表示領域より小さい軸は、
    // 動かせても黒帯の位置が変わるだけで意味がないので中央固定にする。
    const viewport = this.viewportRect();
    newTx = clampPanAxis(newTx, newScale, b.staticLeft, b.staticVideoLeft, b.staticVideoWidth, viewport.left, viewport.right);
    newTy = clampPanAxis(newTy, newScale, b.staticTop, b.staticVideoTop, b.staticVideoHeight, viewport.top, viewport.bottom);

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

  /**
   * canvas は CSS で object-fit:contain (styles/base.css の #screen)。ホストと端末でアスペクト比が
   * 異なると要素内部に黒帯が入り、映像が実際に描かれている矩形は canvas 要素の矩形と一致しない。
   * 倍率は縦横共通の contain スケール (core/input.ts の toDisplayCoords と同じ考え方)。
   */
  private containScale(): number | null {
    const rect = this.rect();
    const display = this.display;
    if (!rect || !display || rect.width === 0 || rect.height === 0) return null;
    return Math.min(rect.width / display.width, rect.height / display.height);
  }

  /**
   * 実際に映像が描かれている矩形 (client 座標、object-fit:contain のレターボックス込み)。
   * オフセットカーソルのクランプ (offsetCursorPoint) でも映像上端の座標が要るため、
   * toHostPoint の内部計算をここへ切り出して両者で共有する (計算がずれると toHostPoint の
   * 「映像外は null」判定とカーソルの見た目上の位置が食い違うため)。
   */
  private videoRect(): { left: number; top: number; width: number; height: number } | null {
    const rect = this.rect();
    const display = this.display;
    const scale = this.containScale();
    if (!rect || !display || scale === null || scale <= 0) return null;
    const width = display.width * scale;
    const height = display.height * scale;
    return {
      left: rect.left + (rect.width - width) / 2,
      top: rect.top + (rect.height - height) / 2,
      width,
      height,
    };
  }

  private toHostPoint(p: Pt): { x: number; y: number } | null {
    const display = this.display;
    const v = this.videoRect();
    if (!display || !v || v.width <= 0 || v.height <= 0) return null;
    const vx = p.x - v.left;
    const vy = p.y - v.top;
    // 黒帯部分 (映像外) のタッチはホストへ送らない (端へ吸着させると誤操作になる。input.ts と同じ判断)。
    if (vx < 0 || vy < 0 || vx > v.width || vy > v.height) return null;
    return {
      x: Math.round((vx / v.width) * display.width),
      y: Math.round((vy / v.height) * display.height),
    };
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

/**
 * ズームのパン量 (1 軸ぶん) を、映像が表示領域を覆う範囲へクランプする。
 * 変形は transform-origin:0 0 の translate→scale なので、静的座標 s は
 * `screen = staticElemMin + t + scale*(s - staticElemMin)` へ写る。
 * 映像 (staticVideoMin..+Size) が表示領域以上に大きい場合はその範囲を覆うように t をクランプし、
 * 表示領域より小さい場合は動かせても黒帯の位置が変わるだけなので中央固定の t を一意に返す。
 */
function clampPanAxis(
  desiredT: number,
  scale: number,
  staticElemMin: number,
  staticVideoMin: number,
  staticVideoSize: number,
  viewportMin: number,
  viewportMax: number,
): number {
  const videoSize = scale * staticVideoSize;
  const viewportSize = viewportMax - viewportMin;
  if (videoSize >= viewportSize) {
    const maxT = viewportMin - staticElemMin - scale * (staticVideoMin - staticElemMin);
    const minT = viewportMax - staticElemMin - scale * (staticVideoMin + staticVideoSize - staticElemMin);
    return clamp(desiredT, minT, maxT);
  }
  const videoCenterStatic = staticVideoMin + staticVideoSize / 2;
  const viewportCenter = (viewportMin + viewportMax) / 2;
  return viewportCenter - staticElemMin - scale * (videoCenterStatic - staticElemMin);
}
