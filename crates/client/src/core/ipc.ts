// Tauri IPC ラッパー。Rust 側コマンドの薄い型付け窓口。
import { invoke, Channel } from "@tauri-apps/api/core";
import type { CodecChoice, CodecId } from "./codec";

export type DisplayDto = {
  id: number;
  width: number;
  height: number;
  name: string;
  is_virtual: boolean;
};

/**
 * ホストの生デバイス名 (Windows DXGI の `\\.\DISPLAY1` 等) を表示用に整形する。
 * 先頭の `\\.\` / `\\?\` を除き、`DISPLAY1` は `Display 1` に直す。
 * (バックスラッシュは日本語フォントで ¥ に化けるため UI には出さない)
 */
export function formatDisplayName(raw: string): string {
  const stripped = raw.replace(/^\\\\[.?]\\/, "");
  const m = /^DISPLAY(\d+)$/i.exec(stripped);
  return m ? `Display ${m[1]}` : stripped;
}

export type ClientEvent =
  | { kind: "connected" }
  | { kind: "host_info"; displays: DisplayDto[]; audio: boolean }
  | { kind: "display_resized"; id: number; width: number; height: number }
  | { kind: "pong"; rtt_us: number }
  | { kind: "stats"; bitrate_kbps: number; fps: number; rtt_us: number; loss_permille: number }
  | { kind: "file_result"; id: number; ok: boolean; message: string }
  | { kind: "file_progress"; id: number; done: number; total: number }
  | { kind: "video_codec_changed"; codec: CodecId }
  | { kind: "disconnected"; reason: string };

export type InputEvent =
  | { MouseMove: { x: number; y: number } }
  | { MouseButton: { button: "Left" | "Right" | "Middle"; down: boolean } }
  | { MouseWheel: { delta: number; horizontal: boolean } }
  | { Key: { vk: number; down: boolean } }
  | { MouseMoveRelative: { dx: number; dy: number } };

// --- アクティブセッション id (共有 UI 専用) ---
// capture ウィンドウは複数タブ (セッション) を持ちうる。映像/入力/再接続などセッション固有の
// 操作は `Session` が**明示的に session_id を渡して**呼ぶ (下の各コマンドの第 1 引数)。
// 一方、画面に 1 つしかない共有パネル (file-transfer / D&D アップロード) はアクティブタブを
// 対象にするため、ここで保持するアクティブ id を暗黙に使う。タブ切替で orchestrator が更新する。
let activeSessionId: string | null = null;

/** アクティブタブのセッション id を設定する (タブ切替時に orchestrator が呼ぶ)。 */
export function setActiveSession(id: string | null): void {
  activeSessionId = id;
}

/** アクティブ session_id を返す。未設定での操作はバグなので例外にする。 */
function sid(): string {
  if (activeSessionId === null) throw new Error("no active session");
  return activeSessionId;
}

/**
 * capture ウィンドウを Rust 側で開く (launcher から呼ぶ)。生成された session_id を返す。
 * password は IPC 本文で Rust の staging へ渡し、ウィンドウ URL には載せない。
 * newWindow=true は既存窓へのタブ合流をせず常に新しいウィンドウを開く。
 */
export async function openCaptureWindow(
  host: string,
  port: number,
  label: string,
  save: boolean,
  password: string | null,
  newWindow = false,
): Promise<string> {
  return await invoke<string>("open_capture_window", {
    host,
    port,
    label,
    save,
    password,
    newWindow,
  });
}

/**
 * 既存 capture ウィンドウ内に新規タブ (セッション) を開く準備。session_id を発番し、
 * password を Rust の staging へ積む (open_capture_window と同じ受け渡し、ただし窓は作らない)。
 * 返った id で connectToHost を呼ぶ。
 */
export async function stageSession(password: string | null): Promise<string> {
  return await invoke<string>("stage_session", { password });
}

/**
 * セッションを開く。password はもう渡さない (Rust が staging / 保存済み DPAPI から解決する)。
 * 所有ウィンドウの紐付けは Rust 側が呼び出し webview から取得する。
 */
