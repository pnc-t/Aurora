// canvas 上のマウス/キー入力を ControlMsg::Input として送信 (1 セッション = 1 インスタンス)。
// 座標はホスト側ディスプレイ解像度にスケーリングする。
// マルチタブでは「アクティブなセッションの Input だけ」を canvas/window へ attach する。
// 送信先セッションの振り分けは、生成時に渡される `sendInput` コールバック (session_id を
// 内包) が担うため、この層は ipc を知らない。

import { type InputEvent } from "./ipc";

type DisplaySize = { width: number; height: number };

type CaptureCallback = (captured: boolean) => void;

type InputOpts = {
  /** 1 入力イベントをホスト (自分のセッション) へ送る。 */
  sendInput: (event: InputEvent) => void;
};

export class Input {
  private canvas: HTMLCanvasElement | null = null;
  private display: DisplaySize | null = null;
  private enabled = false;
  // 押下中の VK / マウスボタン。フォーカス喪失時に解放して残留を防ぐ。
  private readonly pressedKeys = new Set<number>();
  private readonly pressedButtons = new Set<"Left" | "Right" | "Middle">();
  // ポインタロック (相対マウス) が有効か。pointerlockchange で更新する。
  private captured = false;
  private onCaptureChange: CaptureCallback | null = null;
  // ピクセル単位ホイールの軸ごと累積量。
  private wheelAccX = 0;
  private wheelAccY = 0;

  // ハンドラはインスタンスにバインドして add/remove の同一性を保つ。
  private readonly hMouseMove = (e: MouseEvent) => this.onMouseMove(e);
  private readonly hMouseDown = (e: MouseEvent) => this.onMouseDown(e);
  private readonly hMouseUp = (e: MouseEvent) => this.onMouseUp(e);
  private readonly hWheel = (e: WheelEvent) => this.onWheel(e);
  private readonly hContext = (e: Event) => e.preventDefault();
  private readonly hKeyDown = (e: KeyboardEvent) => this.onKeyDown(e);
  private readonly hKeyUp = (e: KeyboardEvent) => this.onKeyUp(e);
  private readonly hBlur = () => this.releaseAll();
  private readonly hLockChange = () => this.onPointerLockChange();
  private readonly hLockError = () => this.onPointerLockError();

  constructor(private readonly opts: InputOpts) {}

  attach(canvas: HTMLCanvasElement, display: DisplaySize, opts?: { onCaptureChange?: CaptureCallback }): void {
    this.detach();
    this.canvas = canvas;
    this.display = display;
    this.enabled = true;
    this.onCaptureChange = opts?.onCaptureChange ?? null;

    document.addEventListener("pointerlockchange", this.hLockChange);
    document.addEventListener("pointerlockerror", this.hLockError);

    canvas.addEventListener("mousemove", this.hMouseMove);
    canvas.addEventListener("mousedown", this.hMouseDown);
    canvas.addEventListener("mouseup", this.hMouseUp);
    canvas.addEventListener("wheel", this.hWheel, { passive: false });
    canvas.addEventListener("contextmenu", this.hContext);
    window.addEventListener("keydown", this.hKeyDown);
    window.addEventListener("keyup", this.hKeyUp);
    // フォーカス喪失 (Alt+Tab 等) で keyup が来ず修飾キーが残留するのを防ぐ。
    window.addEventListener("blur", this.hBlur);
  }

  detach(): void {
    const canvas = this.canvas;
    if (canvas) {
      canvas.removeEventListener("mousemove", this.hMouseMove);
      canvas.removeEventListener("mousedown", this.hMouseDown);
      canvas.removeEventListener("mouseup", this.hMouseUp);
      canvas.removeEventListener("wheel", this.hWheel);
      canvas.removeEventListener("contextmenu", this.hContext);
    }
    window.removeEventListener("keydown", this.hKeyDown);
    window.removeEventListener("keyup", this.hKeyUp);
    window.removeEventListener("blur", this.hBlur);
    document.removeEventListener("pointerlockchange", this.hLockChange);
    document.removeEventListener("pointerlockerror", this.hLockError);
    this.releaseAll();
    // ホイールの端数累積をセッション/タブを跨いで持ち越さない。
    this.wheelAccX = 0;
    this.wheelAccY = 0;
    // キャプチャ中なら解除する。
    if (document.pointerLockElement) document.exitPointerLock();
    this.captured = false;
    this.canvas = null;
    this.display = null;
    this.enabled = false;
    this.onCaptureChange = null;
  }

