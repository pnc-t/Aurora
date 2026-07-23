// Wake on LAN パネル。launcher の「Wake on LAN」タブに埋め込まれる自己完結コンポーネント。
// raspi 中継 (relay_host:relay_port) 経由でマジックパケットを送り、任意で紐付けた登録ホストの
// 起動を到達性プローブで待って自動接続する (起こして接続)。
//
// ホスト由来/ユーザー入力の文字列 (label / mac / relay_host 等) は innerHTML に載せず
// textContent / DOM API で描画する (XSS 注意、CLAUDE.md 規約)。

import {
  deleteHostPassword,
  deleteWolTarget,
  listWolTargets,
  loadRecentHosts,
  savedPasswordHosts,
  probeHosts,
  saveWolTarget,
  setWolTargets,
  wakeHost,
  type HostEntry,
  type WolTarget,
} from "../core/ipc";
import { logger } from "../core/log";
import { showToast } from "./toast";
import { applyAriaLabels, trapFocus } from "../core/a11y";
import { icon } from "./icons";
import { avatarColor, hasImageIcon, imageToIconDataUrl } from "./avatar";
import { enhanceSelects } from "./select-menu";
import "./wol-panel.css";

/** 接続実行コールバック (launcher の onConnect とシグネチャを揃える)。 */
type ConnectCallback = (
  entry: HostEntry,
  password: string | null,
  save: boolean,
  newWindow: boolean,
) => Promise<void>;

export type WolPanelHandle = {
  refresh: () => Promise<void>;
};

const DEFAULT_RELAY_PORT = 7100;
const DEFAULT_HOST_PORT = 7000;
const MAC_RE = /^[0-9A-Fa-f]{2}([:-][0-9A-Fa-f]{2}){5}$/;
const POLL_STEP_MS = 3000;
// コールドブート→ログイン→ホスト常駐起動→Tailscale 復帰まで数分かかることがあるため長めに取る
// (90 秒では起動が間に合わず接続できないことが実機で判明)。
const POLL_MAX_MS = 240_000;

/** MAC を ":" 区切り大文字へ正規化する。形式不正なら null。 */
function normalizeMac(raw: string): string | null {
  const s = raw.trim();
  if (!MAC_RE.test(s)) return null;
  return s.replace(/-/g, ":").toUpperCase();
}

/** 中継先パスワードの secret キー (Rust 側 wol_secret_key + key() の正規化と一致させる)。 */
function relaySecretKey(relayHost: string, relayPort: number): string {
  return `wol:${relayHost}:${relayPort}`.trim().toLowerCase();
}

/** Rust から返るエラーメッセージが「中継先パスワード未保存」を示すかのヒューリスティック判定。
 *  IPC 契約上エラーはただの String なので、キーワード一致で判定する。 */
function looksLikePasswordError(message: string): boolean {
  const m = message.toLowerCase();
  return m.includes("password") || message.includes("パスワード");
}

type PollState = { intervalId: number; elapsedMs: number };

