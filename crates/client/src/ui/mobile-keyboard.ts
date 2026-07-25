// モバイル専用の自前オンスクリーンキーボード。
// 従来は Android IME の隠し input 経由で文字を VK へ逆変換していたが、
// IME が日本語モードだと入力が丸ごと捨てられる・Ctrl/Alt/Win/F キーが打てない等の構造的な穴が
// あった。本プロジェクトは「ホスト側 IME 方式」(クライアントで変換せずローマ字キーストローク
// をそのまま送り、ホストの IME に変換させる) のため、キーボードは文字ではなく VK を送るだけで
// よい。押した物理キーをそのまま VK で送る構成にすることで IME モードの影響を受けなくする。

import { MOD_KEYS, MOD_LABEL, type ModKey, type StickyModifiers } from "../core/sticky-modifiers";
import "./mobile-keyboard.css";

export type MobileKeyboardOpts = {
  root: HTMLElement;
  /** VK を 1 つ送る (down/up の組み立てはこのコンポーネント側で行う)。 */
  sendKey: (vk: number, down: boolean) => void;
  /** 修飾キーの共有ストア。オンスクリーンコントローラ (ui/mobile-pad.ts) と状態を共有するため、
   *  このコンポーネントは修飾キーの状態を自前で持たない。 */
  mods: StickyModifiers;
  /** 開閉が変わったときに通知する (下部バーのトグル表示の同期用)。 */
  onOpenChange?: (open: boolean) => void;
};

export type MobileKeyboardHandle = {
  open: () => void;
  close: () => void;
  isOpen: () => boolean;
};

// VK 値 (Windows 仮想キーコード)。
const VK_BACK = 0x08;
const VK_TAB = 0x09;
const VK_RETURN = 0x0d;
const VK_ESCAPE = 0x1b;
const VK_SPACE = 0x20;
const VK_PRIOR = 0x21; // PageUp
const VK_NEXT = 0x22; // PageDown
const VK_END = 0x23;
const VK_HOME = 0x24;
const VK_LEFT = 0x25;
const VK_UP = 0x26;
const VK_RIGHT = 0x27;
const VK_DOWN = 0x28;
const VK_INSERT = 0x2d;
const VK_DELETE = 0x2e;
// 半角/全角 (IME トグル)。ホスト側 IME 方式のため、クライアントから日本語⇔英数を
// 切り替える唯一の手段としてこのキーが要る。
const VK_KANJI = 0x19;

function vkDigit(ch: string): number {
  return 0x30 + Number(ch);
}
function vkAlpha(ch: string): number {
  return ch.toUpperCase().charCodeAt(0);
}

// OEM キー (US 配列)。記号面レイヤーは作らず、これらに Shift を併用することで
// `!@#$%^&*()` `~_+{}|:"<>?` を含む US ANSI の記号を全てカバーできるため、
// 記号専用レイヤーを別途持たない設計にしている。
const VK_OEM = {
  "`": 0xc0,
  "-": 0xbd,
  "=": 0xbb,
  "[": 0xdb,
  "]": 0xdd,
  "\\": 0xdc,
  ";": 0xba,
  "'": 0xde,
  ",": 0xbc,
  ".": 0xbe,
  "/": 0xbf,
} as const;

// キーリピート: 400ms 経過後、50ms 間隔で down を再送する。物理キーボードのオートリピートは
// WM_KEYDOWN の連続 + 末尾に 1 回の WM_KEYUP という形なので、それを再現する。ホスト側は
// down を送りっぱなしにしても自動リピートしないため、クライアント側でこの間隔を刻む必要がある。
const REPEAT_DELAY_MS = 400;
const REPEAT_INTERVAL_MS = 50;

/** 通常キー 1 個ぶんの `<button>` HTML を作る。 */
function keyHtml(vk: number, label: string, ariaLabel: string, extraClass = ""): string {
  return `<button type="button" class="mkb-key ${extraClass}" data-vk="${vk}" aria-label="${ariaLabel}">${label}</button>`;
}

/** 修飾キー 1 個ぶんの `<button>` HTML を作る。 */
function modHtml(key: ModKey, label: string): string {
  return `<button type="button" class="mkb-key mkb-mod" data-mod="${key}" aria-label="${MOD_LABEL[key]}">${label}</button>`;
}