  /** ポインタロック(相対マウス)モードの ON/OFF。実状態は pointerlockchange で確定する。 */
  setCaptureMode(on: boolean): void {
    const canvas = this.canvas;
    if (!canvas) return;
    if (on) {
      // ユーザージェスチャ起点でないと拒否されることがある (ホットキー/クリック由来で呼ぶ)。
      void canvas.requestPointerLock();
    } else if (document.pointerLockElement) {
      document.exitPointerLock();
    }
  }

  isCaptured(): boolean {
    return this.captured;
  }

  private onPointerLockChange(): void {
    this.captured = document.pointerLockElement === this.canvas;
    // ロック解除時 (Esc 含む) に押下中マウスボタンが残らないよう解放。
    if (!this.captured) {
      for (const button of this.pressedButtons) {
        this.dispatch({ MouseButton: { button, down: false } });
      }
      this.pressedButtons.clear();
    }
    this.onCaptureChange?.(this.captured);
  }

  private onPointerLockError(): void {
    this.captured = false;
    this.onCaptureChange?.(false);
  }

  /** 押下中の全キー / マウスボタンに up を送って残留を解消する。 */
  releaseAll(): void {
    for (const vk of this.pressedKeys) {
      this.dispatch({ Key: { vk, down: false } });
    }
    this.pressedKeys.clear();
    for (const button of this.pressedButtons) {
      this.dispatch({ MouseButton: { button, down: false } });
    }
    this.pressedButtons.clear();
  }

  private toDisplayCoords(e: MouseEvent): { x: number; y: number } | null {
    const canvas = this.canvas;
    const display = this.display;
    if (!canvas || !display) return null;
    const rect = canvas.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return null;
    // canvas は CSS で object-fit:contain。映像はアスペクト比を保って canvas 要素「内部」に
    // レターボックス配置されるため、要素全体 (rect) で正規化すると黒帯のぶんだけ座標がズレる。
    // 実際に映像が描かれている矩形 (contain 計算) を求め、その中での相対位置で正規化する。
    const scale = Math.min(rect.width / display.width, rect.height / display.height);
    const videoW = display.width * scale;
    const videoH = display.height * scale;
    const offsetX = (rect.width - videoW) / 2;
    const offsetY = (rect.height - videoH) / 2;
    const vx = e.clientX - rect.left - offsetX;
    const vy = e.clientY - rect.top - offsetY;
    // 黒帯部分 (映像外) のクリックはホストへ送らない (端へ吸着させると誤操作になる)。
    if (vx < 0 || vy < 0 || vx > videoW || vy > videoH) return null;
    return {
      x: Math.round((vx / videoW) * display.width),
      y: Math.round((vy / videoH) * display.height),
    };
  }

  private dispatch(ev: InputEvent): void {
    if (!this.enabled) return;
    this.opts.sendInput(ev);
  }

  private onMouseMove(e: MouseEvent): void {
    if (this.captured) {
      // ポインタロック中は相対移動 (movementX/Y) を送る。
      const dx = e.movementX;
      const dy = e.movementY;
      if (dx === 0 && dy === 0) return;
      this.dispatch({ MouseMoveRelative: { dx, dy } });
      return;
    }
    const p = this.toDisplayCoords(e);
    if (!p) return;
    this.dispatch({ MouseMove: p });
  }

  private onMouseDown(e: MouseEvent): void {
    const b = mouseButton(e.button);
    if (!b) return;
    e.preventDefault();
    this.pressedButtons.add(b);
    this.dispatch({ MouseButton: { button: b, down: true } });
  }

  private onMouseUp(e: MouseEvent): void {
    const b = mouseButton(e.button);
    if (!b) return;
    e.preventDefault();
    this.pressedButtons.delete(b);
    this.dispatch({ MouseButton: { button: b, down: false } });
  }

  private onWheel(e: WheelEvent): void {
    e.preventDefault();
    const horizontal = Math.abs(e.deltaX) > Math.abs(e.deltaY);
    const raw = horizontal ? e.deltaX : -e.deltaY;
    if (raw === 0) return;

    if (e.deltaMode !== 0) {
      // 行/ページ単位 (一部の物理マウス): 1 イベント = 1 ノッチ。
      this.dispatch({ MouseWheel: { delta: Math.sign(raw) * 120, horizontal } });
      return;
    }
    // ピクセル単位: 軸ごとに累積し、ノッチ境界を跨いだぶんだけ送る。
    if (horizontal) {
      this.wheelAccX += raw;
      const ticks = Math.trunc(this.wheelAccX / WHEEL_PX_PER_NOTCH);
      if (ticks === 0) return;
      this.wheelAccX -= ticks * WHEEL_PX_PER_NOTCH;
      this.dispatch({ MouseWheel: { delta: ticks * 120, horizontal: true } });
    } else {
      this.wheelAccY += raw;
      const ticks = Math.trunc(this.wheelAccY / WHEEL_PX_PER_NOTCH);
      if (ticks === 0) return;
      this.wheelAccY -= ticks * WHEEL_PX_PER_NOTCH;
      this.dispatch({ MouseWheel: { delta: ticks * 120, horizontal: false } });
    }
  }

