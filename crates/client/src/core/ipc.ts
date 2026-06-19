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

/// password に null を渡すと、Rust 側が保存済みパスワード (DPAPI) を復号して使う
/// (平文をフロントに載せない)。手入力時は文字列を渡す。`save` が true の手入力時のみ
/// 接続成功後に Rust が DPAPI 保存する (「今回だけ接続」は save=false)。
export async function connectToHost(
  host: string,
  port: number,
  password: string | null,
  save: boolean,
  eventChannel: Channel<ClientEvent>,
  videoChannel: Channel<ArrayBuffer>,
): Promise<void> {
  await invoke("connect_to_host", { host, port, password, save, eventChannel, videoChannel });
}

/// パスワードを保存済みの host(小文字) 一覧。カードのマーキング/即接続判定に使う。
export async function savedPasswordHosts(): Promise<string[]> {
  return await invoke<string[]>("saved_password_hosts");
}

/// 指定 host の保存済みパスワードを削除する (「忘れる」)。
export async function deleteHostPassword(host: string): Promise<void> {
  await invoke("delete_host_password", { host });
}

export async function disconnect(): Promise<void> {
  await invoke("disconnect");
}

export async function sendInput(event: InputEvent): Promise<void> {
  await invoke("send_input", { event });
}

export async function requestKeyframe(): Promise<void> {
  await invoke("request_keyframe");
}

export async function selectDisplay(id: number): Promise<void> {
  await invoke("select_display", { id });
}

/// 映像パラメータ (fps 上限 / ビットレート上限 bps) を実行時にホストへ送る。
export async function setVideoParams(fps: number, maxBitrateBps: number): Promise<void> {
  await invoke("set_video_params", { fps, maxBitrateBps });
}

/// 音声ミュートの切替をホストへ送る (ミュート中はホストが音声を送らない)。
export async function setAudioMuted(muted: boolean): Promise<void> {
  await invoke("set_audio_muted", { muted });
}

/// 送出する映像の最大高さ (px、0 = キャプチャ解像度のまま) をホストへ送る。
/// ホストが GPU で縮小 (アスペクト比維持) してからエンコードする。
export async function setResolution(maxHeight: number): Promise<void> {
  await invoke("set_resolution", { maxHeight });
}

/// クライアントの映像表示領域サイズ (物理 px) をホストへ送る。ホストはこれに最も近い
/// 対応モードへ解像度を変更する (RDP 風・黒帯解消)。
export async function setClientViewport(width: number, height: number): Promise<void> {
  await invoke("set_client_viewport", { width, height });
}

export async function requestFile(id: number, path: string): Promise<void> {
  await invoke("request_file", { id, path });
}

/// ローカルのファイルパス (キャプチャ画面への D&D 由来) を host へアップロードする。
/// Rust 側がファイルを読み、既存のアップロードプロトコルで送る。
export async function uploadFileFromPath(id: number, path: string): Promise<void> {
  await invoke("upload_file_from_path", { id, path });
}

/// ホストの Explorer に投影する共有フォルダ (ProjFS ドライブリダイレクト) を設定する。
export async function setShareRoot(path: string): Promise<void> {
  await invoke("set_share_root", { path });
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