export async function connectToHost(
  sessionId: string,
  host: string,
  port: number,
  save: boolean,
  eventChannel: Channel<ClientEvent>,
  videoChannel: Channel<ArrayBuffer>,
  supportedCodecs: CodecId[],
): Promise<void> {
  await invoke("connect_to_host", {
    sessionId,
    host,
    port,
    save,
    eventChannel,
    videoChannel,
    supportedCodecs,
  });
}

/**
 * 接続中に申告コーデック集合を変更する (設定変更時の手動切替)。ホストがエンコーダを
 * 作り直し、`video_codec_changed` イベントで実際に切り替わったコーデックが届く。
 */
export async function setSupportedCodecs(sessionId: string, codecs: CodecId[]): Promise<void> {
  await invoke("set_supported_codecs", { sessionId, codecs });
}

/**
 * 既存セッションを別ウィンドウへ移譲するための空ウィンドウを開く (タブのドラッグ分割)。
 * `snapshot` は移譲先が復元に使う表示状態 JSON。`x`/`y` は新ウィンドウの配置 (カーソル位置)。
 * networking は止まらず、Rust 側が表示先 (sink) を即デタッチして所有ウィンドウを付け替える。
 */
export async function openSessionWindow(
  sessionId: string,
  snapshot: string,
  x: number,
  y: number,
): Promise<void> {
  await invoke("open_session_window", { sessionId, snapshot, x, y });
}

/** 移譲先ウィンドウが引き継ぎデータ (表示状態 JSON) を取り出す (1 回限り)。 */
export async function takeHandoff(sessionId: string): Promise<string | null> {
  return await invoke<string | null>("take_handoff", { sessionId });
}

/** staging へ積んだ未使用の接続資格情報を破棄する (同一 host 重複でタブを作らなかったとき)。 */
export async function unstageSession(sessionId: string): Promise<void> {
  await invoke("unstage_session", { sessionId });
}

/** ドロップ点 (論理 px スクリーン座標) にある自分以外の capture 窓のラベル (無ければ null)。 */
export async function findCaptureWindowAt(x: number, y: number): Promise<string | null> {
  return await invoke<string | null>("find_capture_window_at", { x, y });
}

/** 既存セッションを既存の別 capture 窓へ移譲する (タブのドラッグ合流)。 */
export async function handoffSessionToWindow(
  sessionId: string,
  targetLabel: string,
  snapshot: string,
): Promise<void> {
  await invoke("handoff_session_to_window", { sessionId, targetLabel, snapshot });
}

/**
 * 既存セッションの表示先を、この webview の Channel へ差し替える (移譲先がアタッチ)。
 * networking はそのままなので、別途 requestKeyframe で映像を復帰させる。
 */
export async function attachSession(
  sessionId: string,
  eventChannel: Channel<ClientEvent>,
  videoChannel: Channel<ArrayBuffer>,
): Promise<void> {
  await invoke("attach_session", { sessionId, eventChannel, videoChannel });
}

/**
 * 現在接続中の host 一覧 (重複排除)。ランチャーの接続中表示・切断ボタンの初期化に使う。
 * 以降の更新は `aurora://connections` イベントで届く。
 */
export async function connectedHosts(): Promise<string[]> {
  return await invoke<string[]>("connected_hosts");
}

/** 指定 host のセッションを表示中の capture 窓を前面化+タブ活性化する。見つからなければ false。 */
export async function focusHostWindow(host: string): Promise<boolean> {
  return await invoke<boolean>("focus_host_window", { host });
}

/** ホスト到達性プローブの 1 件分の結果。 */
export type ProbeResult = { host: string; online: boolean };

/** 各ホストの制御ポートへ TCP 到達性を並列プローブする (launcher のオンライン表示用)。 */
export async function probeHosts(
  targets: { host: string; port: number }[],
): Promise<ProbeResult[]> {
  return await invoke<ProbeResult[]>("probe_hosts", { targets });
}

/** パスワードを保存済みの host(小文字) 一覧。カードのマーキング/即接続判定に使う。 */
export async function savedPasswordHosts(): Promise<string[]> {
  return await invoke<string[]>("saved_password_hosts");
}