  private onKeyDown(e: KeyboardEvent): void {
    const vk = vkFromKeyboardEvent(e);
    if (vk == null) return;
    e.preventDefault();
    // オートリピート (e.repeat) もホストへ転送する。SendInput で注入した keydown は
    // ホスト OS のタイプマティックを誘発しないため、転送しないと押しっぱなしが 1 回で止まる。
    // 既に登録済みなので pressedKeys への追加は初回のみでよい。
    if (!e.repeat) this.pressedKeys.add(vk);
    this.dispatch({ Key: { vk, down: true } });
  }

  private onKeyUp(e: KeyboardEvent): void {
    const vk = vkFromKeyboardEvent(e);
    if (vk == null) return;
    e.preventDefault();
    this.pressedKeys.delete(vk);
    this.dispatch({ Key: { vk, down: false } });
  }
}

// ピクセル単位ホイール (トラックパッド/高解像度マウス) の 1 ノッチあたりの px 量。
// 符号だけ送ると小刻みな入力が毎回フル 1 ノッチ (120) になり過剰スクロールするため、
// 約 100px ごとに 120 を 1 つ送る。
const WHEEL_PX_PER_NOTCH = 100;

function mouseButton(button: number): "Left" | "Right" | "Middle" | null {
  switch (button) {
    case 0:
      return "Left";
    case 1:
      return "Middle";
    case 2:
      return "Right";
    default:
      return null;
  }
}

/** KeyboardEvent.code/key から Windows VK_* (おおまかなマッピング) を返す。 */
function vkFromKeyboardEvent(e: KeyboardEvent): number | null {
  // 英数
  if (/^Key[A-Z]$/.test(e.code)) return e.code.charCodeAt(3); // 'A'..'Z'
  if (/^Digit[0-9]$/.test(e.code)) return e.code.charCodeAt(5); // '0'..'9'
  if (/^Numpad[0-9]$/.test(e.code)) return 0x60 + (e.code.charCodeAt(6) - 0x30); // VK_NUMPAD0..9
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(e.code)) {
    const n = parseInt(e.code.substring(1), 10);
    return 0x6f + n; // VK_F1=0x70 → 0x6F+1
  }
  switch (e.code) {
    case "Backspace": return 0x08;
    case "Tab": return 0x09;
    case "Enter":
    case "NumpadEnter": return 0x0d;
    case "ShiftLeft": return 0xa0;
    case "ShiftRight": return 0xa1;
    case "ControlLeft": return 0xa2;
    case "ControlRight": return 0xa3;
    case "AltLeft": return 0xa4;
    case "AltRight": return 0xa5;
    case "CapsLock": return 0x14;
    case "Escape": return 0x1b;
    case "Space": return 0x20;
    case "PageUp": return 0x21;
    case "PageDown": return 0x22;
    case "End": return 0x23;
    case "Home": return 0x24;
    case "ArrowLeft": return 0x25;
    case "ArrowUp": return 0x26;
    case "ArrowRight": return 0x27;
    case "ArrowDown": return 0x28;
    case "Insert": return 0x2d;
    case "Delete": return 0x2e;
    case "MetaLeft": return 0x5b;
    case "MetaRight": return 0x5c;
    case "NumpadMultiply": return 0x6a;
    case "NumpadAdd": return 0x6b;
    case "NumpadSubtract": return 0x6d;
    case "NumpadDecimal": return 0x6e;
    case "NumpadDivide": return 0x6f;
    case "Semicolon": return 0xba;
    case "Equal": return 0xbb;
    case "Comma": return 0xbc;
    case "Minus": return 0xbd;
    case "Period": return 0xbe;
    case "Slash": return 0xbf;
    case "Backquote": return 0xc0;
    case "BracketLeft": return 0xdb;
    case "Backslash": return 0xdc;
    case "BracketRight": return 0xdd;
    case "Quote": return 0xde;
    default: return null;
  }
}
