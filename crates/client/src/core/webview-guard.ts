// WebView をネイティブアプリらしく振る舞わせるガード。
// ブラウザ既定の挙動 (右クリックメニュー / リロード / 印刷 / 検索 / devtools /
// ズーム / テキスト選択ドラッグ) を抑止する。
// リモートデスクトップでは、接続中のキー入力は input.ts がホストへ転送するため、
// ここでは preventDefault のみ行い stopPropagation はしない (転送を妨げない)。

/** ローカル UI 操作ホットキーの種別。 */
export type LocalHotkey = "toggle-stats" | "toggle-bar" | "toggle-fullscreen" | "toggle-capture";

export function installWebviewGuard(onHotkey?: (hotkey: LocalHotkey) => void): void {
  // 右クリックメニュー無効化。
  window.addEventListener("contextmenu", (e) => e.preventDefault());

  // Ctrl/Cmd + ホイールによるページズーム無効化。
  window.addEventListener(
    "wheel",
    (e) => {
      if (e.ctrlKey) e.preventDefault();
    },
    { passive: false, capture: true },
  );

  // ブラウザ既定のショートカット抑止 + ローカルホットキー処理 (capture 段階)。
  window.addEventListener(
    "keydown",
    (e) => {
      // Ctrl+Alt+Shift+<key> はローカル UI 操作に予約。ホストへ転送しない
      // (stopImmediatePropagation で input.ts の window ハンドラより手前で握る)。
      const hotkey = localHotkey(e);
      if (hotkey) {
        e.preventDefault();
        e.stopImmediatePropagation();
        onHotkey?.(hotkey);
        return;
      }
      if (shouldBlock(e)) e.preventDefault();
    },
    { capture: true },
  );
}

/** ローカルホットキーを判定する。Ctrl+Alt+Shift+<key> 名前空間 + F11 単独。 */
function localHotkey(e: KeyboardEvent): LocalHotkey | null {
  // F11 単独は標準のフルスクリーンキーとして転用。
  if (e.key === "F11") return "toggle-fullscreen";

  const mod = e.ctrlKey || e.metaKey;
  if (!mod || !e.altKey || !e.shiftKey) return null;
  switch (e.code) {
    case "KeyS": return "toggle-stats";
    case "KeyB": return "toggle-bar";
    case "KeyF": return "toggle-fullscreen";
    case "KeyM": return "toggle-capture";
    default: return null;
  }
}

/** ローカルで握りつぶすべきブラウザショートカットか判定する。 */
function shouldBlock(e: KeyboardEvent): boolean {
  // 単独ファンクション: リロード(F5) / フルスクリーン(F11) / devtools(F12)。
  if (e.key === "F5" || e.key === "F11" || e.key === "F12") return true;

  const mod = e.ctrlKey || e.metaKey;
  if (!mod) return false;

  const key = e.key.toUpperCase();
  // devtools: Ctrl/Cmd + Shift + I/J/C。
  if (e.shiftKey && (key === "I" || key === "J" || key === "C")) return true;
  // リロード / 印刷 / 保存 / 検索 / 履歴ジャンプ / ソース表示 / devtools(J)。
  if (["R", "P", "S", "F", "G", "U", "J"].includes(key)) return true;
  // ズーム: Ctrl/Cmd + + / - / 0 / =。
  if (["+", "-", "0", "="].includes(e.key)) return true;

  return false;
}
