// Android のソフトキーボードから VK を送出するブリッジ。本プロジェクトの日本語入力は
// 「案A = ホスト側 IME」方式 (テキストを直接送る InputEvent は存在しない) なので、ここでは
// クライアント IME に変換させず、ローマ字/US配列のキーストロークとして 1 文字ずつ VK に
// 落として送る。日本語等の変換済み文字は送る手段が無いため無視する (vkFromChar 参照)。

/** Windows 仮想キーコード定数 (修飾キーバー等から使う)。 */
export const VK = {
  CONTROL: 0x11,
  MENU: 0x12,
  SHIFT: 0x10,
  LWIN: 0x5b,
  ESCAPE: 0x1b,
  TAB: 0x09,
  RETURN: 0x0d,
  BACK: 0x08,
  DELETE: 0x2e,
  LEFT: 0x25,
  UP: 0x26,
  RIGHT: 0x27,
  DOWN: 0x28,
  HOME: 0x24,
  END: 0x23,
  PRIOR: 0x21,
  NEXT: 0x22,
} as const;

export type SoftKeyboardOpts = {
  /** VK を 1 つ送る (down/up は呼び出し側が組み立てる)。 */
  sendKey: (vk: number, down: boolean) => void;
  /** 1 文字送出後、スティッキー修飾キーの解除を UI へ通知する。 */
  onConsumeModifiers?: () => void;
  /** 現在押しっぱなし扱いの修飾キー VK 群を返す (Ctrl/Alt/Shift/Win のスティッキー状態)。 */
  getStickyModifiers: () => number[];
};

/** Android ソフトキーボード用の隠し input を介して VK 送出を行う。 */
export class SoftKeyboard {
  private input: HTMLInputElement | null = null;
  // keydown フォールバックが送ろうとしている VK。同じキー入力に対して beforeinput が
  // 先に処理したら null にして二重送出を防ぐ (onBeforeInput 側で消費する)。
  private pendingKey: number | null = null;

  private readonly hBeforeInput = (e: Event) => this.onBeforeInput(e as InputEvent);
  private readonly hKeyDown = (e: KeyboardEvent) => this.onKeyDown(e);

  constructor(private readonly opts: SoftKeyboardOpts) {}

  /** 隠し input を parent に作る。 */
  mount(parent: HTMLElement): void {
    const el = document.createElement("input");
    el.type = "text";
    el.autocomplete = "off";
    el.setAttribute("autocorrect", "off");
    el.setAttribute("autocapitalize", "off");
    el.spellcheck = false;
    // 画面外 (off-screen) 配置だと一部 WebView で focus してもソフトキーボードが開かないため、
    // 画面内に 1px・opacity 0 で置いて確実に開かせる。
    el.style.position = "absolute";
    el.style.width = "1px";
    el.style.height = "1px";
    el.style.opacity = "0";
    el.style.border = "none";
    el.style.padding = "0";
    el.style.margin = "0";
    el.style.outline = "none";
    el.style.caretColor = "transparent";
    el.addEventListener("beforeinput", this.hBeforeInput);
    el.addEventListener("keydown", this.hKeyDown);
    parent.appendChild(el);
    this.input = el;
  }

  /** IME を開く (隠し input へ focus)。 */
  open(): void {
    this.input?.focus();
  }

  /** IME を閉じる (blur)。 */
  close(): void {
    this.input?.blur();
  }

  isOpen(): boolean {
    return this.input !== null && document.activeElement === this.input;
  }

  /** 単発キー送出 (修飾キーバーの Esc/Tab/矢印などから呼ぶ)。スティッキー修飾を前後に挟む。 */
  press(vk: number): void {
    this.sendKeyWithModifiers(vk, false);
  }

  destroy(): void {
    if (this.input) {
      this.input.removeEventListener("beforeinput", this.hBeforeInput);
      this.input.removeEventListener("keydown", this.hKeyDown);
      this.input.remove();
    }
    this.input = null;
    this.pendingKey = null;
  }

  private onBeforeInput(e: InputEvent): void {
    e.preventDefault();
    // 値を溜めるとカーソル位置に依存した挙動になるため、常に空へ戻す。
    if (this.input) this.input.value = "";
    switch (e.inputType) {
      case "insertText":
        this.pendingKey = null; // 対応する keydown フォールバックを打ち消す (二重送出防止)
        if (e.data) for (const ch of Array.from(e.data)) this.sendChar(ch);
        break;
      case "insertLineBreak":
      case "insertParagraph":
        this.pendingKey = null;
        this.press(VK.RETURN);
        break;
      case "deleteContentBackward":
        this.pendingKey = null;
        this.press(VK.BACK);
        break;
      case "deleteContentForward":
        this.pendingKey = null;
        this.press(VK.DELETE);
        break;
      default:
        // それ以外の inputType (履歴の undo/redo 等) は対象外。
        break;
    }
  }

