// モバイル共通のアクションシート (画面下端から立ち上がるメニュー)。
// カードの ⋯ 等から開き、選んだ操作によっては続けてダイアログ (mobile-wol.ts の
// #mwol-dialog-backdrop 等、z-index 60) を開く導線があるため、このシート自身は
// それより上の z-index 62 に置く。開閉の作法 (trapFocus / Escape / backdrop タップ) は
// mobile-wol.ts のダイアログ実装に合わせる。
//
// ラベルはユーザー入力を含みうるので textContent で描画する (XSS 注意、CLAUDE.md 規約)。
// 固定 SVG アイコン (icon() の戻り値) のみ innerHTML で可。

import "./mobile-sheet.css";

import { trapFocus } from "../core/a11y";
import { icon, type IconName } from "./icons";

export type SheetAction = {
  /** 項目のラベル (ユーザー入力を含みうるので textContent で描画すること)。 */
  label: string;
  /** ui/icons.ts の icon() に渡す名前。省略時はアイコン無し。 */
  icon?: IconName;
  /** 破壊的操作 (削除など)。danger 色で描く。 */
  danger?: boolean;
  onSelect: () => void;
};

// 同時に 2 枚開かないための現在開いているシートの後始末関数。
let closeCurrent: (() => void) | null = null;

/** 画面下端から立ち上がるアクションシートを開く。項目タップでシートを閉じてから onSelect を呼ぶ。 */
export function openActionSheet(actions: SheetAction[]): void {
  closeCurrent?.();

  const backdrop = document.createElement("div");
  backdrop.className = "ms-backdrop";

  const sheet = document.createElement("div");
  sheet.className = "ms-sheet";
  sheet.setAttribute("role", "menu");

  const list = document.createElement("div");
  list.className = "ms-list";

  function close(): void {
    releaseTrap();
    document.removeEventListener("keydown", onKeyDown);
    backdrop.remove();
    closeCurrent = null;
  }
  closeCurrent = close;

  function selectAction(a: SheetAction): void {
    // 呼び出し側が続けてダイアログを開くため、シートが残っていると二重に重なる。
    // 先に閉じてから onSelect を呼ぶ。
    close();
    a.onSelect();
  }

  for (const a of actions) {
    const item = document.createElement("button");
    item.type = "button";
    item.className = "ms-item";
    if (a.danger) item.classList.add("ms-danger");
    item.setAttribute("role", "menuitem");
    if (a.icon) {
      const iconWrap = document.createElement("span");
      iconWrap.className = "ms-item-icon";
      iconWrap.innerHTML = icon(a.icon);
      item.appendChild(iconWrap);
    }
    const labelEl = document.createElement("span");
    labelEl.className = "ms-item-label";
    labelEl.textContent = a.label;
    item.appendChild(labelEl);
    item.addEventListener("click", () => selectAction(a));
    list.appendChild(item);
  }

  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "ms-cancel";
  cancel.setAttribute("role", "menuitem");
  cancel.textContent = "キャンセル";
  cancel.addEventListener("click", () => close());

  sheet.append(list, cancel);
  backdrop.appendChild(sheet);
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) close();
  });

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === "Escape") close();
  }
  document.addEventListener("keydown", onKeyDown);

  document.body.appendChild(backdrop);
  const releaseTrap = trapFocus(sheet);
}