/** 指定 host の保存済みパスワードを削除する (「忘れる」)。 */
export async function deleteHostPassword(host: string): Promise<void> {
  await invoke("delete_host_password", { host });
}

/** この端末の識別子 (フィンガープリント)。host のデバイスピン留め承認時に照合する。 */
export async function myDeviceFingerprint(): Promise<string> {
  return await invoke<string>("my_device_fingerprint");
}

export async function disconnect(sessionId: string): Promise<void> {
  await invoke("disconnect", { sessionId });
}

/**
 * 指定 host の全セッションを Rust 側で停止する (ランチャーの切断ボタン)。窓が閉じた後に
 * 残った孤児セッションも host 名で確実に止められる (`aurora://disconnect-host` の補完)。
 */
export async function disconnectHost(host: string): Promise<void> {
  await invoke("disconnect_host", { host });
}

export async function sendInput(sessionId: string, event: InputEvent): Promise<void> {
  await invoke("send_input", { sessionId, event });
}

export async function requestKeyframe(sessionId: string): Promise<void> {
  await invoke("request_keyframe", { sessionId });
}

/**
 * キーボード占有 (WH_KEYBOARD_LL) の ON/OFF。マウスキャプチャ連動で、ON 中は Win キー / Alt+Tab
 * 等がクライアント OS に奪われずホストへ転送される (`grabbed-key` イベントで届く)。
 */
export async function setKeyboardGrab(on: boolean): Promise<void> {
  await invoke("set_keyboard_grab", { on });
}

// --- 計測フラグ (ホスト --perf 相当) ---
// 環境変数 AURORA_PERF で有効化されるクライアント側の計測スイッチ。起動時に 1 回 Rust へ
// 問い合わせて保持し、decoder の vstat (受信→提示の遅延) 集計を有効化する。未取得の間は
// false。同期 getter にして decoder のホットパスから安価に参照できるようにする。
let perfFlag = false;

export async function initPerfFlag(): Promise<void> {
  try {
    perfFlag = await invoke<boolean>("perf_enabled");
  } catch {
    perfFlag = false;
  }
}

export function isPerfEnabled(): boolean {
  return perfFlag;
}

/** ログフォルダ (%LOCALAPPDATA%\Aurora\logs) をエクスプローラで開く。 */
export async function openLogsFolder(): Promise<void> {
  await invoke("open_logs_folder");
}

/** ログ一式を ZIP にまとめて書き出し、生成先パスを返す。 */
export async function exportLogsZip(): Promise<string> {
  return await invoke("export_logs_zip");
}

export async function selectDisplay(sessionId: string, id: number): Promise<void> {
  await invoke("select_display", { sessionId, id });
}

/** 映像パラメータ (fps 上限 / ビットレート上限 bps) を実行時にホストへ送る。 */
export async function setVideoParams(
  sessionId: string,
  fps: number,
  maxBitrateBps: number,
): Promise<void> {
  await invoke("set_video_params", { sessionId, fps, maxBitrateBps });
}

/** 音声ミュートの切替をホストへ送る (ミュート中はホストが音声を送らない)。 */
export async function setAudioMuted(sessionId: string, muted: boolean): Promise<void> {
  await invoke("set_audio_muted", { sessionId, muted });
}

/** クリップボード同期のセッション毎トグル (OFF 中は送受信とも一時停止)。 */
export async function setClipboardSync(sessionId: string, enabled: boolean): Promise<void> {
  await invoke("set_clipboard_sync", { sessionId, enabled });
}

/**
 * 映像送出の一時停止 / 再開をホストへ送る (非アクティブタブの帯域を止める)。
 * 再開時は別途 requestKeyframe で映像を復帰させる。
 */
export async function setVideoPaused(sessionId: string, paused: boolean): Promise<void> {
  await invoke("set_video_paused", { sessionId, paused });
}

/**
 * 映像へのカーソル合成 ON/OFF をホストへ送る (マウスキャプチャ中だけ ON)。
 * DXGI はカーソルを映像に含めないため、キャプチャ中はホストが焼き込んだカーソルを表示する。
 */
export async function setCursorOverlay(sessionId: string, enabled: boolean): Promise<void> {
  await invoke("set_cursor_overlay", { sessionId, enabled });
}

