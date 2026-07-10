// Wake on LAN パネル。launcher の「Wake on LAN」タブに埋め込まれる自己完結コンポーネント。
// raspi 中継 (relay_host:relay_port) 経由でマジックパケットを送り、任意で紐付けた登録ホストの
// 起動を到達性プローブで待って自動接続する (起こして接続)。
//
// ホスト由来/ユーザー入力の文字列 (label / mac / relay_host 等) は innerHTML に載せず
// textContent / DOM API で描画する (XSS 注意、CLAUDE.md 規約)。

import {
  deleteWolTarget,
  listWolTargets,
  loadRecentHosts,
  savedPasswordHosts,
  probeHosts,
  saveWolTarget,
  wakeHost,
  type HostEntry,
  type WolTarget,
} from "../core/ipc";
import { logger } from "../core/log";
import { showToast } from "./toast";
import { applyAriaLabels, trapFocus } from "../core/a11y";
import { icon } from "./icons";
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
const POLL_MAX_MS = 90_000;

/** MAC を ":" 区切り大文字へ正規化する。形式不正なら null。 */
function normalizeMac(raw: string): string | null {
  const s = raw.trim();
  if (!MAC_RE.test(s)) return null;
  return s.replace(/-/g, ":").toUpperCase();
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
    <div id="wol-dialog-backdrop"></div>
  `;

  const listEl = root.querySelector<HTMLElement>("#wol-list") as HTMLElement;
  const backdrop = root.querySelector<HTMLElement>("#wol-dialog-backdrop") as HTMLElement;
  applyAriaLabels(root);

  const activePolls = new Map<string, PollState>();

  // --- ダイアログ共通 (追加/編集/削除確認/パスワード入力で使い回す) ---
  let releaseTrap: (() => void) | null = null;
  function openBackdrop(): void {
    backdrop.classList.add("visible");
    applyAriaLabels(backdrop);
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

  function showProgress(card: HTMLElement, text: string): void {
    const actions = card.querySelector<HTMLElement>(".wol-actions");
    const progress = card.querySelector<HTMLElement>(".wol-progress");
    const label = card.querySelector<HTMLElement>(".wol-progress-text");
    if (actions) actions.hidden = true;
    if (progress) progress.hidden = false;
    if (label) label.textContent = text;
  }
  function hideProgress(card: HTMLElement): void {
    const actions = card.querySelector<HTMLElement>(".wol-actions");
    const progress = card.querySelector<HTMLElement>(".wol-progress");
    if (actions) actions.hidden = false;
    if (progress) progress.hidden = true;
  }
  function stopPoll(id: string): void {
    const p = activePolls.get(id);
    if (!p) return;
    window.clearInterval(p.intervalId);
    activePolls.delete(id);
  }

  /** 起こして接続の到達性ポーリングを開始する (3 秒間隔・最大 90 秒)。 */
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
          <button type="submit" class="btn-primary" id="wp-ok">起こす</button>
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
            okBtn.textContent = "起こす";
          }
        });
    });
  }

  /** 「起こす」ボタン。パスワード未保存エラーならダイアログへフォールバックする。 */
  async function handleWake(t: WolTarget): Promise<void> {
    try {
      await wakeHost(t.relay_host, t.relay_port, null, false, t.mac);
      showToast("マジックパケットを送信しました", "success");
    } catch (err) {
      const msg = String(err);
      if (looksLikePasswordError(msg)) {
        openWakePasswordDialog(t);
      } else {
        showToast(`起こす操作に失敗しました: ${msg}`, "error");
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
        showToast(`起こす操作に失敗しました: ${msg}`, "error");
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
          <label>表示名 (任意)<input id="w-label" type="text" placeholder="home-pc" /></label>
          <label>登録済みホストから選ぶ
            <select id="w-host" class="select-input"><option value="">(紐付けなし・手動)</option></select>
          </label>
          <label>MAC アドレス<input id="w-mac" type="text" placeholder="AA:BB:CC:DD:EE:FF" required /></label>
          <label>中継先 (raspi) host<input id="w-relay-host" type="text" placeholder="100.x.x.x" required /></label>
          <label>中継先 port<input id="w-relay-port" type="text" inputmode="numeric" /></label>
          <div class="dlg-sub">中継先のパスワード (初回のみ・以降は空欄で保存済みを使用)</div>
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
              showToast(`起こす操作に失敗しました: ${String(err)}`, "error");
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
    listEl.replaceChildren();
    if (targets.length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty-hint";
      empty.append("保存済みの WoL 先はありません。", document.createElement("br"), "右上の ＋ から追加してください。");
      listEl.appendChild(empty);
      return;
    }
    for (const t of targets) {
      const card = document.createElement("div");
      card.className = "wol-card";
      card.dataset.id = t.id;

      const avatar = document.createElement("div");
      avatar.className = "wol-avatar";
      avatar.innerHTML = icon("bolt");

      const meta = document.createElement("div");
      meta.className = "meta";
      const labelEl = document.createElement("div");
      labelEl.className = "label";
      labelEl.textContent = t.label || t.mac;
      const macEl = document.createElement("div");
      macEl.className = "mac";
      macEl.textContent = t.mac;
      const relayEl = document.createElement("div");
      relayEl.className = "relay";
      relayEl.textContent = `中継: ${t.relay_host}:${t.relay_port}`;
      meta.append(labelEl, macEl, relayEl);
      if (t.host) {
        const linkEl = document.createElement("div");
        linkEl.className = "link";
        linkEl.textContent = `紐付け: ${t.host}:${t.port ?? DEFAULT_HOST_PORT}`;
        meta.appendChild(linkEl);
      }

      const progress = document.createElement("div");
      progress.className = "wol-progress";
      progress.hidden = true;
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
      progress.append(progressText, cancelBtn);

      const actions = document.createElement("div");
      actions.className = "wol-actions";
      const wakeBtn = document.createElement("button");
      wakeBtn.type = "button";
      wakeBtn.className = "btn-secondary";
      wakeBtn.textContent = "起こす";
      wakeBtn.addEventListener("click", () => void handleWake(t));
      actions.appendChild(wakeBtn);
      if (t.host) {
        const wakeConnectBtn = document.createElement("button");
        wakeConnectBtn.type = "button";
        wakeConnectBtn.className = "btn-secondary accent";
        wakeConnectBtn.textContent = "起こして接続";
        wakeConnectBtn.addEventListener("click", () => void handleWakeAndConnect(t, card));
        actions.appendChild(wakeConnectBtn);
      }
      const editBtn = document.createElement("button");
      editBtn.className = "card-btn";
      editBtn.title = "編集";
      editBtn.innerHTML = icon("edit");
      editBtn.addEventListener("click", () => openTargetDialog(t));
      const delBtn = document.createElement("button");
      delBtn.className = "card-btn danger";
      delBtn.title = "削除";
      delBtn.innerHTML = icon("delete");
      delBtn.addEventListener("click", () => {
        openConfirmDialog(`「${t.label || t.mac}」を一覧から削除しますか?`, async () => {
          stopPoll(t.id);
          await deleteWolTarget(t.id);
          await refresh();
        });
      });
      actions.append(editBtn, delBtn);

      card.append(avatar, meta, progress, actions);
      listEl.appendChild(card);

      // ポーリング継続中に再描画された行 (別カードの追加/削除操作等) は進捗表示を復元する。
      const poll = activePolls.get(t.id);
      if (poll) showProgress(card, `起動待ち… ${Math.round(poll.elapsedMs / 1000)}s`);
    }
    applyAriaLabels(listEl);
  }

  root.querySelector("#wol-add")?.addEventListener("click", () => openTargetDialog(null));

  async function refresh(): Promise<void> {
    try {
      renderTargets(await listWolTargets());
    } catch (e) {
      logger.warn("load wol targets failed", e);
    }
  }

  void refresh();

  return { refresh };
}
