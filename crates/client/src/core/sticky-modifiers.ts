// 修飾キー (Ctrl/Alt/Shift/Win) のスティッキー状態機械。
// パッド (ui/mobile-pad.ts) とオンスクリーンキーボード (ui/mobile-keyboard.ts) は
// 別々の UI だが同じ修飾キーをホストへ送るため、片方で once/locked にした修飾を
// もう片方の消費 (consumeOnce) が知らないとホスト側に押しっぱなしが残る。
// そのため状態を 1 箇所 (このモジュール) に集約し、両 UI が同じインスタンスを共有する。

export type ModKey = "ctrl" | "alt" | "shift" | "win";
export type ModState = "off" | "once" | "locked";

// VK 値 (Windows 仮想キーコード)。
const VK_SHIFT = 0x10;
const VK_CONTROL = 0x11;
const VK_MENU = 0x12; // Alt
const VK_LWIN = 0x5b;

export const MOD_KEYS: readonly ModKey[] = ["ctrl", "alt", "shift", "win"];
export const MOD_VK: Record<ModKey, number> = { ctrl: VK_CONTROL, alt: VK_MENU, shift: VK_SHIFT, win: VK_LWIN };
export const MOD_LABEL: Record<ModKey, string> = { ctrl: "Ctrl", alt: "Alt", shift: "Shift", win: "Win" };

// 修飾キーの長押しでスティッキーを "locked" に固定するまでの時間。
const MOD_LONG_PRESS_MS = 500;

export type StickyModifiers = {
  /** 現在の状態。 */
  state: (key: ModKey) => ModState;
  /** 押下保持中 (once または locked) か。 */
  isHeld: (key: ModKey) => boolean;
  /** ボタン押下開始 (UI の pointerdown から呼ぶ)。長押し 500ms で locked へ確定する。 */
  pressStart: (key: ModKey) => void;
  /** ボタン押下終了 (pointerup)。短押しなら off⇄once / locked→off をトグルする。 */
  pressEnd: (key: ModKey) => void;
  /** 押下のキャンセル (pointercancel)。長押しタイマーだけ畳む。 */
  pressCancel: (key: ModKey) => void;
  /** "once" を全て消費して off へ戻す (キー送出やクリックの直後に呼ぶ)。 */
  consumeOnce: () => void;
  /** 全修飾を強制解除する (UI を隠すときなど、押しっぱなし残留を防ぐ)。 */
  releaseAll: () => void;
  /** 状態変化の購読。複数 UI (パッド/キーボード) が同じ状態を映すために使う。戻り値は解除関数。 */
  subscribe: (fn: (key: ModKey, state: ModState) => void) => () => void;
};

export function createStickyModifiers(sendKey: (vk: number, down: boolean) => void): StickyModifiers {
  const modState: Record<ModKey, ModState> = { ctrl: "off", alt: "off", shift: "off", win: "off" };
  const modLongPressTimers = new Map<ModKey, number>();
  const modLongPressFired = new Set<ModKey>();
  const listeners = new Set<(key: ModKey, state: ModState) => void>();

  function setModState(key: ModKey, state: ModState): void {
    modState[key] = state;
    for (const fn of listeners) fn(key, state);
  }

  function pressStart(key: ModKey): void {
    modLongPressFired.delete(key);
    const startState = modState[key];
    // locked からの長押しは未定義のため開始しない (locked は短押しでのみ off に戻す)。
    if (startState === "locked") return;
    const timer = window.setTimeout(() => {
      modLongPressFired.add(key);
      // "once" は既に down 済みなので送らない。"off" からはここで down を送る。
      if (startState === "off") sendKey(MOD_VK[key], true);
      setModState(key, "locked");
    }, MOD_LONG_PRESS_MS);
    modLongPressTimers.set(key, timer);
  }

  function pressEnd(key: ModKey): void {
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
      sendKey(MOD_VK[key], true);
      setModState(key, "once");
    } else {
      // "once" → off / "locked" → off はどちらも同じ (down 済みのキーを離す)。
      sendKey(MOD_VK[key], false);
      setModState(key, "off");
    }
  }

  function pressCancel(key: ModKey): void {
    const timer = modLongPressTimers.get(key);
    if (timer !== undefined) {
      clearTimeout(timer);
      modLongPressTimers.delete(key);
    }
    modLongPressFired.delete(key);
  }

  function consumeOnce(): void {
    for (const key of MOD_KEYS) {
      if (modState[key] === "once") {
        sendKey(MOD_VK[key], false);
        setModState(key, "off");
      }
    }
  }

  function releaseAll(): void {
    for (const key of MOD_KEYS) {
      const timer = modLongPressTimers.get(key);
      if (timer !== undefined) {
        clearTimeout(timer);
        modLongPressTimers.delete(key);
      }
      modLongPressFired.delete(key);
      if (modState[key] !== "off") {
        sendKey(MOD_VK[key], false);
        setModState(key, "off");
      }
    }
  }

  return {
    state: (key) => modState[key],
    isHeld: (key) => modState[key] !== "off",
    pressStart,
    pressEnd,
    pressCancel,
    consumeOnce,
    releaseAll,
    subscribe: (fn) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
  };
}