const EXT_MARKUP = `
  <div class="mkb-row">
    ${[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
      .map((n) => keyHtml(0x70 + (n - 1), `F${n}`, `F${n}`, "mkb-fn"))
      .join("")}
  </div>
  <div class="mkb-row">
    ${keyHtml(VK_OEM["`"], "`", "`")}
    ${keyHtml(VK_OEM["-"], "-", "-")}
    ${keyHtml(VK_OEM["="], "=", "=")}
    ${keyHtml(VK_OEM["["], "[", "[")}
    ${keyHtml(VK_OEM["]"], "]", "]")}
    ${keyHtml(VK_OEM["\\"], "\\", "\\")}
    ${keyHtml(VK_OEM[";"], ";", ";")}
    ${keyHtml(VK_OEM["'"], "'", "'")}
    ${keyHtml(VK_OEM["/"], "/", "/")}
  </div>
  <div class="mkb-row">
    ${keyHtml(VK_KANJI, "半/全", "半角/全角")}
    ${keyHtml(VK_INSERT, "Ins", "Insert")}
    ${keyHtml(VK_DELETE, "Del", "Delete")}
    ${keyHtml(VK_HOME, "Home", "Home")}
    ${keyHtml(VK_END, "End", "End")}
    ${keyHtml(VK_PRIOR, "PgUp", "Page Up")}
    ${keyHtml(VK_NEXT, "PgDn", "Page Down")}
  </div>
`;

const ROW1 = `
  <div class="mkb-row">
    ${"1234567890"
      .split("")
      .map((ch) => keyHtml(vkDigit(ch), ch, ch))
      .join("")}
  </div>
`;

const ROW2 = `
  <div class="mkb-row">
    ${"qwertyuiop"
      .split("")
      .map((ch) => keyHtml(vkAlpha(ch), ch, ch))
      .join("")}
  </div>
`;

const ROW3 = `
  <div class="mkb-row">
    ${"asdfghjkl"
      .split("")
      .map((ch) => keyHtml(vkAlpha(ch), ch, ch))
      .join("")}
    ${keyHtml(VK_BACK, "⌫", "Backspace", "mkb-wide")}
  </div>
`;

const ROW4 = `
  <div class="mkb-row">
    ${modHtml("shift", "⇧")}
    ${"zxcvbnm"
      .split("")
      .map((ch) => keyHtml(vkAlpha(ch), ch, ch))
      .join("")}
    ${keyHtml(VK_OEM[","], ",", ",")}
    ${keyHtml(VK_OEM["."], ".", ".")}
  </div>
`;

const ROW5 = `
  <div class="mkb-row">
    ${keyHtml(VK_ESCAPE, "Esc", "Escape")}
    ${keyHtml(VK_TAB, "Tab", "Tab")}
    ${keyHtml(VK_LEFT, "←", "左")}
    ${keyHtml(VK_UP, "↑", "上")}
    ${keyHtml(VK_DOWN, "↓", "下")}
    ${keyHtml(VK_RIGHT, "→", "右")}
    <button type="button" class="mkb-key" id="mkb-ext-toggle" aria-label="拡張キー">拡張</button>
    <button type="button" class="mkb-key" id="mkb-close" aria-label="キーボードを閉じる">⌄</button>
  </div>
`;

const ROW6 = `
  <div class="mkb-row">
    ${modHtml("ctrl", MOD_LABEL.ctrl)}
    ${modHtml("alt", MOD_LABEL.alt)}
    ${modHtml("win", MOD_LABEL.win)}
    ${keyHtml(VK_SPACE, "", "Space", "mkb-space")}
    ${keyHtml(VK_RETURN, "Enter", "Enter", "mkb-enter")}
  </div>