  private onKeyDown(e: KeyboardEvent): void {
    // IME 変換中の中間イベント (isComposing/keyCode 229) は送らない。本プロジェクトはホスト側
    // IME 方式なので、クライアント IME の中間状態を転送するとホストで壊れたシーケンスになる
    // (core/input.ts の isImeIntermediate と同じ理由)。
    if (e.isComposing || e.keyCode === 229) return;
    const vk = vkFromSpecialKey(e.key);
    if (vk == null) return;
    e.preventDefault();
    // beforeinput は同じキー入力に対して keydown と同期的に (同じタスク内で) 発火するため、
    // マイクロタスクまで遅延させて「beforeinput が処理しなかった場合のフォールバック」として送る。
    // Enter/Backspace/Delete は通常 beforeinput 側 (insertLineBreak/deleteContentBackward/
    // deleteContentForward) で処理されるので、そちらが先に pendingKey を null にして消費する。
    this.pendingKey = vk;
    const captured = vk;
    queueMicrotask(() => {
      if (this.pendingKey === captured) {
        this.press(captured);
        this.pendingKey = null;
      }
    });
  }

  private sendChar(ch: string): void {
    const mapped = vkFromChar(ch);
    if (!mapped) return; // 日本語文字等はホスト IME 方式のため送る手段が無い
    this.sendKeyWithModifiers(mapped.vk, mapped.shift);
  }

  /** 1 キー送出: スティッキー修飾 down → 対象 down/up → スティッキー修飾を逆順に up。 */
  private sendKeyWithModifiers(vk: number, needShift: boolean): void {
    const sticky = this.opts.getStickyModifiers();
    const hasStickyShift = sticky.includes(VK.SHIFT);
    const tempShift = needShift && !hasStickyShift;

    for (const m of sticky) this.opts.sendKey(m, true);
    if (tempShift) this.opts.sendKey(VK.SHIFT, true);
    this.opts.sendKey(vk, true);
    this.opts.sendKey(vk, false);
    if (tempShift) this.opts.sendKey(VK.SHIFT, false);
    for (let i = sticky.length - 1; i >= 0; i--) this.opts.sendKey(sticky[i], false);

    this.opts.onConsumeModifiers?.();
  }
}

/** Enter/Backspace/矢印など、beforeinput が発火しないことがあるキーの VK マッピング。 */
function vkFromSpecialKey(key: string): number | null {
  switch (key) {
    case "Escape":
      return VK.ESCAPE;
    case "Tab":
      return VK.TAB;
    case "Enter":
      return VK.RETURN;
    case "Backspace":
      return VK.BACK;
    case "Delete":
      return VK.DELETE;
    case "ArrowLeft":
      return VK.LEFT;
    case "ArrowUp":
      return VK.UP;
    case "ArrowRight":
      return VK.RIGHT;
    case "ArrowDown":
      return VK.DOWN;
    case "Home":
      return VK.HOME;
    case "End":
      return VK.END;
    case "PageUp":
      return VK.PRIOR;
    case "PageDown":
      return VK.NEXT;
    default:
      return null;
  }
}

const SHIFT_DIGITS: Record<string, string> = {
  "!": "1",
  "@": "2",
  "#": "3",
  $: "4",
  "%": "5",
  "^": "6",
  "&": "7",
  "*": "8",
  "(": "9",
  ")": "0",
};

const SYMBOL_MAP: Record<string, { vk: number; shift: boolean }> = {
  "`": { vk: 0xc0, shift: false },
  "~": { vk: 0xc0, shift: true },
  "-": { vk: 0xbd, shift: false },
  _: { vk: 0xbd, shift: true },
  "=": { vk: 0xbb, shift: false },
  "+": { vk: 0xbb, shift: true },
  "[": { vk: 0xdb, shift: false },
  "{": { vk: 0xdb, shift: true },
  "]": { vk: 0xdd, shift: false },
  "}": { vk: 0xdd, shift: true },
  "\\": { vk: 0xdc, shift: false },
  "|": { vk: 0xdc, shift: true },
  ";": { vk: 0xba, shift: false },
  ":": { vk: 0xba, shift: true },
  "'": { vk: 0xde, shift: false },
  '"': { vk: 0xde, shift: true },
  ",": { vk: 0xbc, shift: false },
  "<": { vk: 0xbc, shift: true },
  ".": { vk: 0xbe, shift: false },
  ">": { vk: 0xbe, shift: true },
  "/": { vk: 0xbf, shift: false },
  "?": { vk: 0xbf, shift: true },
};

/**
 * 1 文字を US 配列前提の VK + shift 要否に変換する。日本語文字などマッピング不能な文字は
 * null を返す (ホスト側 IME 方式のため変換済み文字を直接注入する手段が無く、ユーザーは
 * ローマ字を打ってホストの IME に変換させる前提)。
 */
function vkFromChar(ch: string): { vk: number; shift: boolean } | null {
  if (/^[a-z]$/.test(ch)) return { vk: ch.toUpperCase().charCodeAt(0), shift: false };
  if (/^[A-Z]$/.test(ch)) return { vk: ch.charCodeAt(0), shift: true };
  if (/^[0-9]$/.test(ch)) return { vk: 0x30 + Number(ch), shift: false };
  if (ch === " ") return { vk: 0x20, shift: false };
  const shiftDigit = SHIFT_DIGITS[ch];
  if (shiftDigit !== undefined) return { vk: 0x30 + Number(shiftDigit), shift: true };
  const symbol = SYMBOL_MAP[ch];
  if (symbol) return symbol;
  return null;
}
