// モバイル (Android) 専用の Wake on LAN パネル。ロジックはデスクトップ版 `wol-panel.ts` の移植だが、
// 見た目とダイアログ操作は `mobile-launcher.ts` の Hosts カード (タップ=主操作 / ⋮ メニュー=編集・削除)
// に合わせて別実装にしている。
//
// デスクトップ版と分離した理由 (=単なる差分ではなく意図的な縮小):
// - `savedPasswordHosts()` (中継先パスワードが DPAPI に保存済みか) は Rust 側実装が DPAPI
//   (Windows 専用) のため、Android では常に失敗し空集合を返す。デスクトップ版はこれを使って
//   「起こして接続」前のガード・鍵バッジ・⋯メニューの「パスワードを忘れる」を実装しているが、
//   Android でそのまま使うと判定が常に false になり、ガードがある「起こして接続」が永久に
//   実行できなくなる。そのためこのファイルは savedPasswordHosts を一切呼ばず、鍵バッジも
//   「パスワードを忘れる」も持たない。
// - 並べ替え (grip ドラッグ) も持たない。モバイル版 Hosts タブが並べ替え非対応であるのに揃えた。
// - 中継先パスワード入力ダイアログの「保存する」チェックも同じ理由で外し、save=false 固定で送る。
//
// カード操作は Hosts タブと統一し ⋮ ボタン → アクションシート (編集/削除) に集約している。
// 不可視のジェスチャ (長押し) に破壊的操作 (削除) を割り当てない方針のため、長押し削除は廃止した。
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
import { avatarColor, hasImageIcon, imageToIconDataUrl } from "./avatar";
import { enhanceSelects } from "./select-menu";
import { openActionSheet } from "./mobile-sheet";
import { attachPullToRefresh } from "./pull-to-refresh";

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
        <!-- .ml-icon-btn-labeled は mobile-launcher.css 定義 (.ml-app 配下スコープ)。
             このパネルは .ml-app の子孫にマウントされるため流用できる。 -->
        <button class="icon-btn ml-icon-btn-labeled" id="mwol-add" title="WoL 先を追加">${icon("add")}<span>追加</span></button>
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
    const moreBtn = card.querySelector<HTMLElement>(".mwol-more");
    if (sub) sub.hidden = true;
    if (progress) progress.hidden = false;
    if (cancelBtn) cancelBtn.hidden = !cancel;
    // ⋮ と × は tail の同じスロットを共有する (44px タップ領域の重なりを防ぐため排他表示)。
    // cancel=false (単発の「起こす」フィードバック) では × を出さないので ⋮ は隠さない。
    if (moreBtn) moreBtn.hidden = cancel;
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
    const moreBtn = card.querySelector<HTMLElement>(".mwol-more");
    const cancelBtn = card.querySelector<HTMLElement>(".mwol-progress-cancel");
    if (sub) sub.hidden = false;
    if (progress) progress.hidden = true;
    if (label) label.classList.remove("done");
    // ⋮ / × は tail 直下で progress とは別要素になったため、両方を明示的に戻す。
    if (moreBtn) moreBtn.hidden = false;
    if (cancelBtn) cancelBtn.hidden = true;
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
          <button type="button" class="btn-ghost" id="mwp-cancel">キャンセル</button>
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

  /** 汎用の確認ダイアログ (削除確認/起動確認で使い回す)。
   *  title/confirmLabel は固定文言に埋め込まず引数化し、danger で削除相当の危険色を切り替える。 */
  function openConfirmDialog(
    dialogOpts: { title: string; message: string; confirmLabel: string; danger?: boolean },
    onConfirm: () => Promise<void>,
  ): void {
    const { title, message, confirmLabel, danger = false } = dialogOpts;
    backdrop.innerHTML = `
      <div class="dialog">
        <h3></h3>
        <div class="s-desc"></div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="mwc-cancel">キャンセル</button>
          <button type="button" class="${danger ? "btn-danger" : "btn-primary"}" id="mwc-ok"></button>
        </div>
      </div>
    `;
    // title/message/confirmLabel は呼び出し元が name (ホスト由来ラベル) を埋め込むため textContent で入れる。
    const titleEl = backdrop.querySelector<HTMLElement>("h3");
    if (titleEl) titleEl.textContent = title;
    const desc = backdrop.querySelector<HTMLElement>(".s-desc");
    if (desc) desc.textContent = message;
    const okBtn = backdrop.querySelector<HTMLElement>("#mwc-ok");
    if (okBtn) okBtn.textContent = confirmLabel;
    openBackdrop();
    backdrop.querySelector("#mwc-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector("#mwc-ok")?.addEventListener("click", () => {
      void onConfirm().finally(closeDialog);
    });
  }

  /** 追加/編集ダイアログ。target=null で新規。
   *  デスクトップ版と違い、中継先パスワードの「保存済み/未保存」ヒントは出さない
   *  (savedPasswordHosts を呼ばないため判定材料が無い)。画像アイコン選択は Hosts の追加ダイアログ
   *  (`mobile-launcher.ts` の openAddHostDialog) と同じ体裁で持たせている。 */
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
            <div class="icon-preview" id="mw-icon-preview"></div>
            <div class="icon-picker-actions">
              <button type="button" class="btn-secondary" id="mw-icon-pick">画像を選択</button>
              <button type="button" class="btn-ghost" id="mw-icon-clear">解除</button>
            </div>
            <input type="file" id="mw-icon-file" accept="image/*" hidden />
          </div>
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
            <button type="button" class="btn-ghost" id="mw-cancel">キャンセル</button>
            <button type="submit" class="btn-primary">${isAdd ? "追加" : "保存"}</button>
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

      // 画像アイコン選択 (mobile-launcher.ts の openAddHostDialog と同じ体裁)。
      // 頭文字フォールバックはカード描画と同じ規則 (表示名 → MAC の順)。フィールドの初期値を
      // 埋めた後に呼ぶ (編集時に頭文字フォールバックが正しい表示名/MAC を参照できるようにする)。
      let currentIcon = target?.icon ?? "";
      const iconPreview = backdrop.querySelector<HTMLElement>("#mw-icon-preview");
      const iconFileInput = backdrop.querySelector<HTMLInputElement>("#mw-icon-file");
      const iconClearBtn = backdrop.querySelector<HTMLButtonElement>("#mw-icon-clear");
      function renderIconPreview(): void {
        if (!iconPreview) return;
        if (hasImageIcon(currentIcon)) {
          iconPreview.style.background = "";
          const img = document.createElement("img");
          img.src = currentIcon; // 自前生成の data URL
          img.alt = "";
          img.draggable = false;
          iconPreview.replaceChildren(img);
        } else {
          const nm = labelInput?.value.trim() || macInput?.value.trim() || "?";
          iconPreview.style.background = avatarColor(nm);
          iconPreview.textContent = (nm[0] ?? "?").toUpperCase();
        }
        if (iconClearBtn) iconClearBtn.style.display = currentIcon ? "" : "none";
      }
      renderIconPreview();
      labelInput?.addEventListener("input", () => {
        if (!hasImageIcon(currentIcon)) renderIconPreview();
      });
      macInput?.addEventListener("input", () => {
        if (!hasImageIcon(currentIcon)) renderIconPreview();
      });
      backdrop.querySelector("#mw-icon-pick")?.addEventListener("click", () => iconFileInput?.click());
      iconFileInput?.addEventListener("change", () => {
        const file = iconFileInput.files?.[0];
        iconFileInput.value = "";
        if (!file) return;
        void imageToIconDataUrl(file)
          .then((url) => {
            currentIcon = url;
            renderIconPreview();
          })
          .catch((err) => logger.warn("icon load failed", err));
      });
      iconClearBtn?.addEventListener("click", () => {
        currentIcon = "";
        renderIconPreview();
      });

      hostSelect?.addEventListener("change", () => {
        const linked = hosts.find((h) => h.host === hostSelect.value);
        if (!linked) return;
        if (labelInput && !labelInput.value.trim()) labelInput.value = linked.label || linked.host;
        if (macInput && !macInput.value.trim() && linked.mac) macInput.value = linked.mac;
        if (!hasImageIcon(currentIcon)) renderIconPreview();
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
          // 画像を選んでいればその data URL、「解除」を押していれば空文字、
          // 何も操作していなければ既存値 (target?.icon) をそのまま保持する。
          icon: currentIcon,
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

      // 1 行目: ラベルのみ。⋮ メニューは tail (カード右端) へ移した (× キャンセルボタンと
      // 同じスロットを共有させ、44px タップ領域の重なりを構造的に無くすため)。
      const row1 = document.createElement("div");
      row1.className = "row1";
      const labelEl = document.createElement("span");
      labelEl.className = "label";
      labelEl.textContent = name;
      const moreBtn = document.createElement("button");
      moreBtn.type = "button";
      moreBtn.className = "card-btn mwol-more";
      moreBtn.title = "メニュー";
      moreBtn.innerHTML = icon("more");
      // カードのタップ (主アクション) へ伝播させない。
      moreBtn.addEventListener("pointerdown", (e) => e.stopPropagation());
      moreBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        openActionSheet([
          {
            label: "編集",
            icon: "edit",
            onSelect: () => openTargetDialog(t),
          },
          {
            label: "削除",
            icon: "delete",
            danger: true,
            onSelect: () => {
              openConfirmDialog(
                {
                  title: "削除の確認",
                  message: `「${name}」を一覧から削除しますか?`,
                  confirmLabel: "削除",
                  danger: true,
                },
                async () => {
                  stopPoll(t.id);
                  sending.delete(t.id);
                  await deleteWolTarget(t.id);
                  await refresh();
                },
              );
            },
          },
        ]);
      });
      row1.append(labelEl);

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
      // tail へ移す前は .mwol-progress の子だったため progress ごと隠れていたが、tail 直下では
      // 自前で初期状態を持つ必要がある (待機中のカードに × が出っぱなしになる)。
      cancelBtn.hidden = true;
      cancelBtn.title = "キャンセル";
      cancelBtn.innerHTML = icon("close");
      cancelBtn.addEventListener("pointerdown", (e) => e.stopPropagation());
      cancelBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        stopPoll(t.id);
        sending.delete(t.id);
        hideProgress(card);
      });
      progress.append(spinner, progressText);

      meta.append(row1, sub, progress);

      // tail: ⋮ メニューと × キャンセルボタンを同じスロットに置き、showProgress/hideProgress で
      // 排他表示する (Hosts カードの .card-tail と同じ構造。launcher.css は共有のため触らない)。
      const tail = document.createElement("div");
      tail.className = "mwol-tail";
      tail.append(moreBtn, cancelBtn);

      card.append(avatar, meta, tail);
      listEl.appendChild(card);

      // ポーリング継続中に再描画された行 (別カードの追加/削除操作等) は進捗表示を復元する。
      const poll = activePolls.get(t.id);
      if (poll) showProgress(card, `起動待ち ${Math.round(poll.elapsedMs / 1000)}s`);

      // タップ = 主アクション (紐付けあり: 起こして接続 / 紐付けなし: 起こす)。
      // 削除は ⋮ メニュー経由に一本化している (不可視のジェスチャに破壊的操作を割り当てない方針)。
      card.addEventListener("click", () => {
        if (isBusy(t.id)) return; // 進捗表示中はタップを無視する
        // 誤タップで即 PC を起動してしまわないよう、主アクションの前に確認ダイアログを挟む。
        if (t.host) {
          openConfirmDialog(
            {
              title: "起動の確認",
              message: `「${name}」を起動して、起動後に接続しますか?`,
              confirmLabel: "起動して接続",
            },
            () => handleWakeAndConnect(t, card),
          );
        } else {
          openConfirmDialog(
            {
              title: "起動の確認",
              message: `「${name}」に起動信号 (マジックパケット) を送信しますか?`,
              confirmLabel: "起動",
            },
            () => handleWake(t, card),
          );
        }
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

  // 引っ張って更新 (Hosts タブと同じ共通部品)。
  attachPullToRefresh(listEl, refresh);

  void refresh();

  return { refresh };
}
