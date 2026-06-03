// canvas 上のマウス/キー入力を ControlMsg::Input として送信。
// 座標はホスト側ディスプレイ解像度にスケーリングする。

import { sendInput, type InputEvent } from "./ipc";

type DisplaySize = { width: number; height: number };

type CaptureCallback = (captured: boolean) => void;

const state: {
  canvas: HTMLCanvasElement | null;
  display: DisplaySize | null;
  enabled: boolean;
  // 押下中の VK / マウスボタン。フォーカス喪失時に解放して残留を防ぐ。
  pressedKeys: Set<number>;
  pressedButtons: Set<"Left" | "Right" | "Middle">;
  // ポインタロック (相対マウス) が有効か。pointerlockchange で更新する。
  captured: boolean;
  onCaptureChange: CaptureCallback | null;
} = {
  canvas: null,
  display: null,
  enabled: false,
  pressedKeys: new Set(),
  pressedButtons: new Set(),
  captured: false,
  onCaptureChange: null,
};

export function attachInput(
  canvas: HTMLCanvasElement,
  display: DisplaySize,
  opts?: { onCaptureChange?: CaptureCallback },
): void {
  detachInput();
  state.canvas = canvas;
  state.display = display;
  state.enabled = true;
  state.onCaptureChange = opts?.onCaptureChange ?? null;

  document.addEventListener("pointerlockchange", onPointerLockChange);
  document.addEventListener("pointerlockerror", onPointerLockError);

  canvas.addEventListener("mousemove", onMouseMove);
  canvas.addEventListener("mousedown", onMouseDown);
  canvas.addEventListener("mouseup", onMouseUp);
  canvas.addEventListener("wheel", onWheel, { passive: false });
  canvas.addEventListener("contextmenu", preventDefault);
  window.addEventListener("keydown", onKeyDown);
  window.addEventListener("keyup", onKeyUp);
  // フォーカス喪失 (Alt+Tab 等) で keyup が来ず修飾キーが残留するのを防ぐ。
  window.addEventListener("blur", releaseAllInput);
}

export function detachInput(): void {
  const canvas = state.canvas;
  if (canvas) {
    canvas.removeEventListener("mousemove", onMouseMove);
    canvas.removeEventListener("mousedown", onMouseDown);
    canvas.removeEventListener("mouseup", onMouseUp);
    canvas.removeEventListener("wheel", onWheel);
    canvas.removeEventListener("contextmenu", preventDefault);
  }
  window.removeEventListener("keydown", onKeyDown);
  window.removeEventListener("keyup", onKeyUp);
  window.removeEventListener("blur", releaseAllInput);
  document.removeEventListener("pointerlockchange", onPointerLockChange);
  document.removeEventListener("pointerlockerror", onPointerLockError);
  releaseAllInput();
  // キャプチャ中なら解除する。
  if (document.pointerLockElement) document.exitPointerLock();
  state.captured = false;
  state.canvas = null;
  state.display = null;
  state.enabled = false;
  state.onCaptureChange = null;
}

/** ポインタロック(相対マウス)モードの ON/OFF。実状態は pointerlockchange で確定する。 */
export function setCaptureMode(on: boolean): void {
  const canvas = state.canvas;
  if (!canvas) return;
  if (on) {
    // ユーザージェスチャ起点でないと拒否されることがある (ホットキー/クリック由来で呼ぶ)。
    void canvas.requestPointerLock();
  } else if (document.pointerLockElement) {
    document.exitPointerLock();
  }
}

export function isCaptured(): boolean {
  return state.captured;
}

function onPointerLockChange(): void {
  state.captured = document.pointerLockElement === state.canvas;
  // ロック解除時 (Esc 含む) に押下中マウスボタンが残らないよう解放。
  if (!state.captured) {
    for (const button of state.pressedButtons) {
      dispatch({ MouseButton: { button, down: false } });
    }
    state.pressedButtons.clear();
  }
  state.onCaptureChange?.(state.captured);
}

function onPointerLockError(): void {
  state.captured = false;
  state.onCaptureChange?.(false);
}

/** 押下中の全キー / マウスボタンに up を送って残留を解消する。 */
export function releaseAllInput(): void {
  for (const vk of state.pressedKeys) {
    dispatch({ Key: { vk, down: false } });
  }
  state.pressedKeys.clear();
  for (const button of state.pressedButtons) {
    dispatch({ MouseButton: { button, down: false } });
  }
  state.pressedButtons.clear();
}

function preventDefault(e: Event): void {
  e.preventDefault();
}

function toDisplayCoords(e: MouseEvent): { x: number; y: number } | null {
  const canvas = state.canvas;
  const display = state.display;
  if (!canvas || !display) return null;
  const rect = canvas.getBoundingClientRect();
  if (rect.width === 0 || rect.height === 0) return null;
  const nx = (e.clientX - rect.left) / rect.width;
  const ny = (e.clientY - rect.top) / rect.height;
  return {
    x: Math.round(nx * display.width),
    y: Math.round(ny * display.height),
  };
}

