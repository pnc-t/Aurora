// 縦画面向けの接続 UI (モバイル専用)。
// デスクトップ版 (`ui/launcher.ts`) はサイドバー/ホスト編集/ショートカット設定/WoL 等
// 多機能だが、モバイルでは「接続履歴タップで接続」「＋で新規追加」「fps/Mbps の簡易設定」
// だけに絞る。
//
// パスワード保存 (DPAPI) は Windows 専用実装 (`common::dpapi`) で Android では機能しないため、
// 「パスワードを保存」トグルは出さず、保存済みパスワードでの即時接続もしない。毎回パスワード
// 入力を求め、`onConnect` の save は常に false を渡す。

import {
  DEFAULT_SETTINGS,
  deleteRecentHost,
  loadRecentHosts,
  loadSettings,
  saveSettings,
  type ClientSettings,
  type HostEntry,
} from "../core/ipc";
import { logger } from "../core/log";
import { icon } from "./icons";
import "./mobile-launcher.css";

export type MobileLauncherOpts = {
  root: HTMLElement;
  onConnect: (entry: HostEntry, password: string | null, save: boolean) => Promise<void>;
};

export type MobileLauncherHandle = {
  show: () => void;
  hide: () => void;
  refresh: () => Promise<void>;
};

/** fps 上限: 0 以上の整数 (0 = 無制限)。不正なら既定値へ丸める (モバイル簡易版はエラー表示を持たない)。 */
function parseFps(raw: string): number {
  const n = Math.floor(Number(raw));
  return Number.isFinite(n) && n >= 0 ? n : DEFAULT_SETTINGS.fps;
}
/** ビットレート上限 (Mbps): 1 以上の整数。 */
function parseMbps(raw: string): number {
  const n = Math.floor(Number(raw));
  return Number.isFinite(n) && n >= 1 ? n : DEFAULT_SETTINGS.max_mbps;
}
/** ポート番号: 1〜65535。範囲外は null (呼び出し側でエラー表示)。 */
function parsePort(raw: string): number | null {
  const n = Math.floor(Number(raw));
  return Number.isFinite(n) && n >= 1 && n <= 65535 ? n : null;
}

