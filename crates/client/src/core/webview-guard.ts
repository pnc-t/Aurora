// WebView をネイティブアプリらしく振る舞わせるガード。
// ブラウザ既定の挙動 (右クリックメニュー / リロード / 印刷 / 検索 / devtools /
// ズーム / テキスト選択ドラッグ) を抑止する。
// リモートデスクトップでは、接続中のキー入力は input.ts がホストへ転送するため、
// ここでは preventDefault のみ行い stopPropagation はしない (転送を妨げない)。

import { matchHotkey, type LocalHotkey } from "./shortcuts";

// 後方互換のため型を re-export (キー定義の正本は core/shortcuts.ts)。
export type { LocalHotkey };

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
      const hotkey = matchHotkey(e);
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
