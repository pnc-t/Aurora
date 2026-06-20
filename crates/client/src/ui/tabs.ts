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
  container.innerHTML = `
    <div class="tabstrip" data-tauri-drag-region>
      <div class="tabstrip-list" id="tabstrip-list"></div>
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

    el.addEventListener("click", (e) => {
      // × クリックは閉じる、それ以外は選択。
      if ((e.target as HTMLElement).closest(".tab-close")) {
        e.stopPropagation();
        opts.onClose(id);
        return;
      }
      opts.onSelect(id);
    });
    // 中クリックで閉じる (ブラウザ的)。
    el.addEventListener("auxclick", (e) => {
      if (e.button === 1) {
        e.preventDefault();
        opts.onClose(id);
      }
    });
    return el;
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
