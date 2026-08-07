// ファイル転送 UI。
// - アップロード (クライアント → ホスト): キャプチャ画面へのドラッグ&ドロップで行う (main.ts)。
//   ここには操作ヒントと進捗表示を置く。
// - ダウンロード (ホスト → クライアント): ホスト上のパスを指定して取得。
//   受信ファイルはクライアント側の %USERPROFILE%\Aurora\downloads に自動保存される。
// 共有フォルダ (ProjFS ドライブ投影) の設定は launcher の設定画面に移設した (接続のたびに
// 自動適用されるため、転送パネルには持たない)。
// ダウンロードの完了 / エラーは host からの FileResult (転送 id で照合)、進行中の割合は
// FileProgress (done/total バイト) で更新する。

import { requestFile } from "../core/ipc";
import { logger } from "../core/log";
import "./file-transfer.css";

// 過去に取得したホスト上のパス (datalist 補完用)。ローカルの UI 選好なので localStorage に持つ。
const PATH_HISTORY_KEY = "aurora.download.paths";
const PATH_HISTORY_MAX = 5;

function loadPathHistory(): string[] {
  try {
    const raw = localStorage.getItem(PATH_HISTORY_KEY);
    const arr: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(arr) ? arr.filter((x): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}

function savePathHistory(path: string): void {
  const next = [path, ...loadPathHistory().filter((p) => p !== path)].slice(0, PATH_HISTORY_MAX);
  try {
    localStorage.setItem(PATH_HISTORY_KEY, JSON.stringify(next));
  } catch (e) {
    logger.warn("save download path history failed", e);
  }
}

let downloadProgressEl: HTMLElement | null = null;
let downloadBarEl: HTMLElement | null = null;
let uploadProgressEl: HTMLElement | null = null;
let uploadBarEl: HTMLElement | null = null;
let pathListEl: HTMLDataListElement | null = null;
// ダウンロードボタン (状態再同期で有効/無効を切り替えるためモジュールに保持)。
let downloadBtn: HTMLButtonElement | null = null;

/**
 * ダウンロード状態はタブ (セッション) 単位。file パネルの DOM は窓に 1 つしか無いが、
 * 進行中の要求は「どのタブが開始したか」に紐づく。単一のモジュール変数のままだと、
 * タブ A で開始したダウンロードが完了する前に B へ切り替えると FileResult が握り潰され、
 * A に戻っても再同期する経路が無いため `activeDownloadId`/ボタンの disabled が永久に
 * 残ってしまう (H-4)。そのためセッション id をキーにした Map で保持する。
 */
interface DownloadState {
  /** 進行中の要求 id。未実施/完了後は null。 */
  id: number | null;
  /** 要求パス (完了時の履歴保存に使う)。 */
  path: string;
  /** ステータス文言 ("" は非表示)。 */
  progressText: string;
  /** 進捗バー比率 (0..1)。 */
  ratio: number;
  /** 進捗バー行を表示するか。 */
  barVisible: boolean;
}

function freshDownloadState(): DownloadState {
  return { id: null, path: "", progressText: "", ratio: 0, barVisible: false };
}

const downloadStates = new Map<string, DownloadState>();

function stateFor(sessionId: string): DownloadState {
  let st = downloadStates.get(sessionId);
  if (!st) {
    st = freshDownloadState();
    downloadStates.set(sessionId, st);
  }
  return st;
}

// 現在パネルに表示中 (= アクティブタブ) のセッション id。イベントは全セッションから届くが
// DOM 更新はこのセッションのものだけに限定する。
let currentSessionId: string | null = null;

function renderPathHistory(): void {
  if (!pathListEl) return;
  pathListEl.replaceChildren(
    ...loadPathHistory().map((p) => {
      const opt = document.createElement("option");
      opt.value = p;
      return opt;
    }),
  );
}

export function mountFileTransfer(panel: HTMLElement): void {
  panel.innerHTML = `
    <div class="section-title">upload to host</div>
    <div class="row">
      <span class="hint">ファイルをキャプチャ画面にドラッグ&ドロップ</span>
    </div>
    <div class="row transfer-bar-row" id="upload-bar-row" hidden>
      <div class="transfer-bar"><div class="transfer-bar-fill" id="upload-bar"></div></div>
      <span id="upload-progress" class="progress"></span>
    </div>
    <div class="section-title">download from host</div>
    <div class="row">
      <input id="download-path" type="text" list="download-path-list"
             placeholder="例: C:\\Users\\name\\file.zip (ホスト上のパス)" />
      <datalist id="download-path-list"></datalist>
      <button type="button" id="download-file-btn">get</button>
      <span id="download-progress" class="progress"></span>
    </div>
    <div class="row transfer-bar-row" id="download-bar-row" hidden>
      <div class="transfer-bar"><div class="transfer-bar-fill" id="download-bar"></div></div>
    </div>
    <div class="row"><span class="hint">受信ファイルは %USERPROFILE%\\Aurora\\downloads に保存されます</span></div>
  `;

  downloadProgressEl = panel.querySelector<HTMLElement>("#download-progress");
  downloadBarEl = panel.querySelector<HTMLElement>("#download-bar");
  uploadProgressEl = panel.querySelector<HTMLElement>("#upload-progress");
  uploadBarEl = panel.querySelector<HTMLElement>("#upload-bar");
  pathListEl = panel.querySelector<HTMLDataListElement>("#download-path-list");
  downloadBtn = panel.querySelector<HTMLButtonElement>("#download-file-btn");
  const pathInput = panel.querySelector<HTMLInputElement>("#download-path");
  renderPathHistory();

  downloadBtn?.addEventListener("click", () => {
    // パネルは常にアクティブタブの状態を表示しているため、要求の所有者は「今表示中の
    // セッション」で確定する (ボタンは非アクティブタブでは操作できない)。
    const sessionId = currentSessionId;
    if (!sessionId) return;
    const path = pathInput?.value?.trim();
    if (!path) {
      setDownloadProgress("パスを入力");
      return;
    }
    const id = Date.now() >>> 0;
    const st = stateFor(sessionId);
    st.id = id;
    st.path = path;
    st.progressText = "要求中…";
    st.ratio = 0;
    st.barVisible = true;
    renderDownloadUi(sessionId);
    void requestFile(id, path).catch((err) => {
      st.id = null;
      st.barVisible = false;
      st.progressText = `エラー: ${String(err)}`;
      if (sessionId === currentSessionId) renderDownloadUi(sessionId);
    });
  });
}

/** 指定セッションのダウンロード状態を DOM (進捗バー/ボタン) へ反映する。 */
function renderDownloadUi(sessionId: string): void {
  const st = stateFor(sessionId);
  setDownloadProgress(st.progressText);
  if (st.barVisible) setBar("download", st.ratio);
  else hideBar("download");
  if (downloadBtn) downloadBtn.disabled = st.id !== null; // 進行中の再要求を防ぐ
}

/**
 * タブ切替時に orchestrator (capture-main) が呼ぶ: パネルの表示対象を切り替え先セッションへ
 * 揃え、その場でダウンロード状態 (進行中/完了/未実施) を再同期する。これが無いと、非アクティブ
 * 中に完了したダウンロードの結果がボタンの disabled 等に反映されず固着したままになる。
 */
export function setActiveTransferSession(sessionId: string | null): void {
  currentSessionId = sessionId;
  if (sessionId) {
    renderDownloadUi(sessionId);
  } else {
    setDownloadProgress("");
    hideBar("download");
    if (downloadBtn) downloadBtn.disabled = false;
  }
}

/** セッション終了/タブクローズ時に、そのセッションのダウンロード状態を破棄する (Map リーク防止)。 */
export function clearTransferSession(sessionId: string): void {
  downloadStates.delete(sessionId);
}

function setBar(kind: "download" | "upload", ratio: number): void {
  const row = document.getElementById(`${kind}-bar-row`);
  const fill = kind === "download" ? downloadBarEl : uploadBarEl;
  if (row) row.hidden = false;
  if (fill) fill.style.width = `${Math.round(Math.max(0, Math.min(1, ratio)) * 100)}%`;
}

function hideBar(kind: "download" | "upload"): void {
  const row = document.getElementById(`${kind}-bar-row`);
  if (row) row.hidden = true;
}

/**
 * host からの FileResult を反映する (ダウンロード)。アップロード (D&D) の結果は main.ts が
 * トーストで通知するため、ここではダウンロードの照合とアップロード進捗バーの後始末のみ行う。
 * イベントは非アクティブタブからも届くため、状態更新 (`sessionId` の Map エントリ) は常に行い、
 * DOM 描画だけを現在表示中のタブに限定する (取りこぼし防止)。
 */
export function handleFileResult(sessionId: string, id: number, ok: boolean, message: string): void {
  const st = downloadStates.get(sessionId);
  if (st && id === st.id) {
    st.progressText = ok ? `保存: ${message}` : `失敗: ${message}`;
    st.barVisible = false;
    if (ok && st.path) savePathHistory(st.path);
    st.id = null;
    st.path = "";
    if (sessionId === currentSessionId) {
      renderDownloadUi(sessionId);
      renderPathHistory();
    }
  } else {
    // ダウンロード以外の id はアップロード。完了/失敗でバーを畳む (表示中タブのみ)。
    if (sessionId === currentSessionId) {
      hideBar("upload");
      if (uploadProgressEl) uploadProgressEl.textContent = "";
    }
  }
}

/**
 * host からの FileProgress (done/total バイト) を進捗バーへ反映する。
 * FileResult と同様、状態更新は常に行い DOM 描画のみ表示中タブに限定する。
 */
export function handleFileProgress(sessionId: string, id: number, done: number, total: number): void {
  const ratio = total > 0 ? done / total : 0;
  const st = downloadStates.get(sessionId);
  if (st && id === st.id) {
    st.ratio = ratio;
    st.progressText = `受信中… ${Math.round(ratio * 100)}%`;
    st.barVisible = true;
    if (sessionId === currentSessionId) renderDownloadUi(sessionId);
  } else if (sessionId === currentSessionId) {
    setBar("upload", ratio);
    if (uploadProgressEl) uploadProgressEl.textContent = `${Math.round(ratio * 100)}%`;
  }
}

function setDownloadProgress(s: string): void {
  if (downloadProgressEl) downloadProgressEl.textContent = s;
}