/**
 * 送出する映像の最大高さ (px、0 = キャプチャ解像度のまま) をホストへ送る。
 * ホストが GPU で縮小 (アスペクト比維持) してからエンコードする。
 */
export async function setResolution(sessionId: string, maxHeight: number): Promise<void> {
  await invoke("set_resolution", { sessionId, maxHeight });
}

/**
 * クライアントの映像表示領域サイズ (物理 px) をホストへ送る。ホストはこれに最も近い
 * 対応モードへ解像度を変更する (RDP 風・黒帯解消)。
 */
export async function setClientViewport(
  sessionId: string,
  width: number,
  height: number,
): Promise<void> {
  await invoke("set_client_viewport", { sessionId, width, height });
}

export async function requestFile(id: number, path: string): Promise<void> {
  await invoke("request_file", { sessionId: sid(), id, path });
}

/**
 * ローカルのファイルパス (キャプチャ画面への D&D 由来) を host へアップロードする。
 * Rust 側がファイルを読み、既存のアップロードプロトコルで送る。
 */
export async function uploadFileFromPath(id: number, path: string): Promise<void> {
  await invoke("upload_file_from_path", { sessionId: sid(), id, path });
}

/**
 * 指定セッションのファイル共有 (ProjFS ドライブ投影) を有効/無効にする。有効時は
 * **このPC (client) の This PC 全体 (全ドライブ)** をホストの仮想ドライブへ公開する。
 * 設定は接続成立時に session が自分の id で適用する (登録端末のみホストにドライブが出る)。
 */
export async function setFileSharing(sessionId: string, enabled: boolean): Promise<void> {
  await invoke("set_file_sharing", { sessionId, enabled });
}

/** ホスト別のクライアント設定上書き。未指定フィールドはグローバル設定に従う (Rust 側 config::HostOverrides と対応)。 */
export type HostOverrides = {
  use_virtual_display?: boolean | null;
  fps?: number | null;
  max_mbps?: number | null;
  /** ホスト別の映像コーデック上書き (未指定/null はグローバル設定に従う)。 */
  codec?: CodecChoice | null;
};

/** 保存する 1 件の接続先 (Rust 側 config::HostEntry と対応)。 */
export type HostEntry = {
  label: string;
  host: string;
  port: number;
  /** カードのアバター画像 (data URL、空文字は明示解除)。無指定 (null/undefined) は「情報なし」で
   *  Rust 側が保存済みの既存値を保持する (接続成功時の再保存でアバターが消えないため)。 */
  icon?: string | null;
  /** ホスト別の設定上書き (無ければグローバル設定に従う)。任意。 */
  overrides?: HostOverrides | null;
  /** Wake on LAN 用 MAC アドレス (接続時に Rust が自動保存する)。icon と同じ
   *  None(null/undefined)=「情報なし」で既存値を保持、空文字=明示解除の規約。 */
  mac?: string | null;
  /** 最後に接続成功した日時 (unix 秒)。icon/mac と同じ「未指定=情報なし・既存値保持」の規約。 */
  last_connected?: number;
};

/** WoL 中継先ターゲット (Rust 側 WolTarget と対応)。 */
export type WolTarget = {
  id: string;
  label: string;
  mac: string;
  relay_host: string;
  relay_port: number;
  host?: string | null;
  port?: number | null;
  icon?: string | null;
};

/** 保存済み WoL ターゲット一覧を読み込む。 */
export async function listWolTargets(): Promise<WolTarget[]> {
  return await invoke<WolTarget[]>("list_wol_targets");
}

/** WoL ターゲットを保存する (id で重複排除・更新)。 */
export async function saveWolTarget(target: WolTarget): Promise<void> {
  await invoke("save_wol_target", { target });
}

/** WoL ターゲット一覧を丸ごと保存する (並べ替えの永続化用)。 */
export async function setWolTargets(targets: WolTarget[]): Promise<void> {
  await invoke("set_wol_targets", { targets });
}

/** 指定 id の WoL ターゲットを削除する。 */
export async function deleteWolTarget(id: string): Promise<void> {
  await invoke("delete_wol_target", { id });
}