`;

/** モバイル用オンスクリーンキーボードをマウントする。 */
export function mountMobileKeyboard(opts: MobileKeyboardOpts): MobileKeyboardHandle {
  opts.root.innerHTML = `
    <div class="mkb-root mkb-hidden" id="mkb-root">
      <div class="mkb-ext mkb-hidden" id="mkb-ext">${EXT_MARKUP}</div>
      ${ROW1}${ROW2}${ROW3}${ROW4}${ROW5}${ROW6}
    </div>
  `;

  const rootEl = opts.root.querySelector<HTMLElement>("#mkb-root");
  const extEl = opts.root.querySelector<HTMLElement>("#mkb-ext");
  const extToggleBtn = opts.root.querySelector<HTMLButtonElement>("#mkb-ext-toggle");
  const closeBtn = opts.root.querySelector<HTMLButtonElement>("#mkb-close");

  // mousedown の既定動作 (フォーカス移動) を許すと、他のモバイル UI 同様に意図しないフォーカス
  // 変化を招くため止める。touchstart は preventDefault しない (Android で合成 click が消えて
  // ボタンが反応しなくなるため)。
  for (const btn of Array.from(opts.root.querySelectorAll<HTMLButtonElement>("button"))) {
    btn.addEventListener("mousedown", (e) => e.preventDefault());
  }

  // ================= 通常キー (リピート付き down/up) =================

  for (const btn of Array.from(opts.root.querySelectorAll<HTMLButtonElement>("[data-vk]"))) {
    const vk = Number(btn.dataset.vk);
    let repeatTimer: number | null = null;
    let repeatInterval: number | null = null;
    // pointerup と pointercancel の両方が届いても up を二重に送らないための押下フラグ
    // (ui/mobile-pad.ts の L/M/R ボタンと同じ作り)。二重に送ると consumeOnce も 2 回走る。
    let pressed = false;

    function stopRepeat(): void {
      if (repeatTimer !== null) {
        window.clearTimeout(repeatTimer);
        repeatTimer = null;
      }
      if (repeatInterval !== null) {
        window.clearInterval(repeatInterval);
        repeatInterval = null;
      }
    }

    btn.addEventListener("pointerdown", (e) => {
      // 押している指がボタンの外へ滑っても up/cancel を確実に受け取るための捕捉
      // (ui/mobile-pad.ts と同じ理由)。
      btn.setPointerCapture(e.pointerId);
      pressed = true;
      btn.classList.add("mkb-active");
      opts.sendKey(vk, true);
      repeatTimer = window.setTimeout(() => {
        repeatInterval = window.setInterval(() => opts.sendKey(vk, true), REPEAT_INTERVAL_MS);
      }, REPEAT_DELAY_MS);
    });

    function release(): void {
      if (!pressed) return;
      pressed = false;
      stopRepeat();
      btn.classList.remove("mkb-active");
      opts.sendKey(vk, false);
      opts.mods.consumeOnce();
    }

    btn.addEventListener("pointerup", release);
    btn.addEventListener("pointercancel", release);
  }

  // ================= 修飾キー (スティッキー、core/sticky-modifiers 共有ストア) =================

  const modButtons = new Map<ModKey, HTMLButtonElement>();
  for (const key of MOD_KEYS) {
    const btn = opts.root.querySelector<HTMLButtonElement>(`[data-mod="${key}"]`);
    if (!btn) continue;
    modButtons.set(key, btn);
    btn.addEventListener("pointerdown", (e) => {
      btn.setPointerCapture(e.pointerId);
      opts.mods.pressStart(key);
    });
    btn.addEventListener("pointerup", () => opts.mods.pressEnd(key));
    btn.addEventListener("pointercancel", () => opts.mods.pressCancel(key));
  }

  // 他 UI (オンスクリーンコントローラ) 由来の状態変化も含め、ボタンの見た目を常に同期する。
  opts.mods.subscribe((key, state) => {
    const btn = modButtons.get(key);
    btn?.classList.toggle("mkb-once", state === "once");
    btn?.classList.toggle("mkb-locked", state === "locked");
  });
  // マウント時点で既に他 UI 側が once/locked にしているかもしれないため、現在値を反映する。
  for (const key of MOD_KEYS) {
    const state = opts.mods.state(key);
    const btn = modButtons.get(key);
    btn?.classList.toggle("mkb-once", state === "once");
    btn?.classList.toggle("mkb-locked", state === "locked");
  }

  // ================= 拡張行トグル / 閉じる =================

  let extOpen = false;
  extToggleBtn?.addEventListener("click", () => {
    extOpen = !extOpen;
    extEl?.classList.toggle("mkb-hidden", !extOpen);
    extToggleBtn.classList.toggle("toggled", extOpen);
    updateHeightVar();
    // 拡張行の開閉でも高さが変わるので、開閉時と同じ経路で通知先へ知らせ直す
    // (知らせないとコントローラのカードが拡張行の裏に残る)。
    opts.onOpenChange?.(isOpen);
  });

  closeBtn?.addEventListener("click", () => handle.close());

  // ================= 高さ変数の同期 =================
  //
  // 下部の操作バー (ui/mobile-controls.css) がキーボードの高さぶん上へ逃げる作りにするため、
  // 開いている間だけ実測高さを CSS 変数へ反映する。拡張行のトグルや画面回転でも高さが
  // 変わるため、都度測り直す。

  function updateHeightVar(): void {
    if (!isOpen) {
      document.documentElement.style.setProperty("--mkb-height", "0px");
      return;
    }
    const h = rootEl?.offsetHeight ?? 0;
    document.documentElement.style.setProperty("--mkb-height", `${h}px`);
  }

  window.addEventListener("resize", updateHeightVar);
  window.addEventListener("orientationchange", updateHeightVar);

  let isOpen = false;

  const handle: MobileKeyboardHandle = {
    open(): void {
      isOpen = true;
      rootEl?.classList.remove("mkb-hidden");
      // 高さ変数の更新は onOpenChange より先に行う。通知先 (コントローラ) はこの変数を見て
      // カードをキーボードの上へ逃がすため、後に更新すると古い高さで計算してしまう。
      updateHeightVar();
      opts.onOpenChange?.(true);
    },
    close(): void {
      // 修飾キーの解除はしない。パッド側の修飾表示と共有している状態なので、閉じただけで
      // 解除すると「キーボードを閉じたら意図せず Ctrl が離れる」といった意図しない挙動になる。
      isOpen = false;
      rootEl?.classList.add("mkb-hidden");
      // open() と同じ理由で先に 0px へ戻す (通知先がカードを元の位置へ戻せるように)。
      updateHeightVar();
      opts.onOpenChange?.(false);
    },
    isOpen(): boolean {
      return isOpen;
    },
  };

  return handle;
}
