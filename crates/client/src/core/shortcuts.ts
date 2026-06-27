// ローカル UI ショートカットの単一定義。
// キー判定 (webview-guard)・ツールバーのツールチップ (toolbar)・設定の一覧 (launcher) は
// すべてここを参照する。キーや表記を変えるときはこの 1 ファイルだけ触ればよい。
//
// 予約名前空間: Ctrl+Alt+Shift+<code> (ホストへ転送せずローカル UI 操作に使う)。
// 加えて F11 単独をフルスクリーンの別キーとして扱う (matchHotkey で特例)。

/** ローカルホットキーの種別。 */
export type LocalHotkey = "toggle-stats" | "toggle-bar" | "toggle-fullscreen" | "toggle-capture";

export interface ShortcutDef {
  id: LocalHotkey;
  /** Ctrl+Alt+Shift+<code> の <code> (KeyboardEvent.code)。 */
  code: string;
  /** 表示用のキー表記 (ツールチップ / 一覧)。 */
  label: string;
  /** 操作の説明 (一覧の見出し)。 */
  desc: string;
}

export const SHORTCUTS: ShortcutDef[] = [
  { id: "toggle-fullscreen", code: "KeyF", label: "F11 / Ctrl+Alt+Shift+F", desc: "フルスクリーン切替" },
  { id: "toggle-bar", code: "KeyB", label: "Ctrl+Alt+Shift+B", desc: "ツールバー表示 / 格納" },
  { id: "toggle-capture", code: "KeyM", label: "Ctrl+Alt+Shift+M", desc: "マウスキャプチャ切替" },
  { id: "toggle-stats", code: "KeyS", label: "Ctrl+Alt+Shift+S", desc: "統計 HUD 表示 / 非表示" },
];

/** KeyboardEvent からローカルホットキーを判定する (該当なしは null)。 */
export function matchHotkey(e: KeyboardEvent): LocalHotkey | null {
  // F11 単独は標準のフルスクリーンキーとして転用。
  if (e.key === "F11") return "toggle-fullscreen";
  const mod = e.ctrlKey || e.metaKey;
  if (!mod || !e.altKey || !e.shiftKey) return null;
  return SHORTCUTS.find((s) => s.code === e.code)?.id ?? null;
}

/** id から表示用キー表記を引く (ツールチップ用)。 */
export function shortcutLabel(id: LocalHotkey): string {
  return SHORTCUTS.find((s) => s.id === id)?.label ?? "";
}
