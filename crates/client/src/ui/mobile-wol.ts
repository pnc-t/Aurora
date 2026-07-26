// モバイル (Android) 専用の Wake on LAN パネル。ロジックはデスクトップ版 `wol-panel.ts` の移植だが、
// 見た目とダイアログ操作は `mobile-launcher.ts` の Hosts カード (タップ=主操作 / 長押し=削除確認)
// に合わせて別実装にしている。
//
// デスクトップ版と分離した理由 (=単なる差分ではなく意図的な縮小):
// - `savedPasswordHosts()` (中継先パスワードが DPAPI に保存済みか) は Rust 側実装が DPAPI
//   (Windows 専用) のため、Android では常に失敗し空集合を返す。デスクトップ版はこれを使って
//   「起こして接続」前のガード・鍵バッジ・⋯メニューの「パスワードを忘れる」を実装しているが、
//   Android でそのまま使うと判定が常に false になり、ガードがある「起こして接続」が永久に
//   実行できなくなる。そのためこのファイルは savedPasswordHosts を一切呼ばず、鍵バッジも
//   ⋯メニューも持たない (削除はカード長押しへ移した)。
// - 並べ替え (grip ドラッグ) も持たない。モバイル版 Hosts タブが並べ替え非対応であるのに揃えた。
// - 中継先パスワード入力ダイアログの「保存する」チェックも同じ理由で外し、save=false 固定で送る。
//
// ホスト由来/ユーザー入力の文字列 (label / mac / relay_host / host) は innerHTML に載せず
// textContent / DOM API で描画する (XSS 注意、CLAUDE.md 規約)。固定の SVG アイコン (icon() の
// 戻り値) のみ innerHTML で可。

import "./mobile-wol.css";

