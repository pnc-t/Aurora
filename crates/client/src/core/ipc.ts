// Tauri IPC ラッパー。Rust 側コマンドの薄い型付け窓口。
import { invoke, Channel } from "@tauri-apps/api/core";

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

/// アクティブタブのセッション id を設定する (タブ切替時に orchestrator が呼ぶ)。
export function setActiveSession(id: string | null): void {
  activeSessionId = id;
}

/// アクティブ session_id を返す。未設定での操作はバグなので例外にする。
function sid(): string {
  if (activeSessionId === null) throw new Error("no active session");
  return activeSessionId;
}

/// capture ウィンドウを Rust 側で開く (launcher から呼ぶ)。生成された session_id を返す。
/// password は IPC 本文で Rust の staging へ渡し、ウィンドウ URL には載せない。
export async function openCaptureWindow(
  host: string,
  port: number,
  save: boolean,
  password: string | null,
): Promise<string> {
  return await invoke<string>("open_capture_window", { host, port, save, password });
}

/// 既存 capture ウィンドウ内に新規タブ (セッション) を開く準備。session_id を発番し、
/// password を Rust の staging へ積む (open_capture_window と同じ受け渡し、ただし窓は作らない)。
/// 返った id で connectToHost を呼ぶ。
export async function stageSession(password: string | null): Promise<string> {
  return await invoke<string>("stage_session", { password });
}

/// セッションを開く。password はもう渡さない (Rust が staging / 保存済み DPAPI から解決する)。
/// 所有ウィンドウの紐付けは Rust 側が呼び出し webview から取得する。
export async function connectToHost(
  sessionId: string,
  host: string,
  port: number,
  save: boolean,
  eventChannel: Channel<ClientEvent>,
  videoChannel: Channel<ArrayBuffer>,
): Promise<void> {
  await invoke("connect_to_host", { sessionId, host, port, save, eventChannel, videoChannel });
}

/// 既存セッションを別ウィンドウへ移譲するための空ウィンドウを開く (タブのドラッグ分割)。
/// `snapshot` は移譲先が復元に使う表示状態 JSON。`x`/`y` は新ウィンドウの配置 (カーソル位置)。
/// networking は止まらず、Rust 側が表示先 (sink) を即デタッチして所有ウィンドウを付け替える。
export async function openSessionWindow(
  sessionId: string,
  snapshot: string,
  x: number,
  y: number,
): Promise<void> {
  await invoke("open_session_window", { sessionId, snapshot, x, y });
}

/// 移譲先ウィンドウが引き継ぎデータ (表示状態 JSON) を取り出す (1 回限り)。
export async function takeHandoff(sessionId: string): Promise<string | null> {
  return await invoke<string | null>("take_handoff", { sessionId });
}

/// 既存セッションの表示先を、この webview の Channel へ差し替える (移譲先がアタッチ)。
/// networking はそのままなので、別途 requestKeyframe で映像を復帰させる。
export async function attachSession(
  sessionId: string,
  eventChannel: Channel<ClientEvent>,
  videoChannel: Channel<ArrayBuffer>,
): Promise<void> {
  await invoke("attach_session", { sessionId, eventChannel, videoChannel });
}

/// 現在接続中の host 一覧 (重複排除)。ランチャーの接続中表示・切断ボタンの初期化に使う。
/// 以降の更新は `aurora://connections` イベントで届く。
export async function connectedHosts(): Promise<string[]> {
  return await invoke<string[]>("connected_hosts");
}

/// パスワードを保存済みの host(小文字) 一覧。カードのマーキング/即接続判定に使う。
export async function savedPasswordHosts(): Promise<string[]> {
  return await invoke<string[]>("saved_password_hosts");
}

/// 指定 host の保存済みパスワードを削除する (「忘れる」)。
export async function deleteHostPassword(host: string): Promise<void> {
  await invoke("delete_host_password", { host });
}

export async function disconnect(sessionId: string): Promise<void> {
  await invoke("disconnect", { sessionId });
}

/// 指定 host の全セッションを Rust 側で停止する (ランチャーの切断ボタン)。窓が閉じた後に
/// 残った孤児セッションも host 名で確実に止められる (`aurora://disconnect-host` の補完)。
export async function disconnectHost(host: string): Promise<void> {
  await invoke("disconnect_host", { host });
}