export function mountWolPanel(root: HTMLElement, opts: { onConnect: ConnectCallback }): WolPanelHandle {
  root.innerHTML = `
    <div class="page-head">
      <h2>Wake on LAN</h2>
      <div class="actions">
        <button class="icon-btn" id="wol-add" title="WoL 先を追加">${icon("add")}</button>
      </div>
    </div>
    <div class="wol-list" id="wol-list"></div>
    <div id="wol-card-menu" class="card-menu"></div>
    <div id="wol-dialog-backdrop"></div>
  `;

  const listEl = root.querySelector<HTMLElement>("#wol-list") as HTMLElement;
  const backdrop = root.querySelector<HTMLElement>("#wol-dialog-backdrop") as HTMLElement;
  const cardMenu = root.querySelector<HTMLElement>("#wol-card-menu") as HTMLElement;
  applyAriaLabels(root);

  const activePolls = new Map<string, PollState>();
  // 現在表示中の並び (ドラッグ並べ替えの基準)。renderTargets で更新する。
  let currentTargets: WolTarget[] = [];
  // 中継先パスワードが保存済みの secret キー (relaySecretKey 形式)。refresh で更新する。
  let savedKeys = new Set<string>();

  // --- ダイアログ共通 (追加/編集/削除確認/パスワード入力で使い回す) ---
  let releaseTrap: (() => void) | null = null;
  function openBackdrop(): void {
    backdrop.classList.add("visible");
    applyAriaLabels(backdrop);
    // 呼び出し側が option 追加 / select.value 代入をする前に差し替える (開くたびに再構築される)。
    enhanceSelects(backdrop);
    releaseTrap?.();
    releaseTrap = trapFocus(backdrop);
  }
  function closeDialog(): void {
    releaseTrap?.();
    releaseTrap = null;
    backdrop.classList.remove("visible");
    backdrop.innerHTML = "";
  }
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeDialog();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && backdrop.classList.contains("visible")) closeDialog();
  });

  function findCard(id: string): HTMLElement | undefined {
    return Array.from(listEl.children).find((el) => (el as HTMLElement).dataset.id === id) as
      | HTMLElement
      | undefined;
  }

  /** カードのアクション行を進捗表示に差し替える。
   *  cancel=false でキャンセルボタンを隠す (単発の「起こす」フィードバック用)。
   *  done=true で完了色 (チェック) にする。 */
  function showProgress(
    card: HTMLElement,
    text: string,
    opts: { cancel?: boolean; done?: boolean } = {},
  ): void {
    const { cancel = true, done = false } = opts;
    const actions = card.querySelector<HTMLElement>(".wol-actions");
    const progress = card.querySelector<HTMLElement>(".wol-progress");
    const label = card.querySelector<HTMLElement>(".wol-progress-text");
    const cancelBtn = card.querySelector<HTMLElement>(".wol-progress-cancel");
    const spinner = card.querySelector<HTMLElement>(".wol-spinner");
    if (actions) actions.hidden = true;
    if (progress) progress.hidden = false;
    if (cancelBtn) cancelBtn.hidden = !cancel;
    if (spinner) spinner.hidden = done; // 完了表示 (✓) 時は回さない
    if (label) {
      label.textContent = text;
      label.classList.toggle("done", done);
    }
  }
  function hideProgress(card: HTMLElement): void {
    const actions = card.querySelector<HTMLElement>(".wol-actions");
    const progress = card.querySelector<HTMLElement>(".wol-progress");
    const label = card.querySelector<HTMLElement>(".wol-progress-text");
    if (actions) actions.hidden = false;
    if (progress) progress.hidden = true;
    if (label) label.classList.remove("done");
  }
  /** 「起こす」成功時の一時フィードバック。数秒後、ポーリング中でなければ元へ戻す。 */
  function flashWakeSent(id: string): void {
    const card = findCard(id);
    if (card) showProgress(card, "✓ 起動信号を送信しました", { cancel: false, done: true });
    window.setTimeout(() => {
      if (activePolls.has(id)) return; // 起こして接続の進捗を消さない
      const cur = findCard(id);
      if (cur) hideProgress(cur);
    }, 4000);
  }
  function stopPoll(id: string): void {
    const p = activePolls.get(id);
    if (!p) return;
    window.clearInterval(p.intervalId);
    activePolls.delete(id);
  }

  /** 起こして接続の到達性ポーリングを開始する (3 秒間隔・最大 4 分)。 */
  function beginPoll(t: WolTarget, host: string, port: number, card: HTMLElement): void {
    stopPoll(t.id);
    const state: PollState = { intervalId: 0, elapsedMs: 0 };
    showProgress(card, "起動待ち… 0s");
    state.intervalId = window.setInterval(() => {
      void (async () => {
        state.elapsedMs += POLL_STEP_MS;
        const curCard = findCard(t.id);
        if (!curCard) {
          // 一覧が再描画されて行を見失った (追加/編集/削除操作)。追跡不能なので打ち切る。
          stopPoll(t.id);
          return;
        }
        showProgress(curCard, `起動待ち… ${Math.round(state.elapsedMs / 1000)}s`);
        try {
          const results = await probeHosts([{ host, port }]);
          const online = results.find((r) => r.host === host)?.online ?? false;
          if (online) {
            stopPoll(t.id);
            hideProgress(curCard);
            try {
              await opts.onConnect({ label: t.label, host, port }, null, false, false);
            } catch (e) {
              logger.warn("wake-and-connect: connect failed", e);
              showToast("接続に失敗しました", "error");
            }
            return;
          }
        } catch (e) {
          logger.warn("wake-and-connect: probe failed", e);
        }
        if (state.elapsedMs >= POLL_MAX_MS) {
          stopPoll(t.id);
          hideProgress(curCard);
          showToast("起動を確認できませんでした", "error");
        }
      })();
    }, POLL_STEP_MS);
    activePolls.set(t.id, state);
  }

  /** パスワード未保存エラー時に出す入力ダイアログ。成功したら afterSuccess を呼ぶ。 */
  function openWakePasswordDialog(t: WolTarget, afterSuccess?: () => void): void {
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>中継先のパスワード</h3>
        <div class="s-desc" style="font-family:monospace"></div>
        <label>password
          <span class="pass-wrap">
            <input id="wp-pass" type="password" required />
            <button type="button" class="pass-eye" id="wp-eye" title="パスワードを表示">${icon("eye")}</button>
          </span>
        </label>
        <label class="dlg-check"><input id="wp-save" type="checkbox" checked /> このパスワードを保存する</label>
        <div class="dlg-status" id="wp-status"></div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="wp-cancel">cancel</button>
          <button type="submit" class="btn-primary" id="wp-ok">起動</button>
        </div>
      </form>
    `;
    // relay_host はユーザー入力由来なので textContent で埋める。
    const desc = backdrop.querySelector<HTMLElement>(".s-desc");
    if (desc) desc.textContent = `${t.relay_host}:${t.relay_port}`;
    openBackdrop();
    const passInput = backdrop.querySelector<HTMLInputElement>("#wp-pass");
    const okBtn = backdrop.querySelector<HTMLButtonElement>("#wp-ok");
    const statusEl = backdrop.querySelector<HTMLElement>("#wp-status");
    const eyeBtn = backdrop.querySelector<HTMLButtonElement>("#wp-eye");
    eyeBtn?.addEventListener("click", () => {
      if (!passInput) return;
      const show = passInput.type === "password";
      passInput.type = show ? "text" : "password";
      eyeBtn.innerHTML = icon(show ? "eye-off" : "eye");
      passInput.focus();
    });
    passInput?.focus();
    backdrop.querySelector("#wp-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (ev) => {
      ev.preventDefault();
      const password = passInput?.value ?? "";
      if (!password) return;
      const save = backdrop.querySelector<HTMLInputElement>("#wp-save")?.checked ?? true;
      if (okBtn) {
        okBtn.disabled = true;
        okBtn.textContent = "送信中…";
      }
      void wakeHost(t.relay_host, t.relay_port, password, save, t.mac)
        .then(() => {
          closeDialog();
          showToast("マジックパケットを送信しました", "success");
          afterSuccess?.();
        })
        .catch((err) => {
          if (statusEl) {
            statusEl.textContent = `error: ${String(err)}`;
            statusEl.classList.add("error");
          }
          if (okBtn) {
            okBtn.disabled = false;
            okBtn.textContent = "起動";
          }
        });
    });
  }

  /** 「起こす」ボタン。送信中/送信済みをカード上に明示する。
   *  パスワード未保存エラーならダイアログへフォールバックする。 */
  async function handleWake(t: WolTarget, card: HTMLElement): Promise<void> {
    if (activePolls.has(t.id)) return; // 起こして接続の進捗表示中は触らない
    showProgress(card, "起動信号を送信中…", { cancel: false });
    try {
      await wakeHost(t.relay_host, t.relay_port, null, false, t.mac);
      showToast("マジックパケットを送信しました", "success");
      flashWakeSent(t.id);
    } catch (err) {
      const msg = String(err);
      hideProgress(card);
      if (looksLikePasswordError(msg)) {
        openWakePasswordDialog(t, () => flashWakeSent(t.id));
      } else {
        showToast(`起動操作に失敗しました: ${msg}`, "error");
      }
    }
  }

  /** 「起こして接続」ボタン。起こしてから到達性を待ち、届いたら通常接続する。 */
  async function handleWakeAndConnect(t: WolTarget, card: HTMLElement): Promise<void> {
    if (!t.host) return;
    const host = t.host;
    const port = t.port ?? DEFAULT_HOST_PORT;
    let saved = new Set<string>();
    try {
      saved = new Set((await savedPasswordHosts()).map((h) => h.trim().toLowerCase()));
    } catch {
      saved = new Set();
    }
    if (!saved.has(host.trim().toLowerCase())) {
      showToast("先にこのホストへ一度手動接続し、パスワードを保存してください", "error");
      return;
    }
    try {
      await wakeHost(t.relay_host, t.relay_port, null, false, t.mac);
      showToast("マジックパケットを送信しました。起動を待っています…", "info");
      beginPoll(t, host, port, card);
    } catch (err) {
      const msg = String(err);
      if (looksLikePasswordError(msg)) {
        openWakePasswordDialog(t, () => beginPoll(t, host, port, card));
      } else {
        showToast(`起動操作に失敗しました: ${msg}`, "error");
      }
    }
  }

  /** 削除確認ダイアログ。 */
  function openConfirmDialog(message: string, onConfirm: () => Promise<void>): void {
    backdrop.innerHTML = `
      <div class="dialog">
        <h3>削除の確認</h3>
        <div class="s-desc"></div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="wc-cancel">cancel</button>
          <button type="button" class="btn-danger" id="wc-ok">削除</button>
        </div>
      </div>
    `;
    const desc = backdrop.querySelector<HTMLElement>(".s-desc");
    if (desc) desc.textContent = message;
    openBackdrop();
    backdrop.querySelector("#wc-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector("#wc-ok")?.addEventListener("click", () => {
      void onConfirm().finally(closeDialog);
    });
  }

  // --- カードの ⋯ メニュー (削除) ---
  // 開いているメニューのアンカー (⋯ ボタン)。同じボタンの再クリックで閉じるトグル判定に使う。
  let cardMenuAnchor: HTMLElement | null = null;
  function closeCardMenu(): void {
    cardMenu.classList.remove("visible");
    cardMenu.innerHTML = "";
    cardMenuAnchor = null;
  }
  // メニュー表示中は ↑↓ で項目間フォーカスを移動する (Hosts と同じ挙動)。
  cardMenu.addEventListener("keydown", (e) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    const items = Array.from(cardMenu.querySelectorAll<HTMLButtonElement>(".menu-item"));
    if (items.length === 0) return;
    e.preventDefault();
    const idx = items.indexOf(document.activeElement as HTMLButtonElement);
    const next = e.key === "ArrowDown" ? (idx + 1) % items.length : (idx - 1 + items.length) % items.length;
    items[next]?.focus();
  });
  /** ⋯ ボタンの下にメニューを開く (Hosts と同じ挙動)。現状は削除のみ。同じ anchor での
   *  再呼び出しはトグルで閉じる。 */
  function openCardMenu(anchor: HTMLElement, t: WolTarget): void {
    if (cardMenuAnchor === anchor) {
      closeCardMenu();
      return;
    }
    const saved = savedKeys.has(relaySecretKey(t.relay_host, t.relay_port));
    const forgetItem = saved
      ? `<button class="menu-item" data-m="forget">${icon("key")} パスワードを忘れる</button>`
      : "";
    cardMenu.innerHTML = `${forgetItem}<button class="menu-item danger" data-m="del">${icon("delete")} 削除</button>`;
    cardMenu.classList.add("visible");
    cardMenuAnchor = anchor;
    // 表示してから幅を測り、ボタン右端に揃えて配置する。
    const r = anchor.getBoundingClientRect();
    cardMenu.style.top = `${r.bottom + 4}px`;
    cardMenu.style.left = `${Math.max(8, r.right - cardMenu.offsetWidth)}px`;
    cardMenu.querySelector('[data-m="forget"]')?.addEventListener("click", () => {
      closeCardMenu();
      void deleteHostPassword(relaySecretKey(t.relay_host, t.relay_port))
        .then(() => refresh())
        .catch((e) => logger.warn("forget wol relay password failed", e));
    });
    cardMenu.querySelector('[data-m="del"]')?.addEventListener("click", () => {
      closeCardMenu();
      openConfirmDialog(`「${t.label || t.mac}」を一覧から削除しますか?`, async () => {
        stopPoll(t.id);
        await deleteWolTarget(t.id);
        await refresh();
      });
    });
    applyAriaLabels(cardMenu);
    cardMenu.querySelector<HTMLButtonElement>(".menu-item")?.focus();
  }
  // メニュー外クリック / スクロール / Esc で閉じる。
  document.addEventListener("click", () => closeCardMenu());
  listEl.addEventListener("scroll", () => closeCardMenu());
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeCardMenu();
  });

  // --- 並べ替え (grip を掴んでドラッグ、Hosts と同じ pointer 実装) ---
  /** from の要素を to の位置へ移動し、保存して再描画する。 */
  function reorder(from: number, to: number): void {
    if (from === to || from < 0 || to < 0) return;
    const arr = [...currentTargets];
    const [moved] = arr.splice(from, 1);
    arr.splice(to, 0, moved);
    currentTargets = arr;
    renderTargets(arr);
    void setWolTargets(arr).catch((e) => logger.warn("reorder wol targets failed", e));
  }
  /** 掴んだカードの分身がカーソルに追従し、元カードは空きスロットとして間を移動する。 */
  function startDrag(from: number, downEvent: PointerEvent, card: HTMLElement): void {
    const rect = card.getBoundingClientRect();
    const offsetY = downEvent.clientY - rect.top;
    let targetPos = from;

    const clone = card.cloneNode(true) as HTMLElement;
    clone.classList.add("drag-clone");
    clone.style.width = `${rect.width}px`;
    clone.style.left = `${rect.left}px`;
    clone.style.top = `${rect.top}px`;
    root.appendChild(clone);
    card.classList.add("drag-placeholder");

    const onMove = (ev: PointerEvent): void => {
      const listRect = listEl.getBoundingClientRect();
      const desiredTop = ev.clientY - offsetY;
      const maxTop = Math.max(listRect.top, listRect.bottom - rect.height);
      const top = Math.min(Math.max(desiredTop, listRect.top), maxTop);
      clone.style.top = `${top}px`;
      const others = Array.from(listEl.querySelectorAll<HTMLElement>(".wol-card")).filter(
        (c) => c !== card,
      );
      let pos = others.length;
      for (let i = 0; i < others.length; i++) {
        const r = others[i].getBoundingClientRect();
        if (ev.clientY < r.top + r.height / 2) {
          pos = i;
          break;
        }
      }
      targetPos = pos;
      if (pos >= others.length) listEl.appendChild(card);
      else listEl.insertBefore(card, others[pos]);
    };
    const onUp = (): void => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      clone.remove();
      card.classList.remove("drag-placeholder");
      reorder(from, targetPos);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  /** 追加/編集ダイアログ。target=null で新規。 */
  function openTargetDialog(target: WolTarget | null): void {
    const isAdd = target === null;
    void (async () => {
      let hosts: HostEntry[] = [];
      try {
        hosts = await loadRecentHosts();
      } catch {
        /* 補助情報。失敗しても手動 MAC 入力は使える */
      }
      const linkedNow = target?.host ? hosts.find((h) => h.host === target.host) : undefined;

      backdrop.innerHTML = `
        <form class="dialog" autocomplete="off">
          <h3>${isAdd ? "WoL 先を追加" : "WoL 先を編集"}</h3>
          <div class="icon-picker">
            <div class="icon-preview" id="w-icon-preview"></div>
            <div class="icon-picker-actions">
              <button type="button" class="btn-secondary" id="w-icon-pick">画像を選択</button>
              <button type="button" class="btn-ghost" id="w-icon-clear">解除</button>
            </div>
            <input type="file" id="w-icon-file" accept="image/*" hidden />
          </div>
          <label>表示名 (任意)<input id="w-label" type="text" placeholder="home-pc" /></label>
          <label>登録済みホストから選ぶ
            <select id="w-host" class="select-input"><option value="">(紐付けなし・手動)</option></select>
          </label>
          <label>MAC アドレス<input id="w-mac" type="text" placeholder="AA:BB:CC:DD:EE:FF" required /></label>
          <div class="dlg-row">
            <label>中継先 (raspi) host<input id="w-relay-host" type="text" placeholder="100.x.x.x" required /></label>
            <label class="port">port<input id="w-relay-port" type="text" inputmode="numeric" /></label>
          </div>
          <div class="dlg-sub" id="w-pass-sub">中継先のパスワード (初回のみ・以降は空欄で保存済みを使用)</div>
          <label>password
            <span class="pass-wrap">
              <input id="w-pass" type="password" />
              <button type="button" class="pass-eye" id="w-eye" title="パスワードを表示">${icon("eye")}</button>
            </span>
          </label>
          <label class="dlg-check"><input id="w-save" type="checkbox" checked /> このパスワードを保存する</label>
          <div class="dlg-status" id="w-status"></div>
          <div class="dlg-actions">
            <button type="button" class="btn-ghost" id="w-cancel">cancel</button>
            <button type="submit" class="btn-primary">${isAdd ? "add" : "save"}</button>
          </div>
        </form>
      `;
      openBackdrop();

      const labelInput = backdrop.querySelector<HTMLInputElement>("#w-label");
      const hostSelect = backdrop.querySelector<HTMLSelectElement>("#w-host");
      const macInput = backdrop.querySelector<HTMLInputElement>("#w-mac");
      const relayHostInput = backdrop.querySelector<HTMLInputElement>("#w-relay-host");
      const relayPortInput = backdrop.querySelector<HTMLInputElement>("#w-relay-port");
      const passInput = backdrop.querySelector<HTMLInputElement>("#w-pass");
      const eyeBtn = backdrop.querySelector<HTMLButtonElement>("#w-eye");
      const statusEl = backdrop.querySelector<HTMLElement>("#w-status");
      const passSub = backdrop.querySelector<HTMLElement>("#w-pass-sub");

      // relay host/port の入力値に応じて、保存済みかどうかの文言を出し分ける。
      function updatePassHint(): void {
        if (!passSub) return;
        const key = relaySecretKey(relayHostInput?.value.trim() ?? "", Number(relayPortInput?.value) || DEFAULT_RELAY_PORT);
        passSub.textContent = savedKeys.has(key)
          ? "中継先のパスワード (保存済み。変更する場合のみ入力)"
          : "中継先のパスワード (未保存。起動時に入力を求めます)";
      }
      relayHostInput?.addEventListener("input", updatePassHint);
      relayPortInput?.addEventListener("input", updatePassHint);

      // ホスト由来文字列 (label/host) は option の value/textContent に DOM API で入れる。
      for (const h of hosts) {
        const optEl = document.createElement("option");
        optEl.value = h.host;
        optEl.textContent = `${h.label || h.host} (${h.host}:${h.port})`;
        hostSelect?.appendChild(optEl);
      }
      if (labelInput) labelInput.value = target?.label ?? "";
      if (hostSelect) hostSelect.value = target?.host ?? "";
      if (macInput) macInput.value = target?.mac ?? "";
      if (relayHostInput) relayHostInput.value = target?.relay_host ?? "";
      if (relayPortInput) relayPortInput.value = String(target?.relay_port ?? DEFAULT_RELAY_PORT);
      if (linkedNow?.mac && macInput && !macInput.value) macInput.value = linkedNow.mac;
      updatePassHint(); // relay host/port の初期値セット後に呼ぶ (先だと "" 扱いで未保存誤判定になる)

      hostSelect?.addEventListener("change", () => {
        const linked = hosts.find((h) => h.host === hostSelect.value);
        if (!linked) return;
        if (labelInput && !labelInput.value.trim()) labelInput.value = linked.label || linked.host;
        if (macInput && !macInput.value.trim() && linked.mac) macInput.value = linked.mac;
      });

      eyeBtn?.addEventListener("click", () => {
        if (!passInput) return;
        const show = passInput.type === "password";
        passInput.type = show ? "text" : "password";
        eyeBtn.innerHTML = icon(show ? "eye-off" : "eye");
        passInput.focus();
      });

      // --- アイコン (画像) 選択 ---
      // 画像未設定なら label/mac 由来の頭文字アバターをプレビューに出し、選択中は画像を出す。
      let currentIcon = target?.icon ?? "";
      const preview = backdrop.querySelector<HTMLElement>("#w-icon-preview");
      const fileInput = backdrop.querySelector<HTMLInputElement>("#w-icon-file");
      const clearBtn = backdrop.querySelector<HTMLButtonElement>("#w-icon-clear");
      function renderIconPreview(): void {
        if (!preview) return;
        if (hasImageIcon(currentIcon)) {
          preview.style.background = "";
          const img = document.createElement("img");
          img.src = currentIcon; // 自前生成の data URL
          img.alt = "";
          img.draggable = false;
          preview.replaceChildren(img);
        } else {
          const nm = labelInput?.value.trim() || macInput?.value.trim() || "?";
          preview.style.background = avatarColor(nm);
          preview.textContent = (nm[0] ?? "?").toUpperCase();
        }
        if (clearBtn) clearBtn.style.display = currentIcon ? "" : "none";
      }
      renderIconPreview();
      labelInput?.addEventListener("input", () => {
        if (!hasImageIcon(currentIcon)) renderIconPreview();
      });
      macInput?.addEventListener("input", () => {
        if (!hasImageIcon(currentIcon)) renderIconPreview();
      });
      backdrop.querySelector("#w-icon-pick")?.addEventListener("click", () => fileInput?.click());
      fileInput?.addEventListener("change", () => {
        const file = fileInput.files?.[0];
        fileInput.value = ""; // 同じファイルを選び直しても change が出るようリセット
        if (!file) return;
        void imageToIconDataUrl(file)
          .then((url) => {
            currentIcon = url;
            renderIconPreview();
          })
          .catch((err) => logger.warn("icon load failed", err));
      });
      clearBtn?.addEventListener("click", () => {
        currentIcon = "";
        renderIconPreview();
      });

      backdrop.querySelector<HTMLInputElement>("#w-mac")?.focus();
      backdrop.querySelector("#w-cancel")?.addEventListener("click", closeDialog);
      backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (ev) => {
        ev.preventDefault();
        const macNorm = normalizeMac(macInput?.value ?? "");
        if (!macNorm) {
          if (statusEl) {
            statusEl.textContent = "MAC アドレスの形式が正しくありません (例: AA:BB:CC:DD:EE:FF)";
            statusEl.classList.add("error");
          }
          return;
        }
        const relayHost = relayHostInput?.value.trim() ?? "";
        if (!relayHost) return;
        const relayPort = Number(relayPortInput?.value) || DEFAULT_RELAY_PORT;
        const label = labelInput?.value.trim() ?? "";
        const selectedHost = hostSelect?.value ?? "";
        const linked = selectedHost ? hosts.find((h) => h.host === selectedHost) : undefined;
        const saved: WolTarget = {
          id: target?.id ?? crypto.randomUUID(),
          label: label || linked?.label || macNorm,
          mac: macNorm,
          relay_host: relayHost,
          relay_port: relayPort,
          host: linked ? linked.host : null,
          port: linked ? linked.port : null,
          icon: currentIcon || null,
        };
        void (async () => {
          await saveWolTarget(saved);
          const password = passInput?.value ?? "";
          if (password) {
            const save = backdrop.querySelector<HTMLInputElement>("#w-save")?.checked ?? true;
            try {
              await wakeHost(relayHost, relayPort, password, save, macNorm);
              showToast("マジックパケットを送信しました", "success");
            } catch (err) {
              showToast(`起動操作に失敗しました: ${String(err)}`, "error");
            }
          }
          closeDialog();
          await refresh();
        })().catch((e) => {
          logger.warn("save wol target failed", e);
          if (statusEl) {
            statusEl.textContent = `error: ${String(e)}`;
            statusEl.classList.add("error");
          }
        });
      });
    })();
  }

  function renderTargets(targets: WolTarget[]): void {
    currentTargets = targets;
    listEl.replaceChildren();
    if (targets.length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty-hint";
      empty.append("保存済みの WoL 先はありません。", document.createElement("br"), "右上の ＋ から追加してください。");
      listEl.appendChild(empty);
      return;
    }
    targets.forEach((t, idx) => {
      const card = document.createElement("div");
      card.className = "wol-card";
      card.dataset.id = t.id;

      // 並べ替えグリップ (Hosts と同じ)。
      const grip = document.createElement("span");
      grip.className = "grip";
      grip.title = "ドラッグで並べ替え";
      grip.innerHTML = icon("grip");
      grip.addEventListener("pointerdown", (e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        closeCardMenu();
        startDrag(idx, e, card);
      });

      const name = t.label || t.mac;
      // 画像アイコンがあれば img、無ければ頭文字 + 自動生成色 (Hosts と共通)。
      const avatar = document.createElement("div");
      if (hasImageIcon(t.icon)) {
        avatar.className = "avatar avatar-img";
        const img = document.createElement("img");
        img.src = t.icon; // 自前生成の data URL
        img.alt = "";
        img.draggable = false;
        avatar.appendChild(img);
      } else {
        avatar.className = "avatar";
        avatar.style.background = avatarColor(name);
        avatar.textContent = (name[0] ?? "?").toUpperCase();
      }

      const meta = document.createElement("div");
      meta.className = "meta";
      const labelEl = document.createElement("div");
      labelEl.className = "label";
      labelEl.textContent = name;
      const labelRow = document.createElement("div");
      labelRow.className = "label-row";
      labelRow.appendChild(labelEl);
      if (savedKeys.has(relaySecretKey(t.relay_host, t.relay_port))) {
        const badge = document.createElement("span");
        badge.className = "saved-badge";
        badge.title = "中継先パスワード保存済み";
        badge.innerHTML = icon("key"); // 固定 SVG のみなので innerHTML で可
        labelRow.appendChild(badge);
      }
      const macEl = document.createElement("div");
      macEl.className = "mac";
      macEl.textContent = t.mac;
      meta.append(labelRow, macEl);
      // 紐付けホストがあれば接続先を控えめに出す (中継先はカードには出さず編集ダイアログで確認)。
      if (t.host) {
        const linkEl = document.createElement("div");
        linkEl.className = "link";
        linkEl.textContent = `→ ${t.host}:${t.port ?? DEFAULT_HOST_PORT}`;
        meta.appendChild(linkEl);
      }

      const progress = document.createElement("div");
      progress.className = "wol-progress";
      progress.hidden = true;
      // 進行中はスピナーを添える。テキストだけだと押した実感が薄く、送信中か
      // 何も起きていないのか区別しづらかった。
      const spinner = document.createElement("span");
      spinner.className = "wol-spinner";
      const progressText = document.createElement("span");
      progressText.className = "wol-progress-text";
      const cancelBtn = document.createElement("button");
      cancelBtn.type = "button";
      cancelBtn.className = "btn-ghost wol-progress-cancel";
      cancelBtn.textContent = "キャンセル";
      cancelBtn.addEventListener("click", () => {
        stopPoll(t.id);
        hideProgress(card);
      });
      progress.append(spinner, progressText, cancelBtn);

      const actions = document.createElement("div");
      actions.className = "wol-actions";
      // Hosts のカードと並びを揃える: 左から 編集 / ⋯ メニュー / 主アクション を右端に 1 つ置く
      // (Hosts は 編集 / ⋯ / chevron=接続)。カードごとに末尾アクションを 1 個に固定することで、
      // 接続状態や紐付けの有無に関わらず 編集/⋯ の位置が全カード・両タブで揃う。
      const editBtn = document.createElement("button");
      editBtn.type = "button";
      editBtn.className = "card-btn";
      editBtn.title = "編集";
      editBtn.innerHTML = icon("edit");
      editBtn.addEventListener("click", () => openTargetDialog(t));
      const menuBtn = document.createElement("button");
      menuBtn.type = "button";
      menuBtn.className = "card-btn";
      menuBtn.title = "メニュー";
      menuBtn.innerHTML = icon("more");
      menuBtn.addEventListener("click", (e) => {
        e.stopPropagation(); // document の閉じるハンドラより先に開かないようにする
        openCardMenu(menuBtn, t);
      });
      // 末尾アクション枠。起動 (稲妻) は常に表示、紐付けありは 起動して接続 (稲妻+chevron) も表示。
      // Hosts と同じ固定幅 (--card-tail-w) を右寄せで共有し、編集/⋯ の位置を揃える。
      const tail = document.createElement("div");
      tail.className = "card-tail";
      const wakeBtn = document.createElement("button");
      wakeBtn.type = "button";
      wakeBtn.className = "card-btn wake";
      wakeBtn.title = "起動";
      wakeBtn.innerHTML = icon("bolt");
      wakeBtn.addEventListener("click", () => void handleWake(t, card));
      tail.appendChild(wakeBtn);
      if (t.host) {
        const wakeConnectBtn = document.createElement("button");
        wakeConnectBtn.type = "button";
        wakeConnectBtn.className = "card-btn wake accent";
        wakeConnectBtn.title = "起動して接続";
        wakeConnectBtn.innerHTML = icon("bolt") + icon("chevron");
        wakeConnectBtn.addEventListener("click", () => void handleWakeAndConnect(t, card));
        tail.appendChild(wakeConnectBtn);
      }
      actions.append(editBtn, menuBtn, tail);

      card.append(grip, avatar, meta, progress, actions);
      listEl.appendChild(card);

      // ポーリング継続中に再描画された行 (別カードの追加/削除操作等) は進捗表示を復元する。
      const poll = activePolls.get(t.id);
      if (poll) showProgress(card, `起動待ち… ${Math.round(poll.elapsedMs / 1000)}s`);
    });
    applyAriaLabels(listEl);
  }

  root.querySelector("#wol-add")?.addEventListener("click", () => openTargetDialog(null));

  async function refresh(): Promise<void> {
    try {
      const targets = await listWolTargets();
      try {
        savedKeys = new Set((await savedPasswordHosts()).map((h) => h.trim().toLowerCase()));
      } catch (e) {
        savedKeys = new Set();
        logger.warn("load saved password hosts failed", e);
      }
      renderTargets(targets);
    } catch (e) {
      logger.warn("load wol targets failed", e);
    }
  }

  void refresh();

  return { refresh };
}
