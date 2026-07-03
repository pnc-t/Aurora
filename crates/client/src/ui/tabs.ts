// capture ウィンドウ上部のタブストリップ。複数セッション (= 接続先) をタブで切り替える。
// 各タブ: 接続状態ドット + ラベル + 閉じる(×)。末尾に新規接続用の「+」。
// ラベルはホスト由来文字列を含むため textContent で描画する (XSS 注意)。

import { icon } from "./icons";
import { applyAriaLabels } from "../core/a11y";
import type { TabStatus } from "../core/session";
import type { ToolbarPosition } from "../core/ipc";
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
  /** フルスクリーン状態を伝える。on の間はストリップを完全に隠す (端ホバーでも出さない)。 */
  setFullscreen: (on: boolean) => void;
  /** タブストリップの配置を切り替える (top/bottom = 横帯、left/right = 縦サイドタブ)。 */
  setPosition: (pos: ToolbarPosition) => void;
};

// 縦配置 (左右) のサイドバー折りたたみ。窓を跨いで記憶したいだけの UI 選好なので
// Rust 設定ではなく localStorage に持つ。
const COLLAPSE_KEY = "aurora.tabstrip.collapsed";

export function mountTabs(container: HTMLElement, opts: TabsOpts): TabsHandle {
  // data-tauri-drag-region は **クリック対象そのもの** に属性が無いとドラッグ移動しない
  // (Tauri v2 は exact target 判定)。spacer が余白を担う要素なのでそこに付ける。
  // list はタブで埋まり空き領域を持たないため付けない。
  container.innerHTML = `
    <div class="tabstrip" data-tauri-drag-region>
      <button type="button" class="tab-collapse" id="tab-collapse" title="タブバーを折りたたむ">${icon("chevron")}</button>
      <div class="tabstrip-list" id="tabstrip-list"></div>
      <!-- 横配置では .tab-add-label が display:none になり、applyAriaLabels も textContent
           非空 (非表示でも) と判定して補わないため、aria-label を明示して名前を確定させる。 -->
      <button type="button" class="tab-add" id="tab-add" title="接続を追加" aria-label="接続を追加">${icon("add")}<span class="tab-add-label">新しいタブ</span></button>
      <div class="tabstrip-spacer" data-tauri-drag-region></div>
    </div>
  `;
  const list = container.querySelector<HTMLElement>("#tabstrip-list");
  const strip = container.querySelector<HTMLElement>(".tabstrip");
  const collapseBtn = container.querySelector<HTMLButtonElement>("#tab-collapse");
  container.querySelector<HTMLButtonElement>("#tab-add")?.addEventListener("click", () => opts.onAdd());
  applyAriaLabels(container); // 「+」など アイコンのみボタンに aria-label を補う

  // id → タブ要素。
  const tabs = new Map<string, HTMLElement>();
  let activeId: string | null = null;

  // フルスクリーン中はストリップを完全に隠す (端ホバーでも出さない)。ツールバーピルと違い、
  // タブは没入時に不要なため一時表示もしない。
  let position: ToolbarPosition = "top";
  let collapsed = localStorage.getItem(COLLAPSE_KEY) === "1";

  function collapseTitle(): string {
    return collapsed ? "タブバーを展開" : "タブバーを折りたたむ";
  }

  function applyPosition(): void {
    container.classList.remove("pos-top", "pos-bottom", "pos-left", "pos-right");
    container.classList.add(`pos-${position}`);
    container.classList.toggle("collapsed", collapsed); // 横配置にも付くが CSS は縦配置でのみ参照する
    document.body.classList.remove("tab-top", "tab-bottom", "tab-left", "tab-right");
    document.body.classList.add(`tab-${position}`);
    // 横帯 (上/下) のときだけ実測のストリップ厚を公開し、統計 HUD をその下へ逃がす
    // (stats-overlay.css の body.tab-top 規則が参照)。
    if ((position === "top" || position === "bottom") && strip) {
      document.documentElement.style.setProperty("--tabstrip-h", `${strip.offsetHeight}px`);
    }
    // 縦配置 (左右) のときだけ幅を公開する。映像 (#screen-wrap) やツールバー/統計 HUD の
    // 逃げ幅が折りたたみ状態に追従するよう documentElement に置く (tabs.css がフォールバックのみ持つ)。
    if (position === "left" || position === "right") {
      document.documentElement.style.setProperty("--tabstrip-w", collapsed ? "44px" : "200px");
    } else {
      document.documentElement.style.removeProperty("--tabstrip-w");
    }
    if (collapseBtn) {
      const title = collapseTitle();
      collapseBtn.title = title;
      collapseBtn.setAttribute("aria-label", title);
    }
  }
  applyPosition();

  collapseBtn?.addEventListener("click", () => {
    collapsed = !collapsed;
    if (collapsed) {
      localStorage.setItem(COLLAPSE_KEY, "1");
    } else {
      localStorage.removeItem(COLLAPSE_KEY);
    }
    applyPosition();
  });

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
    applyAriaLabels(el); // × ボタン (アイコンのみ) に aria-label を補う

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
    // 掴んだ点とタブ左上の相対位置 (grab offset)。クローンをこの分だけずらして追従させると、
    // タブの向き/形状 (横帯の小タブ・縦サイドタブの全幅タブ) に関わらず「掴んだ箇所」が
    // カーソルに張り付く。固定オフセットだと top の横タブ基準になり、bottom/left/right で
    // 掴み位置とクローンがズレて崩れて見える。
    const grab = el.getBoundingClientRect();
    const grabX = startX - grab.left;
    const grabY = startY - grab.top;
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
        clone.style.height = `${r.height}px`;
        document.body.appendChild(clone);
        el.classList.add("tab-dragging");
      }
      if (clone) {
        clone.style.left = `${ev.clientX - grabX}px`;
        clone.style.top = `${ev.clientY - grabY}px`;
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
    setFullscreen(on): void {
      // フルスクリーン中はタブストリップを完全に隠す (不透明な帯が映像を覆い黒帯を生むため)。
      container.classList.toggle("hidden", on);
      // 縦サイドタブの映像寄せ (CSS) をフルスクリーン中だけ無効化するためのフラグ。
      document.body.classList.toggle("tab-fs", on);
    },
    setPosition(pos): void {
      position = pos;
      applyPosition();
    },
  };
}