/**
 * raspi 中継経由でマジックパケットを送る。password=null は中継先の保存済みパスワード
 * (DPAPI) を使う。savePassword=true で入力した password を新たに DPAPI 保存する。
 */
export async function wakeHost(
  relayHost: string,
  relayPort: number,
  password: string | null,
  savePassword: boolean,
  mac: string,
): Promise<void> {
  await invoke("wake_host", { relayHost, relayPort, password, savePassword, mac });
}

/** 配色テーマ。auto は OS の prefers-color-scheme に追従、legacy はグレーのクラシック調。 */
export type ThemeId = "auto" | "dark" | "light" | "legacy";

/** ツールバーの配置。left/right では縦並びのサイドバーになる。 */
export type ToolbarPosition = "top" | "bottom" | "left" | "right";

/** 統計 HUD の表示位置 (画面の隅)。 */
export type StatsPosition = "tl" | "tr" | "bl" | "br";

/** クライアント設定 (Rust 側 config::ClientSettings と対応)。 */
export type ClientSettings = {
  default_fullscreen: boolean;
  show_stats: boolean;
  default_capture: boolean;
  default_port: number;
  toolbar_pinned: boolean;
  toolbar_position: ToolbarPosition;
  tab_position: ToolbarPosition;
  accent_color: string;
  theme: ThemeId;
  fps: number;
  max_mbps: number;
  auto_reconnect: boolean;
  use_virtual_display: boolean;
  share_files: boolean;
  /** 映像コーデック ("auto" はプローブで対応する全コーデックを申告する)。 */
  codec: CodecChoice;
  /** 接続時に音声をミュートで開始する。 */
  audio_muted: boolean;
  /** マウスキャプチャ中にキーボード占有 (WH_KEYBOARD_LL) を有効にするか。 */
  grab_keyboard: boolean;
  /** 統計 HUD の表示位置。 */
  stats_position: StatsPosition;
  /** ローカル UI ショートカットのユーザー割当 (LocalHotkey id → "Ctrl+Alt+KeyF" 形式)。
   *  未登録の id は既定バインディングを使う。 */
  shortcuts: Record<string, string>;
  /** クリップボード同期を行うか。接続時のセッションゲート初期値になる。 */
  clipboard_sync: boolean;
};

/** ClientSettings の既定値。設定未保存時のフォールバックや capture 窓の未読込中フォールバックで使う
 *  (host/client 双方に散らばっていた複製を一元化。§A-12)。 */
export const DEFAULT_SETTINGS: ClientSettings = {
  default_fullscreen: false,
  show_stats: false,
  default_capture: false,
  default_port: 7000,
  toolbar_pinned: false,
  toolbar_position: "top",
  tab_position: "top",
  accent_color: "#3a82f7",
  theme: "dark",
  fps: 60,
  max_mbps: 8,
  auto_reconnect: true,
  use_virtual_display: false,
  share_files: false,
  codec: "auto",
  audio_muted: false,
  grab_keyboard: true,
  stats_position: "tr",
  shortcuts: {},
  clipboard_sync: true,
};

/** 接続先 host の履歴を読み込む (最新が先頭)。 */
export async function loadRecentHosts(): Promise<HostEntry[]> {
  return await invoke<HostEntry[]>("load_recent_hosts");
}

/** 接続成功した host を履歴へ保存する (host で重複排除・更新)。 */
export async function saveRecentHost(entry: HostEntry): Promise<void> {
  await invoke("save_recent_host", { entry });
}

/** 指定 host を履歴から削除する。 */
export async function deleteRecentHost(host: string): Promise<void> {
  await invoke("delete_recent_host", { host });
}

/** 履歴の並び順を丸ごと置き換える (ドラッグ並べ替え後)。 */
export async function setRecentHosts(hosts: HostEntry[]): Promise<void> {
  await invoke("set_recent_hosts", { hosts });
}

/** クライアント設定を読み込む。 */
export async function loadSettings(): Promise<ClientSettings> {
  return await invoke<ClientSettings>("load_settings");
}

/** クライアント設定を保存する。 */
export async function saveSettings(settings: ClientSettings): Promise<void> {
  await invoke("save_settings", { settings });
}
