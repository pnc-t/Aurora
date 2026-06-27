// アクセシビリティの小道具。
// アイコンのみのボタン (SVG だけで可視テキストを持たない) は title 属性しか持たず、
// スクリーンリーダーが内容を読み上げられない。title を aria-label として補い、
// 読み上げ可能にする。可視テキストを持つ要素や既に aria-label がある要素は触らない。

/** root 配下で title だけを持つアイコン要素に aria-label を補う。 */
export function applyAriaLabels(root: ParentNode): void {
  root.querySelectorAll<HTMLElement>("[title]").forEach((el) => {
    if (el.getAttribute("aria-label")) return;
    const title = el.getAttribute("title")?.trim();
    if (!title) return;
    // 可視テキストがある要素は不要 (テキスト自体が読み上げられる)。
    if ((el.textContent ?? "").trim().length > 0) return;
    el.setAttribute("aria-label", title);
  });
}

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), ' +
  'textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/**
 * モーダル内に Tab フォーカスを閉じ込める (フォーカストラップ)。
 * Tab / Shift+Tab が末端を超えたら反対側へ巻き戻し、背後の要素へ抜けさせない。
 * 戻り値を呼ぶとリスナを解除する (ダイアログを閉じるときに呼ぶ)。
 */
export function trapFocus(container: HTMLElement): () => void {
  function onKey(e: KeyboardEvent): void {
    if (e.key !== "Tab") return;
    const items = Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
      (el) => el.offsetParent !== null, // 非表示要素を除く
    );
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement;
    if (e.shiftKey) {
      if (active === first || !container.contains(active)) {
        e.preventDefault();
        last.focus();
      }
    } else if (active === last || !container.contains(active)) {
      e.preventDefault();
      first.focus();
    }
  }
  container.addEventListener("keydown", onKey);
  return () => container.removeEventListener("keydown", onKey);
}