import {
  deleteWolTarget,
  listWolTargets,
  loadRecentHosts,
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
import { avatarColor, hasImageIcon } from "./avatar";
import { enhanceSelects } from "./select-menu";

export type MobileWolPanelHandle = {
  refresh: () => Promise<void>;
};

const DEFAULT_RELAY_PORT = 7100;
const DEFAULT_HOST_PORT = 7000;
const MAC_RE = /^[0-9A-Fa-f]{2}([:-][0-9A-Fa-f]{2}){5}$/;
const POLL_STEP_MS = 3000;
// コールドブート→ログイン→ホスト常駐起動→Tailscale 復帰まで数分かかることがあるため長めに取る
// (デスクトップ版 wol-panel.ts と同じ値。実機で 90 秒では起動が間に合わなかった実績に合わせる)。
const POLL_MAX_MS = 240_000;
const LONG_PRESS_MS = 600;

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

export function mountMobileWolPanel(
  root: HTMLElement,
  opts: { onReachable: (entry: HostEntry) => void },
): MobileWolPanelHandle {
  root.innerHTML = `
    <div class="page-head">
      <h2>Wake on LAN</h2>
      <div class="actions">
        <button class="icon-btn" id="mwol-add" title="WoL 先を追加">${icon("add")}</button>
      </div>
    </div>
    <div class="mwol-list" id="mwol-list"></div>
    <div id="mwol-dialog-backdrop"></div>
  `;

  const listEl = root.querySelector<HTMLElement>("#mwol-list") as HTMLElement;
  const backdrop = root.querySelector<HTMLElement>("#mwol-dialog-backdrop") as HTMLElement;
  applyAriaLabels(root);

  const activePolls = new Map<string, PollState>();
  // 送信中 (ポーリング開始前) の id 集合。カードタップの二重起動防止に使う。
  const sending = new Set<string>();
  function isBusy(id: string): boolean {
    return sending.has(id) || activePolls.has(id);
  }

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

  /** カードの 2 行目を進捗表示 (スピナー + テキスト + キャンセル) に差し替える。
   *  cancel=false でキャンセルボタンを隠す (単発の「起こす」フィードバック用)。
   *  done=true で完了色 (チェック) にする。 */
  function showProgress(
    card: HTMLElement,
    text: string,
    opts: { cancel?: boolean; done?: boolean } = {},
  ): void {
    const { cancel = true, done = false } = opts;
    const sub = card.querySelector<HTMLElement>(".mwol-sub");
    const progress = card.querySelector<HTMLElement>(".mwol-progress");
    const label = card.querySelector<HTMLElement>(".mwol-progress-text");
    const cancelBtn = card.querySelector<HTMLElement>(".mwol-progress-cancel");
    const spinner = card.querySelector<HTMLElement>(".mwol-spinner");
    if (sub) sub.hidden = true;
    if (progress) progress.hidden = false;
    if (cancelBtn) cancelBtn.hidden = !cancel;
    if (spinner) spinner.hidden = done; // 完了表示 (✓) 時は回さない
    if (label) {
      label.textContent = text;
      label.classList.toggle("done", done);
    }
  }
  function hideProgress(card: HTMLElement): void {
    const sub = card.querySelector<HTMLElement>(".mwol-sub");
    const progress = card.querySelector<HTMLElement>(".mwol-progress");
    const label = card.querySelector<HTMLElement>(".mwol-progress-text");
    if (sub) sub.hidden = false;
    if (progress) progress.hidden = true;
    if (label) label.classList.remove("done");
  }
  /** 「起こす」成功時の一時フィードバック。数秒後、ポーリング中でなければ元へ戻す。 */
  function flashWakeSent(id: string): void {
    const card = findCard(id);
    if (card) showProgress(card, "✓ 起動信号を送信しました", { cancel: false, done: true });
    window.setTimeout(() => {
      if (activePolls.has(id)) return; // 起こして接続の進捗を消さない
      sending.delete(id);
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

  /** 起こして接続の到達性ポーリングを開始する (3 秒間隔・最大 4 分)。
   *  到達したら opts.onReachable を呼ぶだけ (接続処理自体は呼び出し側の責務)。 */
  function beginPoll(t: WolTarget, host: string, port: number, card: HTMLElement): void {
    stopPoll(t.id);
    sending.delete(t.id);
    const state: PollState = { intervalId: 0, elapsedMs: 0 };
    showProgress(card, "起動待ち 0s");
    state.intervalId = window.setInterval(() => {
      void (async () => {
        state.elapsedMs += POLL_STEP_MS;
        const curCard = findCard(t.id);
        if (!curCard) {
          // 一覧が再描画されて行を見失った (追加/編集/削除操作)。追跡不能なので打ち切る。
          stopPoll(t.id);
          return;
        }
        showProgress(curCard, `起動待ち ${Math.round(state.elapsedMs / 1000)}s`);
        try {
          const results = await probeHosts([{ host, port }]);
          const online = results.find((r) => r.host === host)?.online ?? false;
          if (online) {
            stopPoll(t.id);
            hideProgress(curCard);
            opts.onReachable({ label: t.label, host, port });
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

  /** パスワード未保存エラー時に出す入力ダイアログ。Android では DPAPI 保存が機能しないため
   *  「保存する」チェックは持たず、save=false 固定で送る。成功したら afterSuccess を呼ぶ。 */
  function openWakePasswordDialog(t: WolTarget, afterSuccess?: () => void): void {
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>中継先のパスワード</h3>
        <div class="s-desc" style="font-family:monospace"></div>
        <label>password
          <span class="pass-wrap">
            <input id="mwp-pass" type="password" required />
            <button type="button" class="pass-eye" id="mwp-eye" title="パスワードを表示">${icon("eye")}</button>
          </span>
        </label>
        <div class="dlg-status" id="mwp-status"></div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="mwp-cancel">cancel</button>
          <button type="submit" class="btn-primary" id="mwp-ok">起動</button>
        </div>
      </form>
    `;
    // relay_host はユーザー入力由来なので textContent で埋める。
    const desc = backdrop.querySelector<HTMLElement>(".s-desc");
    if (desc) desc.textContent = `${t.relay_host}:${t.relay_port}`;
    openBackdrop();
    const passInput = backdrop.querySelector<HTMLInputElement>("#mwp-pass");
    const okBtn = backdrop.querySelector<HTMLButtonElement>("#mwp-ok");
    const statusEl = backdrop.querySelector<HTMLElement>("#mwp-status");
    const eyeBtn = backdrop.querySelector<HTMLButtonElement>("#mwp-eye");
    eyeBtn?.addEventListener("click", () => {
      if (!passInput) return;
      const show = passInput.type === "password";
      passInput.type = show ? "text" : "password";
      eyeBtn.innerHTML = icon(show ? "eye-off" : "eye");
      passInput.focus();
    });
    passInput?.focus();
    backdrop.querySelector("#mwp-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (ev) => {
      ev.preventDefault();
      const password = passInput?.value ?? "";
      if (!password) return;
      if (okBtn) {
        okBtn.disabled = true;
        okBtn.textContent = "送信中…";
      }
      void wakeHost(t.relay_host, t.relay_port, password, false, t.mac)
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

  /** 「起こす」単発。送信中/送信済みをカード上に明示する。
   *  パスワード未保存エラーならダイアログへフォールバックする。 */
  async function handleWake(t: WolTarget, card: HTMLElement): Promise<void> {
    if (isBusy(t.id)) return;
    sending.add(t.id);
    showProgress(card, "起動信号を送信中…", { cancel: false });
    try {
      await wakeHost(t.relay_host, t.relay_port, null, false, t.mac);
      showToast("マジックパケットを送信しました", "success");
      flashWakeSent(t.id);
    } catch (err) {
      const msg = String(err);
      sending.delete(t.id);
      hideProgress(card);
      if (looksLikePasswordError(msg)) {
        // パスワードダイアログは modal backdrop (z-index 60) で画面全体を覆うため、開いている間は
        // カードをタップできない。sending へ登録すると、キャンセル (backdrop クリック/Escape/
        // cancel ボタン) 時に削除する経路が無く isBusy が真のまま固着してしまうため登録しない。
        openWakePasswordDialog(t, () => flashWakeSent(t.id));
      } else {
        showToast(`起動操作に失敗しました: ${msg}`, "error");
      }
    }
  }

  /** 「起こして接続」。起こしてから到達性を待ち、届いたら opts.onReachable を呼ぶ
   *  (呼び出し側が接続ダイアログを開く。このパネル自身は接続処理をしない)。
   *  デスクトップ版と違い savedPasswordHosts() による事前ガードは持たない
   *  (Android では常に空集合を返し、ガードがあると永久に実行できなくなるため)。 */
  async function handleWakeAndConnect(t: WolTarget, card: HTMLElement): Promise<void> {
    if (!t.host) return;
    if (isBusy(t.id)) return;
    const host = t.host;
    const port = t.port ?? DEFAULT_HOST_PORT;
    sending.add(t.id);
    showProgress(card, "起動信号を送信中…", { cancel: false });
    try {
      await wakeHost(t.relay_host, t.relay_port, null, false, t.mac);
      showToast("マジックパケットを送信しました。起動を待っています…", "info");
      beginPoll(t, host, port, card);
    } catch (err) {
      const msg = String(err);
      sending.delete(t.id);
      hideProgress(card);
      if (looksLikePasswordError(msg)) {
        // handleWake と同じ理由 (modal backdrop でカードを操作できないため sending への登録は不要。
        // 登録するとキャンセル経路が無く isBusy が真のまま固着する)。
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
          <button type="button" class="btn-ghost" id="mwc-cancel">cancel</button>
          <button type="button" class="btn-danger" id="mwc-ok">削除</button>
        </div>
      </div>
    `;
    const desc = backdrop.querySelector<HTMLElement>(".s-desc");
    if (desc) desc.textContent = message;
    openBackdrop();
    backdrop.querySelector("#mwc-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector("#mwc-ok")?.addEventListener("click", () => {
      void onConfirm().finally(closeDialog);
    });
  }

  /** 追加/編集ダイアログ。target=null で新規。
   *  デスクトップ版と違い、中継先パスワードの「保存済み/未保存」ヒントは出さない
   *  (savedPasswordHosts を呼ばないため判定材料が無い)。画像アイコン選択もモバイル版では
   *  持たない (デスクトップ側で設定済みの icon フィールドは編集時も保持する)。 */
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
          <label>表示名 (任意)<input id="mw-label" type="text" placeholder="home-pc" /></label>
          <label>登録済みホストから選ぶ
            <select id="mw-host" class="select-input"><option value="">(紐付けなし・手動)</option></select>
          </label>
          <label>MAC アドレス<input id="mw-mac" type="text" placeholder="AA:BB:CC:DD:EE:FF" required /></label>
          <div class="dlg-row">
            <label>中継先 (raspi) host<input id="mw-relay-host" type="text" placeholder="100.x.x.x" required /></label>
            <label class="port">port<input id="mw-relay-port" type="text" inputmode="numeric" /></label>
          </div>
          <div class="dlg-sub">中継先のパスワード (入力すると保存後すぐ起動信号を送ります・空欄可)</div>
          <label>password
            <span class="pass-wrap">
              <input id="mw-pass" type="password" />
              <button type="button" class="pass-eye" id="mw-eye" title="パスワードを表示">${icon("eye")}</button>
            </span>
          </label>
          <div class="dlg-status" id="mw-status"></div>
          <div class="dlg-actions">
            <button type="button" class="btn-ghost" id="mw-cancel">cancel</button>
            <button type="submit" class="btn-primary">${isAdd ? "add" : "save"}</button>
          </div>
        </form>
      `;
      openBackdrop();

      const labelInput = backdrop.querySelector<HTMLInputElement>("#mw-label");
      const hostSelect = backdrop.querySelector<HTMLSelectElement>("#mw-host");
      const macInput = backdrop.querySelector<HTMLInputElement>("#mw-mac");
      const relayHostInput = backdrop.querySelector<HTMLInputElement>("#mw-relay-host");
      const relayPortInput = backdrop.querySelector<HTMLInputElement>("#mw-relay-port");
      const passInput = backdrop.querySelector<HTMLInputElement>("#mw-pass");
      const eyeBtn = backdrop.querySelector<HTMLButtonElement>("#mw-eye");
      const statusEl = backdrop.querySelector<HTMLElement>("#mw-status");

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

      backdrop.querySelector<HTMLInputElement>("#mw-mac")?.focus();
      backdrop.querySelector("#mw-cancel")?.addEventListener("click", closeDialog);
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
          icon: target?.icon ?? null, // 画像アイコン選択 UI は持たないので既存値をそのまま保持する
        };
        void (async () => {
          await saveWolTarget(saved);
          const password = passInput?.value ?? "";
          if (password) {
            // Android は DPAPI 保存が機能しないため「保存する」チェックは持たず save=false 固定
            // (保存できないのにできるかのような UI を出さない)。
            try {
              await wakeHost(relayHost, relayPort, password, false, macNorm);
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
    listEl.replaceChildren();
    if (targets.length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty-hint";
      empty.append("保存済みの WoL 先はありません。", document.createElement("br"), "右上の ＋ から追加してください。");
      listEl.appendChild(empty);
      return;
    }
    targets.forEach((t) => {
      const card = document.createElement("div");
      card.className = "mwol-card";
      card.dataset.id = t.id;
      card.tabIndex = 0;
      card.setAttribute("role", "button");

      const name = t.label || t.mac;
      // 画像アイコンがあれば img、無ければ頭文字 + 自動生成色 (Hosts/デスクトップ WoL と共通)。
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

      // 1 行目: ラベル + 編集ボタン。
      const row1 = document.createElement("div");
      row1.className = "row1";
      const labelEl = document.createElement("span");
      labelEl.className = "label";
      labelEl.textContent = name;
      const editBtn = document.createElement("button");
      editBtn.type = "button";
      editBtn.className = "card-btn mwol-edit";
      editBtn.title = "編集";
      editBtn.innerHTML = icon("edit");
      // カードのタップ (主アクション) へ伝播させない。
      editBtn.addEventListener("pointerdown", (e) => e.stopPropagation());
      editBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        openTargetDialog(t);
      });
      row1.append(labelEl, editBtn);

      // 2 行目: 紐付けホストがあれば接続先、無ければ MAC (紐付けありの MAC は編集ダイアログで確認)。
      const sub = document.createElement("div");
      sub.className = "mwol-sub";
      sub.textContent = t.host ? `→ ${t.host}:${t.port ?? DEFAULT_HOST_PORT}` : t.mac;

      // 進捗表示 (起動待ち/送信中)。通常時は hidden、showProgress/hideProgress で切り替える。
      const progress = document.createElement("div");
      progress.className = "mwol-progress";
      progress.hidden = true;
      const spinner = document.createElement("span");
      spinner.className = "mwol-spinner";
      const progressText = document.createElement("span");
      progressText.className = "mwol-progress-text";
      const cancelBtn = document.createElement("button");
      cancelBtn.type = "button";
      cancelBtn.className = "card-btn mwol-progress-cancel";
      cancelBtn.title = "キャンセル";
      cancelBtn.innerHTML = icon("close");
      cancelBtn.addEventListener("pointerdown", (e) => e.stopPropagation());
      cancelBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        stopPoll(t.id);
        sending.delete(t.id);
        hideProgress(card);
      });
      progress.append(spinner, progressText, cancelBtn);

      meta.append(row1, sub, progress);
      card.append(avatar, meta);
      listEl.appendChild(card);

      // ポーリング継続中に再描画された行 (別カードの追加/削除操作等) は進捗表示を復元する。
      const poll = activePolls.get(t.id);
      if (poll) showProgress(card, `起動待ち ${Math.round(poll.elapsedMs / 1000)}s`);

      // タップ = 主アクション (紐付けあり: 起こして接続 / 紐付けなし: 起こす)、
      // 長押し (600ms) = 削除確認 (mobile-launcher.ts の Hosts カードと同じジェスチャ)。
      let pressTimer: number | null = null;
      let longPressFired = false;
      const clearPress = (): void => {
        if (pressTimer !== null) {
          window.clearTimeout(pressTimer);
          pressTimer = null;
        }
      };
      card.addEventListener("pointerdown", () => {
        longPressFired = false;
        clearPress();
        pressTimer = window.setTimeout(() => {
          pressTimer = null;
          longPressFired = true;
          openConfirmDialog(`「${name}」を一覧から削除しますか?`, async () => {
            stopPoll(t.id);
            sending.delete(t.id);
            await deleteWolTarget(t.id);
            await refresh();
          });
        }, LONG_PRESS_MS);
      });
      card.addEventListener("pointerup", clearPress);
      card.addEventListener("pointerleave", clearPress);
      card.addEventListener("pointercancel", clearPress);
      card.addEventListener("click", () => {
        if (longPressFired) return;
        if (isBusy(t.id)) return; // 進捗表示中はタップを無視する
        if (t.host) void handleWakeAndConnect(t, card);
        else void handleWake(t, card);
      });
      card.addEventListener("keydown", (e) => {
        if (e.target !== card) return;
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          card.click();
        }
      });
    });
    applyAriaLabels(listEl);
  }

  root.querySelector("#mwol-add")?.addEventListener("click", () => openTargetDialog(null));

  async function refresh(): Promise<void> {
    try {
      const targets = await listWolTargets();
      renderTargets(targets);
    } catch (e) {
      logger.warn("load wol targets failed", e);
    }
  }

  void refresh();

  return { refresh };
}
