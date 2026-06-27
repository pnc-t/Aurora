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
