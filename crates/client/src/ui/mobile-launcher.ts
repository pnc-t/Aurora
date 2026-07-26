// モバイル (Android) 版ランチャー。デスクトップ版 (`ui/launcher.ts`) と見た目をほぼ揃える。
// デスクトップは「左サイドバー + コンテンツ」だが、モバイルはウィンドウが無く縦画面が基本なので
// サイドバーだけ「下部タブバー」に置き換える。titlebar/window-controls/statusbar (ウィンドウ前提の
// 要素) は作らない。
//
// 見た目の再利用方針:
// - `launcher.css` はセレクタがほぼ全てグローバル (`.launcher` 以外はスコープされていない) なので
//   `@import` ではなくこのファイルから直接 `import "./launcher.css"` し、`.host-card` / `.dialog` /
//   `.setting-row` 等をそのまま流用する。`launcher.css` 自体は変更しない (デスクトップと共有)。
// - `.page` / `.page.active` の表示切替、`.settings-tabs` / `.settings-sect` の切替ロジックも
//   デスクトップ (`launcher.ts`) と同じ実装をそのままモバイル用に持たせている。
//
// モバイル固有の縮小方針:
// - Hosts ページ: タップ=接続、カード右端の ⋮ =アクションシート (編集/削除) に統一する。
//   不可視のジェスチャ (長押し) に破壊的操作を割り当てない方針のため、長押し削除は廃止した。
//   デスクトップの ⋯ メニュー (card-menu、複数項目のポップアップ) の代わりに、共通部品の
//   アクションシート (mobile-sheet.ts) を使う。ドラッグ並べ替えは対象外。
// - Settings ページ: モバイルで実際に機能しない項目 (自動フルスクリーン/統計 HUD/マウスキャプチャ/
//   キーボード占有/クリップボード同期/音声ミュート/ツールバー・タブ配置/ショートカット/
//   ファイル共有/ログ) は出さない (呼び出し元 main.rs の invoke_handler から該当コマンドが
//   除外されているログは特に、出すと必ず失敗する)。デバイスピン留め (端末ID表示) も同様に
//   出さない: 取得元の `myDeviceFingerprint` が DPAPI (Windows 専用) 依存で Android では必ず失敗する。
// - 更新チェック UI (`.update-banner`) はモバイルには無い (`tauri-plugin-updater` 無効化のため)。
//
// パスワード保存 (DPAPI) は Windows 専用実装 (`common::dpapi`) で Android では機能しないため、
// 「パスワードを保存」トグルは出さず、保存済みパスワードでの即時接続もしない。毎回パスワード
// 入力を求め、`onConnect` の save は常に false を渡す。

import {
  DEFAULT_SETTINGS,
  deleteRecentHost,
  loadRecentHosts,
  loadSettings,
  probeHosts,
  saveRecentHost,
  saveSettings,
  type ClientSettings,
  type HostEntry,
  type HostOverrides,
  type ThemeId,
  type ToolbarPosition,
} from "../core/ipc";
import { getVersion } from "@tauri-apps/api/app";
import { probeSupportedCodecs, type CodecChoice } from "../core/codec";
import { logger } from "../core/log";
import {
  CUSTOM_PRESET_ID,
  findPreset,
  matchPreset,
  presetOptionsHtml,
  VIDEO_PRESETS,
} from "../core/video-presets";
import { applyAriaLabels, trapFocus } from "../core/a11y";
import { icon } from "./icons";
import { avatarColor, hasImageIcon, imageToIconDataUrl } from "./avatar";
import { mountMobileWolPanel } from "./mobile-wol";
import { enhanceSelects } from "./select-menu";
import { openActionSheet, type SheetAction } from "./mobile-sheet";
import { attachPullToRefresh } from "./pull-to-refresh";
import "./launcher.css";
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

/** コーデック select の表示ラベル (launcher.ts の CODEC_LABELS と同じ内容の複製)。 */
const CODEC_LABELS: Record<Exclude<CodecChoice, "auto">, string> = {
  h264: "H.264",
  hevc: "HEVC",
  av1: "AV1",
};

// ブラウザ (WebCodecs) が実際に対応するコーデック一覧。起動時に 1 回プローブしてここへ反映する。
let supportedCodecs: CodecChoice[] = [];

/** コーデック `<select>` の option 一覧。includeDefault=true でホスト別 override 用の
 *  「既定に従う」を先頭に足す (launcher.ts の codecOptionsHtml と同じ仕様)。非対応コーデックは
 *  disabled + 「(非対応)」表記にする。 */
function codecOptionsHtml(includeDefault: boolean): string {
  const items: string[] = [];
  if (includeDefault) items.push(`<option value="default">既定に従う</option>`);
  items.push(`<option value="auto">自動</option>`);
  (["h264", "hevc", "av1"] as const).forEach((c) => {
    const ok = supportedCodecs.includes(c);
    items.push(
      `<option value="${c}"${ok ? "" : " disabled"}>${CODEC_LABELS[c]}${ok ? "" : " (非対応)"}</option>`,
    );
  });
  return items.join("");
}