export function mountMobileLauncher(opts: MobileLauncherOpts): MobileLauncherHandle {
  opts.root.innerHTML = `
    <div class="ml-root">
      <div class="ml-header"><h1>Aurora</h1></div>
      <div class="ml-list" id="ml-list"></div>
      <button type="button" class="ml-add-btn" id="ml-add">${icon("add")} 新しい接続</button>
      <div class="ml-settings">
        <h2>設定</h2>
        <div class="ml-row">
          <label for="ml-fps">送出 fps 上限</label>
          <input type="text" inputmode="numeric" id="ml-fps" />
        </div>
        <div class="ml-row">
          <label for="ml-mbps">ビットレート上限 (Mbps)</label>
          <input type="text" inputmode="numeric" id="ml-mbps" />
        </div>
      </div>
    </div>
    <div class="ml-form-backdrop" id="ml-form-backdrop"></div>
  `;

  const listEl = opts.root.querySelector<HTMLElement>("#ml-list") as HTMLElement;
  const addBtn = opts.root.querySelector<HTMLButtonElement>("#ml-add");
  const backdrop = opts.root.querySelector<HTMLElement>("#ml-form-backdrop") as HTMLElement;
  const fpsInput = opts.root.querySelector<HTMLInputElement>("#ml-fps");
  const mbpsInput = opts.root.querySelector<HTMLInputElement>("#ml-mbps");

  let settings: ClientSettings = { ...DEFAULT_SETTINGS };
  let connecting = false;

  function persistSettings(): void {
    void saveSettings(settings).catch((e) => logger.warn("save settings failed", e));
  }
  fpsInput?.addEventListener("change", () => {
    settings = { ...settings, fps: parseFps(fpsInput.value) };
    fpsInput.value = String(settings.fps);
    persistSettings();
  });
  mbpsInput?.addEventListener("change", () => {
    settings = { ...settings, max_mbps: parseMbps(mbpsInput.value) };
    mbpsInput.value = String(settings.max_mbps);
    persistSettings();
  });

  function closeForm(): void {
    backdrop.classList.remove("visible");
    backdrop.replaceChildren();
  }
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeForm();
  });

  /**
   * パスワード入力欄 (目トグル付き) を持つフォームを描く共通処理。
   * `entry` が null なら新規ホスト追加 (host/port/label も入力させる)、
   * それ以外は既存ホストへの接続 (パスワードのみ)。
   */
  function openConnectForm(entry: HostEntry | null): void {
    const isNew = entry === null;
    const portDefault = entry?.port ?? settings.default_port;
    const form = document.createElement("form");
    form.className = "ml-form";
    form.autocomplete = "off";

    const title = document.createElement("h3");
    title.textContent = isNew ? "新しい接続" : entry.label || entry.host;
    form.appendChild(title);

    let hostInput: HTMLInputElement | null = null;
    let portInput: HTMLInputElement | null = null;
    let labelInput: HTMLInputElement | null = null;
    if (isNew) {
      const hostLabel = document.createElement("label");
      hostLabel.textContent = "host (Tailscale IP / hostname)";
      hostInput = document.createElement("input");
      hostInput.type = "text";
      hostInput.placeholder = "100.x.x.x";
      hostInput.required = true;
      hostLabel.appendChild(hostInput);
      form.appendChild(hostLabel);

      const portLabel = document.createElement("label");
      portLabel.textContent = "port";
      portInput = document.createElement("input");
      portInput.type = "text";
      portInput.inputMode = "numeric";
      portInput.value = String(portDefault);
      portLabel.appendChild(portInput);
      form.appendChild(portLabel);

      const labelLabel = document.createElement("label");
      labelLabel.textContent = "表示名 (任意)";
      labelInput = document.createElement("input");
      labelInput.type = "text";
      labelInput.placeholder = "home-pc";
      labelLabel.appendChild(labelInput);
      form.appendChild(labelLabel);
    } else {
      const info = document.createElement("div");
      info.style.color = "var(--muted)";
      info.style.fontSize = "13px";
      info.textContent = `${entry.host}:${entry.port}`;
      form.appendChild(info);
    }

    const passLabel = document.createElement("label");
    passLabel.textContent = "password";
    const passWrap = document.createElement("div");
    passWrap.className = "ml-pass-wrap";
    const passInput = document.createElement("input");
    passInput.type = "password";
    passInput.required = true;
    const eyeBtn = document.createElement("button");
    eyeBtn.type = "button";
    eyeBtn.className = "ml-pass-eye";
    eyeBtn.innerHTML = icon("eye"); // 固定 SVG のみなので innerHTML で問題ない
    eyeBtn.addEventListener("click", () => {
      const show = passInput.type === "password";
      passInput.type = show ? "text" : "password";
      eyeBtn.innerHTML = icon(show ? "eye-off" : "eye");
    });
    passWrap.append(passInput, eyeBtn);
    passLabel.appendChild(passWrap);
    form.appendChild(passLabel);

    const statusEl = document.createElement("div");
    statusEl.className = "ml-form-status";
    form.appendChild(statusEl);

    const actions = document.createElement("div");
    actions.className = "ml-form-actions";
    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.textContent = "キャンセル";
    cancelBtn.addEventListener("click", closeForm);
    const okBtn = document.createElement("button");
    okBtn.type = "submit";
    okBtn.className = "primary";
    okBtn.textContent = isNew ? "追加して接続" : "接続";
    actions.append(cancelBtn, okBtn);
    form.appendChild(actions);

    form.addEventListener("submit", (ev) => {
      ev.preventDefault();
      if (connecting) return;
      let target: HostEntry;
      if (isNew) {
        const host = hostInput?.value.trim() ?? "";
        if (!host) return;
        const port = parsePort(portInput?.value ?? "");
        if (port === null) {
          statusEl.textContent = "port は 1〜65535 の数値で入力してください";
          return;
        }
        const label = labelInput?.value.trim() ?? "";
        target = { label: label || host, host, port };
      } else {
        target = entry;
      }
      const password = passInput.value;
      if (!password) return;
      connecting = true;
      okBtn.disabled = true;
      okBtn.textContent = "接続中…";
      statusEl.textContent = "";
      // モバイルはパスワード非保存 (DPAPI が Windows 専用のため)。save は常に false。
      void opts
        .onConnect(target, password, false)
        .then(() => closeForm())
        .catch((e) => {
          statusEl.textContent = `接続に失敗しました: ${String(e)}`;
          okBtn.disabled = false;
          okBtn.textContent = isNew ? "追加して接続" : "接続";
          connecting = false;
        });
    });

    backdrop.replaceChildren(form);
    backdrop.classList.add("visible");
    (hostInput ?? passInput).focus();
  }

  addBtn?.addEventListener("click", () => openConnectForm(null));

  /** 削除確認の簡易フォーム (既存 ml-form-backdrop を流用)。 */
  function openDeleteConfirm(entry: HostEntry): void {
    const form = document.createElement("div");
    form.className = "ml-form";
    const title = document.createElement("h3");
    title.textContent = `「${entry.label || entry.host}」を削除しますか?`;
    form.appendChild(title);
    const actions = document.createElement("div");
    actions.className = "ml-form-actions";
    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.textContent = "キャンセル";
    cancelBtn.addEventListener("click", closeForm);
    const okBtn = document.createElement("button");
    okBtn.type = "button";
    okBtn.className = "primary";
    okBtn.textContent = "削除";
    okBtn.addEventListener("click", () => {
      void deleteRecentHost(entry.host)
        .then(() => refresh())
        .catch((e) => logger.warn("delete recent host failed", e))
        .finally(closeForm);
    });
    actions.append(cancelBtn, okBtn);
    form.appendChild(actions);
    backdrop.replaceChildren(form);
    backdrop.classList.add("visible");
  }

  function renderHosts(hosts: HostEntry[]): void {
    listEl.replaceChildren();
    if (hosts.length === 0) {
      const empty = document.createElement("div");
      empty.className = "ml-empty";
      empty.textContent = "接続履歴がありません。「+ 新しい接続」から追加してください";
      listEl.appendChild(empty);
      return;
    }
    for (const entry of hosts) {
      const card = document.createElement("button");
      card.type = "button";
      card.className = "ml-card";

      const avatar = document.createElement("div");
      avatar.className = "ml-card-avatar";
      avatar.textContent = (entry.label || entry.host || "?").slice(0, 1).toUpperCase();

      const info = document.createElement("div");
      info.className = "ml-card-info";
      const labelEl = document.createElement("div");
      labelEl.className = "ml-card-label";
      labelEl.textContent = entry.label || entry.host;
      const hostEl = document.createElement("div");
      hostEl.className = "ml-card-host";
      hostEl.textContent = `${entry.host}:${entry.port}`;
      info.append(labelEl, hostEl);
      card.append(avatar, info);
      listEl.appendChild(card);

      // 長押し (600ms) で削除確認、それ未満のタップは接続フォームを開く。
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
          openDeleteConfirm(entry);
        }, 600);
      });
      card.addEventListener("pointerup", clearPress);
      card.addEventListener("pointerleave", clearPress);
      card.addEventListener("pointercancel", clearPress);
      card.addEventListener("click", () => {
        if (longPressFired) return;
        openConnectForm(entry);
      });
    }
  }

  async function refresh(): Promise<void> {
    try {
      const hosts = await loadRecentHosts();
      renderHosts(hosts);
    } catch (e) {
      logger.warn("load recent hosts failed", e);
      renderHosts([]);
    }
    try {
      settings = await loadSettings();
    } catch {
      settings = { ...DEFAULT_SETTINGS };
    }
    if (fpsInput) fpsInput.value = String(settings.fps);
    if (mbpsInput) mbpsInput.value = String(settings.max_mbps);
  }

  return {
    show(): void {
      opts.root.classList.remove("hidden");
      void refresh();
    },
    hide(): void {
      opts.root.classList.add("hidden");
      closeForm();
    },
    refresh,
  };
}
