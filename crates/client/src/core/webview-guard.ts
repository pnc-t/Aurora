// WebView をネイティブアプリらしく振る舞わせるガード。
// ブラウザ既定の挙動 (右クリックメニュー / リロード / 印刷 / 検索 / devtools /
// ズーム / テキスト選択ドラッグ) を抑止する。
// リモートデスクトップでは、接続中のキー入力は input.ts がホストへ転送するため、
// ここでは preventDefault のみ行い stopPropagation はしない (転送を妨げない)。

import { matchHotkey, matchTabJump, type LocalHotkey } from "./shortcuts";

// 後方互換のため型を re-export (キー定義の正本は core/shortcuts.ts)。
export type { LocalHotkey };

/** Esc 2 回連打を「脱出」とみなす最大間隔 (ms)。 */
const ESC_DOUBLE_TAP_MS = 400;

export function installWebviewGuard(
  onHotkey?: (hotkey: LocalHotkey) => void,
  // 脱出した (= マウス解放/全画面解除した) なら true。何もしなければ false で Esc はホストへ通す。
  onEscapeBreakout?: () => boolean,
  // タブ直接ジャンプ (Ctrl+Alt+Shift+1〜9 固定)。n は 1 始まりのタブ番号。
  onTabJump?: (n: number) => void,
): void {
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

  // Esc 2 連打の検出用。前回 Esc の時刻を保持する。
  let lastEsc = 0;

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
      // タブ直接ジャンプ (固定バインディング)。ホットキー同様ホストへは転送しない。
      if (onTabJump) {
        const n = matchTabJump(e);
        if (n !== null) {
          e.preventDefault();
          e.stopImmediatePropagation();
          onTabJump(n);
          return;
        }
      }
      // Esc 2 連打 = ローカル「脱出」(マウス解放 + 全画面解除)。フルスクリーン中は
      // keyboard lock で Esc がホストへ転送され、ブラウザの「Esc で解除」も奪われるため、
      // 押下が遮断されない確実な脱出手段を別に用意する (オートリピートは無視)。
      // 1 回目は通常どおりホストへ送り (単押し Esc はホスト用)、2 回目「かつ実際に脱出した」
      // ときだけ握ってホストへ送らない。脱出対象が無い (キャプチャ/全画面でない) 2 連打は
      // ホストへ素通しする (ホストへ素早く Esc×2 を送る用途を潰さない)。
      if (onEscapeBreakout && e.code === "Escape" && !e.repeat) {
        const now = performance.now();
        const isDouble = now - lastEsc <= ESC_DOUBLE_TAP_MS;
        lastEsc = now;
        if (isDouble && onEscapeBreakout()) {
          lastEsc = 0;
          e.preventDefault();
          e.stopImmediatePropagation();
          return;
        }
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