function escapeHtml(s: string): string {
  return s.replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] ?? c,
  );
}

/** fps 上限: 0 以上の整数 (0 = 無制限)。不正なら既定値へ丸める。 */
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

/** 数秒だけ表示して自動で消える行内エラー (launcher.ts の flashInlineError と同じ体裁)。 */
function flashInlineError(el: HTMLElement | null, message: string): void {
  if (!el) return;
  el.textContent = message;
  window.setTimeout(() => {
    if (el.textContent === message) el.textContent = "";
  }, 3000);
}

/** アクセント色を CSS 変数へ反映する。 */
function applyAccent(color: string): void {
  document.documentElement.style.setProperty("--accent", color);
}

/** 配色テーマを <html data-theme> へ反映する (launcher.ts の applyTheme と同じ処理の複製。
 *  launcher.ts は変更禁止のため、モバイル側で数行複製して持つ)。 */
function applyTheme(theme: ThemeId, accent: string): void {
  document.documentElement.dataset.theme = theme;
  applyAccent(accent);
}

export function mountMobileLauncher(opts: MobileLauncherOpts): MobileLauncherHandle {
  opts.root.innerHTML = `
    <div class="ml-app">
      <main class="content">
        <section class="page active" id="page-hosts">
          <div class="page-head">
            <h2>Hosts</h2>
            <div class="actions">
              <button class="icon-btn ml-icon-btn-labeled" id="probe-refresh" title="オンライン状態を更新">${icon("refresh")}<span>更新</span></button>
              <button class="icon-btn ml-icon-btn-labeled" id="add-host" title="ホストを追加">${icon("add")}<span>追加</span></button>
            </div>
          </div>
          <div class="host-list" id="host-list"></div>
        </section>
        <section class="page" id="page-wol"></section>
        <section class="page" id="page-settings">
          <div class="page-head"><h2>Settings</h2></div>
          <div class="settings-tabs">
            <button type="button" class="settings-tab active" data-sect="av">映像・接続</button>
            <button type="button" class="settings-tab" data-sect="display">表示</button>
            <button type="button" class="settings-tab" data-sect="other">その他</button>
          </div>
          <div class="settings-body">
            <div class="settings-sect active" data-sect="av">
              <div class="ml-set-block">
              <div class="setting-group">映像 (接続時にホストへ適用)</div>
              <div class="setting-card">
              <div class="setting-row">
                <div class="s-meta">
                  <div class="s-title">画質プリセット</div>
                  <div class="s-desc">fps とビットレートをまとめて設定。手動で変えるとカスタムになる</div>
                </div>
                <select id="set-preset" class="select-input">${presetOptionsHtml()}</select>
              </div>
              <div class="setting-row" data-custom-only>
                <div class="s-meta">
                  <div class="s-title">送出 fps 上限</div>
                  <div class="s-desc">0 で無制限。高リフレッシュレートで遅延が増えるのを防ぐ</div>
                </div>
                <input type="text" inputmode="numeric" id="set-fps" class="num-input" />
              </div>
              <div class="setting-row" data-custom-only>
                <div class="s-meta">
                  <div class="s-title">ビットレート上限 (Mbps)</div>
                  <div class="s-desc">この値を上限に、輻輳時は自動で下げる (適応制御)</div>
                </div>
                <input type="text" inputmode="numeric" id="set-mbps" class="num-input" />
              </div>
              <div class="setting-row">
                <div class="s-meta">
                  <div class="s-title">映像コーデック</div>
                  <div class="s-desc">自動はホストと対応コーデックを相談して選ぶ。手動でこの端末が非対応のものを選ぶと H.264 にフォールバックする</div>
                </div>
                <select id="set-codec" class="select-input">${codecOptionsHtml(false)}</select>
              </div>
              </div>
              </div>
              <div class="ml-set-block">
              <div class="setting-group">接続</div>
              <div class="setting-card">
              <div class="setting-row">
                <div class="s-meta">
                  <div class="s-title">切断時に自動で再接続</div>
                  <div class="s-desc">通信断などで切れたとき、間隔を空けて自動的に接続し直す</div>
                </div>
                <label class="switch"><input type="checkbox" id="set-reconnect" /><span class="slider"></span></label>
              </div>
              <div class="setting-row">
                <div class="s-meta">
                  <div class="s-title">仮想ディスプレイを使う (RDP 風)</div>
                  <div class="s-desc">ホストに仮想ディスプレイがあれば、それをキャプチャし画面に解像度を合わせる (黒帯解消)。物理画面とは別の空のデスクトップになる点に注意</div>
                </div>
                <label class="switch"><input type="checkbox" id="set-virtual" /><span class="slider"></span></label>
              </div>
              </div>
              </div>
            </div>
            <div class="settings-sect" data-sect="display">
              <div class="ml-set-block">
              <div class="setting-group">操作バー</div>
              <div class="setting-card">
              <div class="setting-row">
                <div class="s-meta">
                  <div class="s-title">配置</div>
                  <div class="s-desc">操作バーを画面のどの端に置くか。左右に置くと縦並びのサイドバーになる</div>
                </div>
                <select id="set-tb-position" class="select-input">
                  <option value="top">上</option>
                  <option value="bottom">下</option>
                  <option value="left">左</option>
                  <option value="right">右</option>
                </select>
              </div>
              </div>
              </div>
              <div class="ml-set-block">
              <div class="setting-group">外観・既定値</div>
              <div class="setting-card">
              <div class="setting-row">
                <div class="s-meta">
                  <div class="s-title">テーマ</div>
                  <div class="s-desc">配色。auto は OS のダーク/ライト設定に追従。legacy はグレーのクラシック調</div>
                </div>
                <select id="set-theme" class="select-input">
                  <option value="auto">システム連動</option>
                  <option value="dark">ダーク</option>
                  <option value="light">ライト</option>
                  <option value="legacy">レガシー</option>
                </select>
              </div>
              <div class="setting-row">
                <div class="s-meta">
                  <div class="s-title">アクセントカラー</div>
                  <div class="s-desc">ボタンや選択中の表示に使う色</div>
                </div>
                <label class="ml-tap44"><input type="color" id="set-accent" class="color-input" /></label>
              </div>
              <div class="setting-row">
                <div class="s-meta">
                  <div class="s-title">新規ホストの既定ポート</div>
                  <div class="s-desc">「＋追加」ダイアログの port 欄の初期値</div>
                </div>
                <span class="shortcut-error" data-num-error="port"></span>
                <input type="text" inputmode="numeric" id="set-port" class="num-input" />
              </div>
              </div>
              </div>
            </div>
            <div class="settings-sect" data-sect="other">
              <div class="ml-set-block">
              <div class="setting-group">診断</div>
              <div class="setting-card">
              <div class="setting-row">
                <div class="s-meta">
                  <div class="s-title">バージョン</div>
                </div>
                <span id="app-version" class="s-desc">…</span>
              </div>
              </div>
              </div>
            </div>
          </div>
        </section>
      </main>
      <nav class="ml-tabbar">
        <button type="button" class="ml-tab active" data-page="hosts">${icon("hosts")}<span>Hosts</span></button>
        <button type="button" class="ml-tab" data-page="wol">${icon("bolt")}<span>WoL</span></button>
        <button type="button" class="ml-tab" data-page="settings">${icon("settings")}<span>Settings</span></button>
      </nav>
      <div id="dialog-backdrop"></div>
    </div>
  `;

  // 設定ページの select を自作ドロップダウンへ (デスクトップ launcher.ts と同じ位置・同じ理由:
  // 設定値の反映より前に済ませる)。ネイティブ select を残すと見た目がデスクトップと明確に
  // 食い違うため、Android のネイティブピッカーではなくこちらへ揃える。
  enhanceSelects(opts.root);

  const $ = <T extends HTMLElement>(sel: string) => opts.root.querySelector<T>(sel);
  const hostList = $("#host-list") as HTMLElement;
  const backdrop = $("#dialog-backdrop") as HTMLElement;
  const presetSelect = $<HTMLSelectElement>("#set-preset");
  const fpsInput = $<HTMLInputElement>("#set-fps");
  const mbpsInput = $<HTMLInputElement>("#set-mbps");
  const codecSelect = $<HTMLSelectElement>("#set-codec");
  const reconnectToggle = $<HTMLInputElement>("#set-reconnect");
  const virtualToggle = $<HTMLInputElement>("#set-virtual");
  const tbPositionSelect = $<HTMLSelectElement>("#set-tb-position");
  const themeSelect = $<HTMLSelectElement>("#set-theme");
  const accentInput = $<HTMLInputElement>("#set-accent");
  const portInput = $<HTMLInputElement>("#set-port");
  const portError = $<HTMLElement>('[data-num-error="port"]');

  // WoL タブ: 中継 (raspi) 経由でマジックパケットを送るパネル。
  // 旧 wol-panel の「起こして接続」は保存済みパスワード (DPAPI = Windows 専用) を前提にしており
  // Android では必ず失敗するため使わない。モバイル専用パネルは到達確認のみ行い、到達したら
  // 毎回パスワード入力を求める接続ダイアログ (openConnectDialog) を開く配線にする。
  // openConnectDialog は下方で宣言されるため、参照時点でまだ定義されていない可能性がある
  // (宣言順の都合)。アロー関数で包んで呼び出し時まで評価を遅延させる。
  const wolRoot = $("#page-wol") as HTMLElement;
  mountMobileWolPanel(wolRoot, {
    onReachable: (entry) => openConnectDialog(entry),
  });

  applyAriaLabels(opts.root);

  // --- 下部タブバー: ページ切替 (デスクトップのサイドバー切替と同じロジック) ---
  opts.root.querySelectorAll<HTMLButtonElement>(".ml-tab").forEach((btn) => {
    btn.addEventListener("click", () => {
      const page = btn.dataset.page;
      opts.root.querySelectorAll(".ml-tab").forEach((b) => b.classList.toggle("active", b === btn));
      opts.root.querySelectorAll(".page").forEach((p) => {
        p.classList.toggle("active", p.id === `page-${page}`);
      });
    });
  });

  // --- Settings サブタブ切替 (デスクトップと同じ) ---
  opts.root.querySelectorAll<HTMLButtonElement>(".settings-tab").forEach((btn) => {
    btn.addEventListener("click", () => {
      const sect = btn.dataset.sect;
      opts.root.querySelectorAll(".settings-tab").forEach((b) => b.classList.toggle("active", b === btn));
      opts.root.querySelectorAll(".settings-sect").forEach((s) => {
        s.classList.toggle("active", (s as HTMLElement).dataset.sect === sect);
      });
      opts.root.querySelector(".settings-body")?.scrollTo({ top: 0 });
    });
  });

  // --- ダイアログ共通 ---
  let releaseTrap: (() => void) | null = null;
  function openBackdrop(): void {
    backdrop.classList.add("visible");
    applyAriaLabels(backdrop);
    // 呼び出し側が select.value を入れるより前に差し替える (setter でラベルが追従するため)。
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

  /** 削除確認ダイアログ (デスクトップの openConfirmDialog と同じ見た目)。 */
  function openConfirmDialog(message: string, onConfirm: () => Promise<void>): void {
    backdrop.innerHTML = `
      <div class="dialog">
        <h3>削除の確認</h3>
        <div class="s-desc">${message}</div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="c-cancel">cancel</button>
          <button type="button" class="btn-danger" id="c-ok">削除</button>
        </div>
      </div>
    `;
    openBackdrop();
    backdrop.querySelector<HTMLButtonElement>("#c-cancel")?.focus();
    backdrop.querySelector("#c-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector("#c-ok")?.addEventListener("click", () => {
      void onConfirm().finally(closeDialog);
    });
  }

  /** ホスト追加/編集ダイアログ (デスクトップの openHostDialog と項目・組み立てを揃える)。
   *  entry=null で新規追加、指定時は編集。ホスト別設定は <details> に折りたたむ。 */
  function openHostDialog(entry: HostEntry | null): void {
    const isAdd = entry === null;
    const e: HostEntry = entry ?? {
      label: "",
      host: "",
      port: settings.default_port,
    };
    const o: HostOverrides = e.overrides ?? {};
    const hasQuality = o.fps != null && o.max_mbps != null;
    const qualityId = hasQuality ? matchPreset(o.fps as number, o.max_mbps as number) : "default";
    // 手で JSON を編集した等でプリセット外の値が入っている場合は現値ラベルで残す (保存時は維持)。
    const customOpt =
      qualityId === CUSTOM_PRESET_ID
        ? `<option value="${CUSTOM_PRESET_ID}">カスタム (${o.fps}fps / ${o.max_mbps}Mbps)</option>`
        : "";
    const presetOpts =
      `<option value="default">既定に従う</option>` +
      VIDEO_PRESETS.map((p) => `<option value="${p.id}">${p.label}</option>`).join("") +
      customOpt;
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>${isAdd ? "ホストを追加" : "ホストを編集"}</h3>
        <div class="icon-picker">
          <div class="icon-preview" id="h-icon-preview"></div>
          <div class="icon-picker-actions">
            <button type="button" class="btn-secondary" id="h-icon-pick">画像を選択</button>
            <button type="button" class="btn-ghost" id="h-icon-clear">解除</button>
          </div>
          <input type="file" id="h-icon-file" accept="image/*" hidden />
        </div>
        <label>表示名 (任意)<input id="h-label" type="text" value="${escapeHtml(e.label)}" placeholder="home-pc" /></label>
        <label>host (Tailscale IP / hostname)<input id="h-host" type="text" value="${escapeHtml(e.host)}" placeholder="100.x.x.x" required /></label>
        <label>port
          <input id="h-port" type="text" inputmode="numeric" value="${e.port}" />
          <span class="shortcut-error" data-h-port-error></span>
        </label>
        <details class="ml-host-adv">
          <summary>詳細設定 (このホストのみ)</summary>
          <label>仮想ディスプレイ
            <select id="h-virtual" class="select-input">
              <option value="default">既定に従う</option>
              <option value="on">使う</option>
              <option value="off">使わない</option>
            </select>
          </label>
          <label>画質プリセット<select id="h-preset" class="select-input">${presetOpts}</select></label>
          <label>コーデック<select id="h-codec" class="select-input">${codecOptionsHtml(true)}</select></label>
        </details>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="h-cancel">cancel</button>
          <button type="submit" class="btn-primary">${isAdd ? "add" : "save"}</button>
        </div>
      </form>
    `;
    openBackdrop();

    const virtualSel = backdrop.querySelector<HTMLSelectElement>("#h-virtual");
    const presetSel = backdrop.querySelector<HTMLSelectElement>("#h-preset");
    const codecSel = backdrop.querySelector<HTMLSelectElement>("#h-codec");
    if (virtualSel)
      virtualSel.value = o.use_virtual_display == null ? "default" : o.use_virtual_display ? "on" : "off";
    if (presetSel) presetSel.value = qualityId;
    if (codecSel) codecSel.value = o.codec == null ? "default" : o.codec;

    let currentIcon = e.icon ?? "";
    const labelInput = backdrop.querySelector<HTMLInputElement>("#h-label");
    const hostInput = backdrop.querySelector<HTMLInputElement>("#h-host");
    const preview = backdrop.querySelector<HTMLElement>("#h-icon-preview");
    const fileInput = backdrop.querySelector<HTMLInputElement>("#h-icon-file");
    const clearBtn = backdrop.querySelector<HTMLButtonElement>("#h-icon-clear");
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
        const nm = labelInput?.value.trim() || hostInput?.value.trim() || "?";
        preview.style.background = avatarColor(nm);
        preview.textContent = (nm[0] ?? "?").toUpperCase();
      }
      if (clearBtn) clearBtn.style.display = currentIcon ? "" : "none";
    }
    renderIconPreview();
    labelInput?.addEventListener("input", () => {
      if (!hasImageIcon(currentIcon)) renderIconPreview();
    });
    hostInput?.addEventListener("input", () => {
      if (!hasImageIcon(currentIcon)) renderIconPreview();
    });
    backdrop.querySelector("#h-icon-pick")?.addEventListener("click", () => fileInput?.click());
    fileInput?.addEventListener("change", () => {
      const file = fileInput.files?.[0];
      fileInput.value = "";
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

    hostInput?.focus();
    backdrop.querySelector("#h-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (ev) => {
      ev.preventDefault();
      const host = hostInput?.value.trim() ?? "";
      const label = labelInput?.value.trim() ?? "";
      if (!host) return;
      const port = parsePort(backdrop.querySelector<HTMLInputElement>("#h-port")?.value ?? "");
      if (port === null) {
        flashInlineError(
          backdrop.querySelector<HTMLElement>("[data-h-port-error]"),
          "1〜65535 の数値を入れてください",
        );
        return;
      }
      const ov: HostOverrides = {};
      if (virtualSel?.value === "on") ov.use_virtual_display = true;
      else if (virtualSel?.value === "off") ov.use_virtual_display = false;
      if (codecSel?.value && codecSel.value !== "default") ov.codec = codecSel.value as CodecChoice;
      const pv = presetSel?.value ?? "default";
      if (pv === CUSTOM_PRESET_ID) {
        // プリセット外の既存値をそのまま維持
        ov.fps = o.fps;
        ov.max_mbps = o.max_mbps;
      } else {
        const p = findPreset(pv);
        if (p) {
          ov.fps = p.fps;
          ov.max_mbps = p.mbps;
        }
      }
      void (async () => {
        // host を変えた編集なら旧エントリを消してから保存する (デスクトップ launcher.ts と同じ挙動:
        // 新 host で重複排除される)。
        const hostChanged = e.host.localeCompare(host, undefined, { sensitivity: "accent" }) !== 0;
        if (!isAdd && hostChanged) {
          await deleteRecentHost(e.host);
        }
        // overrides は空オブジェクトも常に送る: Rust 側が「既定に従う」への明示リセットとして
        // None へ正規化する (undefined だと「情報なし」扱いで既存値が残ってしまう)。icon も
        // 常に明示値 (空文字 = 解除) を送る。
        await saveRecentHost({
          label: label || host,
          host,
          port,
          icon: currentIcon,
          overrides: ov,
        });
        closeDialog();
        await refresh();
      })().catch((e) => logger.warn("save host failed", e));
    });
  }

  /** 接続ダイアログ (パスワードのみ。パスワード保存トグルは出さない: save は常に false)。 */
  function openConnectDialog(entry: HostEntry): void {
    backdrop.innerHTML = `
      <form class="dialog" autocomplete="off">
        <h3>${escapeHtml(entry.label || entry.host)}</h3>
        <div class="s-desc" style="font-family:monospace">${escapeHtml(entry.host)}:${entry.port}</div>
        <label>password
          <span class="pass-wrap">
            <input id="d-pass" type="password" required />
            <button type="button" class="pass-eye" id="d-eye" title="パスワードを表示">${icon("eye")}</button>
          </span>
        </label>
        <div class="dlg-status" id="d-status"></div>
        <div class="dlg-actions">
          <button type="button" class="btn-ghost" id="d-cancel">cancel</button>
          <button type="submit" class="btn-primary" id="d-ok">connect</button>
        </div>
      </form>
    `;
    openBackdrop();
    const passInput = backdrop.querySelector<HTMLInputElement>("#d-pass");
    const okBtn = backdrop.querySelector<HTMLButtonElement>("#d-ok");
    const statusEl = backdrop.querySelector<HTMLElement>("#d-status");
    const eyeBtn = backdrop.querySelector<HTMLButtonElement>("#d-eye");
    eyeBtn?.addEventListener("click", () => {
      if (!passInput) return;
      const show = passInput.type === "password";
      passInput.type = show ? "text" : "password";
      eyeBtn.innerHTML = icon(show ? "eye-off" : "eye");
      eyeBtn.title = show ? "パスワードを隠す" : "パスワードを表示";
      passInput.focus();
    });
    passInput?.focus();
    backdrop.querySelector("#d-cancel")?.addEventListener("click", closeDialog);
    backdrop.querySelector<HTMLFormElement>(".dialog")?.addEventListener("submit", (ev) => {
      ev.preventDefault();
      const password = passInput?.value ?? "";
      if (!password) return;
      if (okBtn) {
        okBtn.disabled = true;
        okBtn.textContent = "connecting…";
      }
      if (statusEl) statusEl.textContent = `connecting to ${entry.host}…`;
      // モバイルはパスワード非保存 (DPAPI が Windows 専用のため)。save は常に false。
      void opts
        .onConnect(entry, password, false)
        .then(() => closeDialog())
        .catch((err) => {
          if (statusEl) {
            statusEl.textContent = `error: ${String(err)}`;
            statusEl.classList.add("error");
          }
          if (okBtn) {
            okBtn.disabled = false;
            okBtn.textContent = "connect";
          }
        });
    });
  }

  // --- ホスト一覧の描画 ---
  let currentHosts: HostEntry[] = [];
  const probeMap = new Map<string, boolean>();
  let probing = false;
  const refreshBtn = $("#probe-refresh");

  /** 未接続の保存ホストへ並列 TCP プローブし、結果をカードのドットへ反映する
   *  (デスクトップの runProbe と同じ挙動。最低 500ms は更新ボタンを回す)。 */
  async function runProbe(): Promise<void> {
    if (probing) return;
    const targets = currentHosts.map((h) => ({ host: h.host, port: h.port }));
    probing = true;
    refreshBtn?.classList.add("spinning");
    const started = Date.now();
    try {
      if (targets.length > 0) {
        const results = await probeHosts(targets);
        for (const r of results) probeMap.set(r.host.trim().toLowerCase(), r.online);
        renderHosts(currentHosts);
      }
    } catch (e) {
      logger.warn("probe hosts failed", e);
    } finally {
      const wait = Math.max(0, 500 - (Date.now() - started));
      window.setTimeout(() => {
        refreshBtn?.classList.remove("spinning");
        probing = false;
      }, wait);
    }
  }

  /** 最終接続日時 (unix 秒) を「たった今/N分前/N時間前/N日前」に整形する (デスクトップと同じ)。 */
  function formatRelativeConnected(unixSec: number): string {
    const diffSec = Math.max(0, Math.floor(Date.now() / 1000) - unixSec);
    if (diffSec < 60) return "たった今";
    const diffMin = Math.floor(diffSec / 60);
    if (diffMin < 60) return `${diffMin}分前`;
    const diffHour = Math.floor(diffMin / 60);
    if (diffHour < 24) return `${diffHour}時間前`;
    const diffDay = Math.floor(diffHour / 24);
    return `${diffDay}日前`;
  }

  function renderHosts(hosts: HostEntry[]): void {
    currentHosts = hosts;
    hostList.replaceChildren();
    if (hosts.length === 0) {
      const div = document.createElement("div");
      div.className = "empty-guide";
      div.innerHTML = `
        <div class="empty-guide-title">まだホストがありません</div>
        <ol class="empty-guide-steps">
          <li class="empty-guide-step"><span class="empty-guide-badge">1</span><span>操作される PC に Aurora Host をインストール</span></li>
          <li class="empty-guide-step"><span class="empty-guide-badge">2</span><span>両方の端末を Tailscale で同じ Tailnet に入れる</span></li>
          <li class="empty-guide-step"><span class="empty-guide-badge">3</span><span>右上の ＋ から IP とパスワードを登録</span></li>
        </ol>`;
      hostList.appendChild(div);
      return;
    }
    for (const entry of hosts) {
      const name = entry.label || entry.host;
      const card = document.createElement("div");
      card.className = "host-card";
      card.tabIndex = 0;
      card.setAttribute("role", "button");

      const avatarWrap = document.createElement("div");
      avatarWrap.className = "avatar-wrap";
      const avatar = document.createElement("div");
      if (hasImageIcon(entry.icon)) {
        avatar.className = "avatar avatar-img";
        const img = document.createElement("img");
        img.src = entry.icon; // 自前生成の data URL
        img.alt = "";
        img.draggable = false;
        avatar.appendChild(img);
      } else {
        avatar.className = "avatar";
        avatar.style.background = avatarColor(name);
        avatar.textContent = (name[0] ?? "?").toUpperCase();
      }
      avatarWrap.appendChild(avatar);
      const online = probeMap.get(entry.host.trim().toLowerCase());
      if (online !== undefined) {
        const presence = document.createElement("span");
        presence.className = `presence ${online ? "online" : "offline"}`;
        avatarWrap.appendChild(presence);
      }

      const meta = document.createElement("div");
      meta.className = "meta";
      const labelRow = document.createElement("div");
      labelRow.className = "label-row";
      const labelEl = document.createElement("span");
      labelEl.className = "label";
      labelEl.textContent = name;
      labelRow.appendChild(labelEl);
      // スマホ幅では「IP:port · N時間前」が addr 行 1 行に収まらず溢れるため、相対時刻は
      // ラベル行の右端 (.ml-last-connected) へ移し、addr は host:port のみにする。
      if (entry.last_connected != null) {
        const lc = document.createElement("span");
        lc.className = "ml-last-connected";
        lc.textContent = formatRelativeConnected(entry.last_connected);
        labelRow.appendChild(lc);
      }
      const addr = document.createElement("div");
      addr.className = "addr";
      addr.textContent = `${entry.host}:${entry.port}`;
      meta.append(labelRow, addr);

      const tail = document.createElement("div");
      tail.className = "card-tail";
      const moreBtn = document.createElement("button");
      moreBtn.type = "button";
      moreBtn.className = "card-btn";
      moreBtn.title = "メニュー";
      moreBtn.innerHTML = icon("more"); // 固定 SVG のみなので innerHTML で可
      tail.appendChild(moreBtn);

      card.append(avatarWrap, meta, tail);
      hostList.appendChild(card);

      // ⋮ は不可視の長押しではなく明示タップでメニューを開く (編集/削除)。カードへの
      // クリック伝播 (=接続) を止める。
      moreBtn.addEventListener("click", (ev) => {
        ev.stopPropagation();
        const actions: SheetAction[] = [
          {
            label: "編集",
            icon: "edit",
            onSelect: () => openHostDialog(entry),
          },
          {
            label: "削除",
            icon: "delete",
            danger: true,
            onSelect: () => {
              openConfirmDialog(`「${escapeHtml(name)}」を一覧から削除しますか?`, async () => {
                await deleteRecentHost(entry.host);
                await refresh();
              });
            },
          },
        ];
        openActionSheet(actions);
      });
      card.addEventListener("click", () => {
        openConnectDialog(entry);
      });
      card.addEventListener("keydown", (e) => {
        if (e.target !== card) return;
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          card.click();
        }
      });
    }
    applyAriaLabels(hostList);
  }

  $("#add-host")?.addEventListener("click", () => openHostDialog(null));
  $("#probe-refresh")?.addEventListener("click", () => void runProbe());
  attachPullToRefresh(hostList, runProbe);

  // --- 設定 ---
  let settings: ClientSettings = { ...DEFAULT_SETTINGS };

  function persist(): void {
    void saveSettings(settings).catch((e) => logger.warn("save settings failed", e));
  }

  /** fps/mbps の現在値からプリセット選択を一致させる (手動値なら「カスタム」)。 */
  function syncPresetSelect(): void {
    if (presetSelect) presetSelect.value = matchPreset(settings.fps, settings.max_mbps);
  }
  // fps/Mbps の生値はプリセットが決めるカスタム時のみ意味を持つため、スマホ幅の縦積みで
  // 場所を取る 2 行を「カスタム」選択時だけ出す (値自体は非表示中も保持・変更しない)。
  const customOnlyRows = opts.root.querySelectorAll<HTMLElement>("[data-custom-only]");
  function syncCustomOnlyRows(): void {
    const isCustom = presetSelect?.value === CUSTOM_PRESET_ID;
    customOnlyRows.forEach((row) => row.classList.toggle("ml-hidden", !isCustom));
  }
  presetSelect?.addEventListener("change", () => {
    const p = findPreset(presetSelect.value);
    if (p) {
      settings = { ...settings, fps: p.fps, max_mbps: p.mbps };
      if (fpsInput) fpsInput.value = String(p.fps);
      if (mbpsInput) mbpsInput.value = String(p.mbps);
      persist();
    } // カスタム: fps/Mbps の手動値を維持する
    syncCustomOnlyRows();
  });
  fpsInput?.addEventListener("change", () => {
    settings = { ...settings, fps: parseFps(fpsInput.value) };
    fpsInput.value = String(settings.fps);
    syncPresetSelect();
    persist();
  });
  mbpsInput?.addEventListener("change", () => {
    settings = { ...settings, max_mbps: parseMbps(mbpsInput.value) };
    mbpsInput.value = String(settings.max_mbps);
    syncPresetSelect();
    persist();
  });
  codecSelect?.addEventListener("change", () => {
    settings = { ...settings, codec: codecSelect.value as CodecChoice };
    persist();
  });
  reconnectToggle?.addEventListener("change", () => {
    settings = { ...settings, auto_reconnect: reconnectToggle.checked };
    persist();
  });
  virtualToggle?.addEventListener("change", () => {
    settings = { ...settings, use_virtual_display: virtualToggle.checked };
    persist();
  });
  tbPositionSelect?.addEventListener("change", () => {
    settings = {
      ...settings,
      toolbar_position: tbPositionSelect.value as ToolbarPosition,
    };
    persist();
  });
  themeSelect?.addEventListener("change", () => {
    const theme = themeSelect.value as ThemeId;
    settings = { ...settings, theme };
    applyTheme(theme, settings.accent_color);
    persist();
  });
  // ドラッグ中の input では保存を連打しない。プレビューのみ即時反映し、保存は指を離した
  // (色を確定した) change で 1 回だけ行う (launcher.ts と同じ配慮)。
  accentInput?.addEventListener("input", () => {
    applyAccent(accentInput.value);
  });
  accentInput?.addEventListener("change", () => {
    settings = { ...settings, accent_color: accentInput.value };
    persist();
  });
  portInput?.addEventListener("change", () => {
    const v = parsePort(portInput.value);
    if (v === null) {
      flashInlineError(portError, "1〜65535 の数値を入れてください");
      portInput.value = String(settings.default_port);
      return;
    }
    settings = { ...settings, default_port: v };
    portInput.value = String(v);
    persist();
  });

  /** コーデック select の option を最新のプローブ結果で作り直し、現在値を反映する。 */
  function syncCodecSelect(): void {
    if (!codecSelect) return;
    codecSelect.innerHTML = codecOptionsHtml(false);
    codecSelect.value = settings.codec;
  }
  void probeSupportedCodecs().then((list) => {
    supportedCodecs = list;
    syncCodecSelect();
  });

  /** 現在の settings を設定フォームの各コントロールへ反映する (テーマ/アクセント色は即適用)。 */
  function applySettingsToForm(): void {
    if (fpsInput) fpsInput.value = String(settings.fps);
    if (mbpsInput) mbpsInput.value = String(settings.max_mbps);
    if (reconnectToggle) reconnectToggle.checked = settings.auto_reconnect;
    if (virtualToggle) virtualToggle.checked = settings.use_virtual_display;
    if (tbPositionSelect) tbPositionSelect.value = settings.toolbar_position;
    if (themeSelect) themeSelect.value = settings.theme;
    if (accentInput) accentInput.value = settings.accent_color;
    if (portInput) portInput.value = String(settings.default_port);
    syncPresetSelect();
    syncCodecSelect();
    syncCustomOnlyRows();
    applyTheme(settings.theme, settings.accent_color);
  }

  // 診断: アプリのバージョン表示 (updater/process プラグインはモバイル無効のため getVersion を使う)。
  const versionEl = $("#app-version");
  if (versionEl) {
    void getVersion()
      .then((v) => {
        versionEl.textContent = v;
      })
      .catch((e) => {
        logger.warn("app version fetch failed", e);
        versionEl.textContent = "取得できませんでした";
      });
  }

  async function refresh(): Promise<void> {
    try {
      renderHosts(await loadRecentHosts());
      void runProbe();
    } catch (e) {
      logger.warn("load recent hosts failed", e);
      renderHosts([]);
    }
    try {
      settings = await loadSettings();
    } catch {
      settings = { ...DEFAULT_SETTINGS };
    }
    applySettingsToForm();
  }

  void refresh();

  return {
    show(): void {
      opts.root.classList.remove("hidden");
      // 単一 WebView 内で mobile-main.ts (接続中の操作パッド位置/fps/mbps) が同じ設定ファイルを
      // 独立に書き込むため、表示のたびに読み直さないと古いキャッシュの persist() で全体上書きし
      // 他方の変更を消してしまう (例: パッド位置を保存→切断→ランチャーでテーマ変更、で位置が戻る)。
      void refresh();
    },
    hide(): void {
      opts.root.classList.add("hidden");
      closeDialog();
    },
    refresh,
  };
}
