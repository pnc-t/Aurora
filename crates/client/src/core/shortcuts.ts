// ローカル UI ショートカットの単一定義。
// キー判定 (webview-guard)・ツールバーのツールチップ (toolbar)・設定の一覧/編集 (launcher) は
// すべてここを参照する。バインディングの解析/整形/照合ロジックを純関数として集約し、
// 各所からの再利用と検証を容易にする。
//
// 既定は Ctrl+Alt+Shift+<code> (ホストへ転送せずローカル UI 操作に使う)。ユーザーは
// launcher の設定画面で各操作に別のキーを割り当てられる (ClientSettings.shortcuts、
// id → "Ctrl+Alt+KeyF" 形式の文字列)。割り当てが無い id は既定を使う。
// 加えて F11 単独をフルスクリーンの別キーとして扱う (matchHotkey で特例、変更不可)。

/** ローカルホットキーの種別。 */
export type LocalHotkey =
  | "toggle-stats"
  | "toggle-bar"
  | "toggle-fullscreen"
  | "toggle-capture"
  | "tab-prev"
  | "tab-next";

export interface ShortcutDef {
  id: LocalHotkey;
  /** 既定バインディングの <code> (KeyboardEvent.code)。修飾は既定で Ctrl+Alt+Shift。 */
  code: string;
  /** 操作の説明 (一覧の見出し)。 */
  desc: string;
}

export const SHORTCUTS: ShortcutDef[] = [
  { id: "toggle-fullscreen", code: "KeyF", desc: "フルスクリーン切替" },
  { id: "toggle-bar", code: "KeyB", desc: "ツールバー表示 / 格納" },
  { id: "toggle-capture", code: "KeyM", desc: "マウスキャプチャ切替" },
  { id: "toggle-stats", code: "KeyS", desc: "統計 HUD 表示 / 非表示" },
  { id: "tab-prev", code: "ArrowLeft", desc: "前のタブへ" },
  { id: "tab-next", code: "ArrowRight", desc: "次のタブへ" },
];

/**
 * タブ直接ジャンプ (Ctrl+Alt+Shift+1〜9、変更不可) の判定。該当すれば 1 始まりのタブ番号。
 * 設定可能な SHORTCUTS とは別枠の固定バインディングのため、validateBinding でも予約扱いにする。
 */
export function matchTabJump(e: KeyboardEvent): number | null {
  if (!e.ctrlKey || !e.altKey || !e.shiftKey || e.metaKey) return null;
  const m = /^Digit([1-9])$/.exec(e.code);
  return m ? Number(m[1]) : null;
}

/** 修飾キー + KeyboardEvent.code の組合せ。 */
export interface KeyBinding {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  meta: boolean;
  code: string;
}

/** 保存文字列の修飾表記順 (固定)。 */
const MOD_ORDER: (keyof Omit<KeyBinding, "code">)[] = ["ctrl", "alt", "shift", "meta"];
const MOD_LABEL: Record<(typeof MOD_ORDER)[number], string> = {
  ctrl: "Ctrl",
  alt: "Alt",
  shift: "Shift",
  meta: "Meta",
};

/** id の既定バインディング (Ctrl+Alt+Shift+<code>)。 */
export function defaultBinding(id: LocalHotkey): KeyBinding {
  const def = SHORTCUTS.find((s) => s.id === id);
  return { ctrl: true, alt: true, shift: true, meta: false, code: def?.code ?? "" };
}

/** KeyboardEvent から候補バインディングを組み立てる (記録 UI 用)。 */
export function bindingFromEvent(e: KeyboardEvent): KeyBinding {
  return { ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, meta: e.metaKey, code: e.code };
}

/** 修飾キー単体の code か (記録 UI が「まだ押されていない」と判定するのに使う)。 */
export function isModifierOnlyCode(code: string): boolean {
  return [
    "ControlLeft",
    "ControlRight",
    "AltLeft",
    "AltRight",
    "ShiftLeft",
    "ShiftRight",
    "MetaLeft",
    "MetaRight",
    "OSLeft",
    "OSRight",
  ].includes(code);
}

/** "Ctrl+Alt+KeyF" 形式の保存文字列を解析する。不正な形式は null。 */
export function parseBinding(s: string): KeyBinding | null {
  const parts = s.split("+").filter((p) => p.length > 0);
  if (parts.length === 0) return null;
  const code = parts[parts.length - 1];
  const b: KeyBinding = { ctrl: false, alt: false, shift: false, meta: false, code };
  for (const token of parts.slice(0, -1)) {
    switch (token) {
      case "Ctrl":
        b.ctrl = true;
        break;
      case "Alt":
        b.alt = true;
        break;
      case "Shift":
        b.shift = true;
        break;
      case "Meta":
        b.meta = true;
        break;
      default:
        return null;
    }
  }
  return b;
}

/** バインディングを保存文字列へ整形する (修飾は Ctrl/Alt/Shift/Meta の固定順)。 */
export function formatBinding(b: KeyBinding): string {
  const mods = MOD_ORDER.filter((m) => b[m]).map((m) => MOD_LABEL[m]);
  return [...mods, b.code].join("+");
}