function mouseButton(button: number): "Left" | "Right" | "Middle" | null {
  switch (button) {
    case 0: return "Left";
    case 1: return "Middle";
    case 2: return "Right";
    default: return null;
  }
}

function dispatch(ev: InputEvent): void {
  if (!state.enabled) return;
  void sendInput(ev).catch((e) => console.warn("sendInput failed", e));
}

function onMouseMove(e: MouseEvent): void {
  if (state.captured) {
    // ポインタロック中は相対移動 (movementX/Y) を送る。
    const dx = e.movementX;
    const dy = e.movementY;
    if (dx === 0 && dy === 0) return;
    dispatch({ MouseMoveRelative: { dx, dy } });
    return;
  }
  const p = toDisplayCoords(e);
  if (!p) return;
  dispatch({ MouseMove: p });
}

function onMouseDown(e: MouseEvent): void {
  const b = mouseButton(e.button);
  if (!b) return;
  e.preventDefault();
  state.pressedButtons.add(b);
  dispatch({ MouseButton: { button: b, down: true } });
}

function onMouseUp(e: MouseEvent): void {
  const b = mouseButton(e.button);
  if (!b) return;
  e.preventDefault();
  state.pressedButtons.delete(b);
  dispatch({ MouseButton: { button: b, down: false } });
}

function onWheel(e: WheelEvent): void {
  e.preventDefault();
  const horizontal = Math.abs(e.deltaX) > Math.abs(e.deltaY);
  const raw = horizontal ? e.deltaX : -e.deltaY;
  // 120 単位の WHEEL_DELTA に正規化
  const delta = Math.round(raw / Math.max(1, Math.abs(raw)) * 120);
  if (delta === 0) return;
  dispatch({ MouseWheel: { delta, horizontal } });
}

function onKeyDown(e: KeyboardEvent): void {
  if (e.repeat) return;
  const vk = vkFromKeyboardEvent(e);
  if (vk == null) return;
  e.preventDefault();
  state.pressedKeys.add(vk);
  dispatch({ Key: { vk, down: true } });
}

function onKeyUp(e: KeyboardEvent): void {
  const vk = vkFromKeyboardEvent(e);
  if (vk == null) return;
  e.preventDefault();
  state.pressedKeys.delete(vk);
  dispatch({ Key: { vk, down: false } });
}

/** KeyboardEvent.code/key から Windows VK_* (おおまかなマッピング) を返す。 */
function vkFromKeyboardEvent(e: KeyboardEvent): number | null {
  // 英数
  if (/^Key[A-Z]$/.test(e.code)) return e.code.charCodeAt(3);                  // 'A'..'Z'
  if (/^Digit[0-9]$/.test(e.code)) return e.code.charCodeAt(5);                // '0'..'9'
  if (/^Numpad[0-9]$/.test(e.code)) return 0x60 + (e.code.charCodeAt(6) - 0x30); // VK_NUMPAD0..9
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(e.code)) {
    const n = parseInt(e.code.substring(1), 10);
    return 0x6F + n; // VK_F1=0x70 → 0x6F+1
  }
  switch (e.code) {
    case "Backspace": return 0x08;
    case "Tab": return 0x09;
    case "Enter":
    case "NumpadEnter": return 0x0D;
    case "ShiftLeft": return 0xA0;
    case "ShiftRight": return 0xA1;
    case "ControlLeft": return 0xA2;
    case "ControlRight": return 0xA3;
    case "AltLeft": return 0xA4;
    case "AltRight": return 0xA5;
    case "CapsLock": return 0x14;
    case "Escape": return 0x1B;
    case "Space": return 0x20;
    case "PageUp": return 0x21;
    case "PageDown": return 0x22;
    case "End": return 0x23;
    case "Home": return 0x24;
    case "ArrowLeft": return 0x25;
    case "ArrowUp": return 0x26;
    case "ArrowRight": return 0x27;
    case "ArrowDown": return 0x28;
    case "Insert": return 0x2D;
    case "Delete": return 0x2E;
    case "MetaLeft": return 0x5B;
    case "MetaRight": return 0x5C;
    case "NumpadMultiply": return 0x6A;
    case "NumpadAdd": return 0x6B;
    case "NumpadSubtract": return 0x6D;
    case "NumpadDecimal": return 0x6E;
    case "NumpadDivide": return 0x6F;
    case "Semicolon": return 0xBA;
    case "Equal": return 0xBB;
    case "Comma": return 0xBC;
    case "Minus": return 0xBD;
    case "Period": return 0xBE;
    case "Slash": return 0xBF;
    case "Backquote": return 0xC0;
    case "BracketLeft": return 0xDB;
    case "Backslash": return 0xDC;
    case "BracketRight": return 0xDD;
    case "Quote": return 0xDE;
    default: return null;
  }
}
