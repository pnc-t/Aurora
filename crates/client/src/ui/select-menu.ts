// ネイティブ `<select>` を自作ドロップダウンに差し替える汎用ヘルパー。
// ツールバーは既に自作メニューに統一済みで、ネイティブ select だけ OS 依存の見た目
// (Windows のグレー枠・角丸なし) が残り浮いていたため、同じ見た目に揃える。
//
// 既存の `<select>` は **DOM に残したまま隠して** 上に UI を被せる方式にしている。
// 各呼び出し側は `sel.value` の読み書きと `change` イベントで実装済みで、それらを
// 書き換えずに済むため (option を後から足す wol-panel も含め) 差分が小さい。

import { icon } from "./icons";
import "./select-menu.css";

/** 開いているパネルは常に 1 つ。次を開く前に閉じる。 */
let openPanel: (() => void) | null = null;

function closeOpenPanel(): void {
  openPanel?.();
  openPanel = null;
}

/**
 * root 配下の `select.select-input` を自作ドロップダウンに差し替える。
 * 二重適用は無視するので、DOM を差し替えるたびに呼んでよい。
 */
export function enhanceSelects(root: ParentNode): void {
  for (const sel of root.querySelectorAll<HTMLSelectElement>("select.select-input")) {
    if (sel.dataset.enhanced) continue;
    sel.dataset.enhanced = "1";
    enhanceOne(sel);
  }
}

function enhanceOne(sel: HTMLSelectElement): void {
  const wrap = document.createElement("span");
  wrap.className = "sel-wrap";
  sel.parentNode?.insertBefore(wrap, sel);
  wrap.appendChild(sel);

  const trigger = document.createElement("button");
  trigger.type = "button";
  trigger.className = "sel-trigger";
  trigger.setAttribute("aria-haspopup", "listbox");
  trigger.setAttribute("aria-expanded", "false");
  const label = document.createElement("span");
  label.className = "sel-trigger-label";
  const caret = document.createElement("span");
  caret.className = "sel-trigger-caret";
  caret.innerHTML = icon("down"); // 固定 SVG のみなので innerHTML で問題ない
  trigger.append(label, caret);
  wrap.appendChild(trigger);

  // ラベルはホスト由来文字列 (WoL の紐付け先など) があり得るため textContent で入れる。
  const syncLabel = (): void => {
    label.textContent = sel.selectedOptions[0]?.textContent ?? "";
  };
  syncLabel();

  // 呼び出し側は設定値の反映を `sel.value = ...` で行う (change は飛ばない) ため、
  // setter を挟んでラベルを追従させる。これで既存コードを 1 行も変えずに済む。
  const desc = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value");
  if (desc?.get && desc.set) {
    const { get, set } = desc;
    Object.defineProperty(sel, "value", {
      configurable: true,
      get: () => get.call(sel),
      set: (v: string) => {
        set.call(sel, v);
        syncLabel();
      },
    });
  }
  sel.addEventListener("change", syncLabel);

  trigger.addEventListener("click", () => {
    if (wrap.classList.contains("open")) {
      closeOpenPanel();
      return;
    }
    openMenu(sel, wrap, trigger, syncLabel);
  });
  trigger.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!wrap.classList.contains("open")) openMenu(sel, wrap, trigger, syncLabel);
    }
  });
}

function openMenu(
  sel: HTMLSelectElement,
  wrap: HTMLElement,
  trigger: HTMLButtonElement,
  syncLabel: () => void,
): void {
  closeOpenPanel();

  const panel = document.createElement("div");
  panel.className = "sel-panel";
  panel.setAttribute("role", "listbox");

  // option は後から差し替わりうる (WoL の紐付け先など) ので、開くたびに作り直す。
  const items = [...sel.options].map((opt) => {
    const item = document.createElement("button");
    item.type = "button";
    item.className = "sel-item";
    item.setAttribute("role", "option");
    item.setAttribute("aria-selected", String(opt.selected));
    const check = document.createElement("span");
    check.className = "sel-item-check";
    check.innerHTML = icon("check"); // 固定 SVG のみ
    const text = document.createElement("span");
    text.className = "sel-item-label";
    text.textContent = opt.textContent ?? "";
    item.append(check, text);
    item.addEventListener("click", () => {
      if (sel.value !== opt.value) {
        sel.value = opt.value;
        sel.dispatchEvent(new Event("change", { bubbles: true }));
      }
      syncLabel();
      close();
      trigger.focus();
    });
    return item;
  });
  panel.append(...items);
  // ダイアログ / 設定ページは overflow: auto なので、内側に置くと下端で切れる。
  // body 直下の fixed レイヤーに出し、位置はトリガの矩形から決める。
  document.body.appendChild(panel);
  wrap.classList.add("open");
  trigger.setAttribute("aria-expanded", "true");

  const anchor = trigger.getBoundingClientRect();
  panel.style.minWidth = `${anchor.width}px`;
  const size = panel.getBoundingClientRect();
  // 右辺をトリガに揃え、画面外へはみ出す分だけ内側へ寄せる。
  const left = Math.max(8, Math.min(anchor.right - size.width, window.innerWidth - size.width - 8));
  // 下に入りきらなければ上に開く。
  const below = anchor.bottom + 4;
  const top = below + size.height > window.innerHeight - 8 ? anchor.top - size.height - 4 : below;
  panel.style.left = `${left}px`;
  panel.style.top = `${Math.max(8, top)}px`;

  let cursor = Math.max(0, sel.selectedIndex);
  const focusAt = (i: number): void => {
    cursor = (i + items.length) % items.length;
    items[cursor]?.focus();
  };
  focusAt(cursor);

  const onKey = (e: KeyboardEvent): void => {
    if (e.key === "Escape") {
      // メニューだけ閉じる。伝播させるとダイアログごと閉じてしまう。
      e.preventDefault();
      e.stopPropagation();
      close();
      trigger.focus();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      focusAt(cursor + 1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      focusAt(cursor - 1);
    } else if (e.key === "Tab") {
      // 項目はダイアログ外 (body 直下) にあり、そのまま Tab させるとフォーカスが
      // トラップの外へ抜ける。閉じてトリガへ戻し、そこから移動させる。
      e.preventDefault();
      close();
      trigger.focus();
    }
  };
  // capture 段で拾う: ダイアログ側の Escape ハンドラより先にメニューだけを閉じる。
  document.addEventListener("keydown", onKey, true);

  const onPointerDown = (e: PointerEvent): void => {
    const t = e.target as Node;
    // パネルは body 直下にあるので wrap だけでは「外側」判定を誤る (項目クリックが
    // click より先に閉じてしまう)。両方を外側判定から除く。
    if (!wrap.contains(t) && !panel.contains(t)) close();
  };
  document.addEventListener("pointerdown", onPointerDown, true);
  // fixed 配置はスクロール / リサイズに追従しないので、ずれる前に閉じる。
  const onReflow = (): void => close();
  window.addEventListener("scroll", onReflow, true);
  window.addEventListener("resize", onReflow);

  function close(): void {
    document.removeEventListener("keydown", onKey, true);
    document.removeEventListener("pointerdown", onPointerDown, true);
    window.removeEventListener("scroll", onReflow, true);
    window.removeEventListener("resize", onReflow);
    panel.remove();
    wrap.classList.remove("open", "flip-up");
    trigger.setAttribute("aria-expanded", "false");
    if (openPanel === close) openPanel = null;
  }
  openPanel = close;
}