export async function sendInput(sessionId: string, event: InputEvent): Promise<void> {
  await invoke("send_input", { sessionId, event });
}

export async function requestKeyframe(sessionId: string): Promise<void> {
  await invoke("request_keyframe", { sessionId });
}

export async function selectDisplay(sessionId: string, id: number): Promise<void> {
  await invoke("select_display", { sessionId, id });
}

/// 映像パラメータ (fps 上限 / ビットレート上限 bps) を実行時にホストへ送る。
export async function setVideoParams(
  sessionId: string,
  fps: number,
  maxBitrateBps: number,
): Promise<void> {
  await invoke("set_video_params", { sessionId, fps, maxBitrateBps });
}

/// 音声ミュートの切替をホストへ送る (ミュート中はホストが音声を送らない)。
export async function setAudioMuted(sessionId: string, muted: boolean): Promise<void> {
  await invoke("set_audio_muted", { sessionId, muted });
}

/// 映像送出の一時停止 / 再開をホストへ送る (非アクティブタブの帯域を止める)。
/// 再開時は別途 requestKeyframe で映像を復帰させる。
export async function setVideoPaused(sessionId: string, paused: boolean): Promise<void> {
  await invoke("set_video_paused", { sessionId, paused });
}

/// 送出する映像の最大高さ (px、0 = キャプチャ解像度のまま) をホストへ送る。
/// ホストが GPU で縮小 (アスペクト比維持) してからエンコードする。
export async function setResolution(sessionId: string, maxHeight: number): Promise<void> {
  await invoke("set_resolution", { sessionId, maxHeight });
}

/// クライアントの映像表示領域サイズ (物理 px) をホストへ送る。ホストはこれに最も近い
/// 対応モードへ解像度を変更する (RDP 風・黒帯解消)。
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

/// ローカルのファイルパス (キャプチャ画面への D&D 由来) を host へアップロードする。
/// Rust 側がファイルを読み、既存のアップロードプロトコルで送る。
export async function uploadFileFromPath(id: number, path: string): Promise<void> {
  await invoke("upload_file_from_path", { sessionId: sid(), id, path });
}

/// ホストの Explorer に投影する共有フォルダ (ProjFS ドライブリダイレクト) を設定する。
export async function setShareRoot(path: string): Promise<void> {
  await invoke("set_share_root", { sessionId: sid(), path });
}

/// 保存する 1 件の接続先 (Rust 側 config::HostEntry と対応)。
export type HostEntry = {
  label: string;
  host: string;
  port: number;
};

/// 配色テーマ。auto は OS の prefers-color-scheme に追従、legacy はグレーのクラシック調。
export type ThemeId = "auto" | "dark" | "light" | "legacy";

/// ツールバーの配置。left/right では縦並びのサイドバーになる。
export type ToolbarPosition = "top" | "bottom" | "left" | "right";

/// クライアント設定 (Rust 側 config::ClientSettings と対応)。
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
};

/// 接続先 host の履歴を読み込む (最新が先頭)。
export async function loadRecentHosts(): Promise<HostEntry[]> {
  return await invoke<HostEntry[]>("load_recent_hosts");
}

/// 接続成功した host を履歴へ保存する (host で重複排除・更新)。
export async function saveRecentHost(entry: HostEntry): Promise<void> {
  await invoke("save_recent_host", { entry });
}

/// 指定 host を履歴から削除する。
export async function deleteRecentHost(host: string): Promise<void> {
  await invoke("delete_recent_host", { host });
}

/// 履歴の並び順を丸ごと置き換える (ドラッグ並べ替え後)。
export async function setRecentHosts(hosts: HostEntry[]): Promise<void> {
  await invoke("set_recent_hosts", { hosts });
}

/// クライアント設定を読み込む。
export async function loadSettings(): Promise<ClientSettings> {
  return await invoke<ClientSettings>("load_settings");
}

/// クライアント設定を保存する。
export async function saveSettings(settings: ClientSettings): Promise<void> {
  await invoke("save_settings", { settings });
}
