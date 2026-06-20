// capture ウィンドウ上部のタブストリップ。複数セッション (= 接続先) をタブで切り替える。
// 各タブ: 接続状態ドット + ラベル + 閉じる(×)。末尾に新規接続用の「+」。
// ラベルはホスト由来文字列を含むため textContent で描画する (XSS 注意)。

import { icon } from "./icons";
import type { TabStatus } from "../core/session";
import "./tabs.css";

type TabsOpts = {
  /** タブ選択 (アクティブ切替)。 */
  onSelect: (id: string) => void;
  /** タブを閉じる (× / 中クリック)。 */
  onClose: (id: string) => void;
  /** 「+」で新規接続を追加。 */
  onAdd: () => void;
  /** タブをウィンドウ外へドラッグして離した (別ウィンドウへ分割)。座標は画面 px。 */
  onDetach: (id: string, screenX: number, screenY: number) => void;
};

export type TabsHandle = {
  add: (id: string, label: string, status: TabStatus) => void;
  remove: (id: string) => void;
  update: (id: string, label: string, status: TabStatus) => void;
  setActive: (id: string) => void;
  /** タブ本数 (窓を閉じる判定などに使う)。 */
  count: () => number;
};

export function mountTabs(container: HTMLElement, opts: TabsOpts): TabsHandle {
  // data-tauri-drag-region は **クリック対象そのもの** に属性が無いとドラッグ移動しない
  // (Tauri v2 は exact target 判定)。tabstrip-list が flex:1 で空き領域を覆うため、
  // list 自身にも付けないとタブ以外の余白でウィンドウを掴めない。タブ/ボタンは子要素
  // (属性無し) なので掴み対象から外れる。
  container.innerHTML = `
    <div class="tabstrip" data-tauri-drag-region>
      <div class="tabstrip-list" id="tabstrip-list" data-tauri-drag-region></div>
      <button type="button" class="tab-add" id="tab-add" title="接続を追加">${icon("add")}</button>
    </div>
  `;
  const list = container.querySelector<HTMLElement>("#tabstrip-list");
  container.querySelector<HTMLButtonElement>("#tab-add")?.addEventListener("click", () => opts.onAdd());

  // id → タブ要素。
  const tabs = new Map<string, HTMLElement>();
  let activeId: string | null = null;

  function makeTab(id: string, label: string, status: TabStatus): HTMLElement {
    const el = document.createElement("div");
    el.className = "tab";
    el.dataset.id = id;

    const dot = document.createElement("span");
    dot.className = `tab-dot ${status}`;

    const name = document.createElement("span");
    name.className = "tab-label";
    name.textContent = label;

    const close = document.createElement("button");
    close.type = "button";
    close.className = "tab-close";
    close.title = "閉じる";
    close.innerHTML = icon("close");

    el.append(dot, name, close);

    // × クリックで閉じる (ドラッグ開始の pointerdown より前に拾えるよう close 自身に付ける)。
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      opts.onClose(id);
    });
    // 中クリックで閉じる (ブラウザ的)。
    el.addEventListener("auxclick", (e) => {
      if (e.button === 1) {
        e.preventDefault();
        opts.onClose(id);
      }
    });
    // 左ボタン: ドラッグ (ウィンドウ外で別窓へ分割)。動かなければクリック扱いで選択。
    el.addEventListener("pointerdown", (e) => {
      if (e.button !== 0) return;
      if ((e.target as HTMLElement).closest(".tab-close")) return; // × は閉じる
      e.preventDefault();
      startTabDrag(id, e, el);
    });
    return el;
  }

  /**
   * タブのドラッグ。動かさず離せば選択、ウィンドウ外で離せば別ウィンドウへ分割 (onDetach)。
   * HTML5 DnD は WebView2 で不安定なため pointer イベントで自前実装する
   * (launcher.ts:startDrag と同方針)。
   */
  function startTabDrag(id: string, down: PointerEvent, el: HTMLElement): void {
    const startX = down.clientX;
    const startY = down.clientY;
    let dragging = false;
    let clone: HTMLElement | null = null;
    el.setPointerCapture(down.pointerId);

    const onMove = (ev: PointerEvent): void => {
      if (!dragging) {
        if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < 6) return;
        dragging = true;
        const r = el.getBoundingClientRect();
        clone = el.cloneNode(true) as HTMLElement;
        clone.classList.add("tab-drag-clone");
        clone.style.width = `${r.width}px`;
        document.body.appendChild(clone);
        el.classList.add("tab-dragging");
      }
      if (clone) {
        clone.style.left = `${ev.clientX - 24}px`;
        clone.style.top = `${ev.clientY - 14}px`;
      }
    };
    const onUp = (ev: PointerEvent): void => {
      el.releasePointerCapture(down.pointerId);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      if (clone) clone.remove();
      el.classList.remove("tab-dragging");
      if (!dragging) {
        opts.onSelect(id); // 動いていない = クリック
        return;
      }
      const outside =
        ev.clientX < 0 ||
        ev.clientY < 0 ||
        ev.clientX > window.innerWidth ||
        ev.clientY > window.innerHeight;
      if (outside) {
        opts.onDetach(id, window.screenX + ev.clientX, window.screenY + ev.clientY);
      }
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  return {
    add(id, label, status): void {
      if (tabs.has(id)) return;
      const el = makeTab(id, label, status);
      tabs.set(id, el);
      list?.appendChild(el);
    },
    remove(id): void {
      const el = tabs.get(id);
      if (el) {
        el.remove();
        tabs.delete(id);
      }
      if (activeId === id) activeId = null;
    },
    update(id, label, status): void {
      const el = tabs.get(id);
      if (!el) return;
      const name = el.querySelector<HTMLElement>(".tab-label");
      if (name) name.textContent = label;
      const dot = el.querySelector<HTMLElement>(".tab-dot");
      if (dot) dot.className = `tab-dot ${status}`;
    },
    setActive(id): void {
      activeId = id;
      tabs.forEach((el, key) => el.classList.toggle("active", key === id));
    },
    count(): number {
      return tabs.size;
    },
  };
}