/** code の表示用文字列 (KeyF→F, Digit1→1, F9→F9 等)。 */
function codeToDisplay(code: string): string {
  if (code.startsWith("Key") && code.length === 4) return code.slice(3);
  if (code.startsWith("Digit") && code.length === 6) return code.slice(5);
  const arrows: Record<string, string> = {
    ArrowLeft: "←",
    ArrowRight: "→",
    ArrowUp: "↑",
    ArrowDown: "↓",
  };
  return arrows[code] ?? code;
}

/** バインディングの表示用キー表記 (例: "Ctrl+Alt+F")。 */
export function bindingLabel(b: KeyBinding): string {
  const mods = MOD_ORDER.filter((m) => b[m]).map((m) => MOD_LABEL[m]);
  return [...mods, codeToDisplay(b.code)].join("+");
}

export function bindingsEqual(a: KeyBinding, b: KeyBinding): boolean {
  return a.ctrl === b.ctrl && a.alt === b.alt && a.shift === b.shift && a.meta === b.meta && a.code === b.code;
}

/** id の実効バインディング (map に登録があればそれ、無ければ既定)。 */
export function effectiveBinding(id: LocalHotkey, map: Record<string, string>): KeyBinding {
  const raw = map[id];
  const custom = raw ? parseBinding(raw) : null;
  return custom ?? defaultBinding(id);
}

/**
 * 記録したバインディングを検証する。拒否理由 (表示用の短い日本語) を返す。問題なければ null。
 * - 修飾に Ctrl/Alt/Meta のいずれも含まない (Shift のみ・修飾なしは通常入力を壊す)
 * - F11 / Escape (別機能で予約済み)
 * - 他のローカル操作 (id 以外) の実効バインディングと重複
 */
export function validateBinding(
  id: LocalHotkey,
  binding: KeyBinding,
  map: Record<string, string>,
): string | null {
  if (!binding.ctrl && !binding.alt && !binding.meta) {
    return "Ctrl / Alt / Meta のいずれかを含めてください";
  }
  if (binding.code === "F11") return "F11 は使用できません (フルスクリーン専用)";
  if (binding.code === "Escape") return "Esc は使用できません (マウスキャプチャ解除専用)";
  if (binding.ctrl && binding.alt && binding.shift && /^Digit[1-9]$/.test(binding.code)) {
    return "Ctrl+Alt+Shift+1〜9 はタブ切替に予約されています";
  }
  for (const other of SHORTCUTS) {
    if (other.id === id) continue;
    if (bindingsEqual(effectiveBinding(other.id, map), binding)) {
      return `「${other.desc}」と重複しています`;
    }
  }
  return null;
}

// --- 実行時バインディングテーブル (matchHotkey/shortcutLabel が参照) ---
let currentBindings: Map<LocalHotkey, KeyBinding> = new Map(
  SHORTCUTS.map((s) => [s.id, defaultBinding(s.id)]),
);

/** ClientSettings.shortcuts を実行時テーブルへ反映する。設定ロード/変更のたびに呼ぶ。 */
export function applyShortcutBindings(map: Record<string, string>): void {
  currentBindings = new Map(SHORTCUTS.map((s) => [s.id, effectiveBinding(s.id, map)]));
}

// launcher のショートカット記録 UI が入力を捕捉している間、matchHotkey を無効化する
// フラグ。webview-guard の window capture リスナーは記録 UI (document capture) より
// 先に発火するため、これが無いと「記録中に既存の割当と同じキーを押す」と matchHotkey が
// 先取りしてしまい、記録 UI 側が duplicate エラーを出せなくなる。
let recordingActive = false;

/** ショートカット記録 UI の開始/終了を通知する (launcher から呼ぶ)。 */
export function setRecordingActive(active: boolean): void {
  recordingActive = active;
}

/** KeyboardEvent からローカルホットキーを判定する (該当なしは null)。 */
export function matchHotkey(e: KeyboardEvent): LocalHotkey | null {
  if (recordingActive) return null;
  // オートリピートはローカル UI 操作としては無視する (キー判定はここが唯一の経路)。
  // 押しっぱなし中に toggleFullscreen 等を連発させないための入口ガード。
  // input.ts へのホスト転送は別の window リスナー (このガードの対象外) が担うため、
  // 通常のリモート入力のキーリピートはここでは弾かれず素通りする。
  if (e.repeat) return null;
  // F11 単独は標準のフルスクリーンキーとして転用 (変更不可)。
  if (e.key === "F11") return "toggle-fullscreen";
  for (const s of SHORTCUTS) {
    const b = currentBindings.get(s.id);
    if (!b) continue;
    if (
      b.code === e.code &&
      b.ctrl === e.ctrlKey &&
      b.alt === e.altKey &&
      b.shift === e.shiftKey &&
      b.meta === e.metaKey
    ) {
      return s.id;
    }
  }
  return null;
}

/** id から表示用キー表記を引く (ツールチップ用)。toggle-fullscreen は "F11 / <binding>" 併記。 */
export function shortcutLabel(id: LocalHotkey): string {
  const b = currentBindings.get(id) ?? defaultBinding(id);
  const label = bindingLabel(b);
  return id === "toggle-fullscreen" ? `F11 / ${label}` : label;
}
