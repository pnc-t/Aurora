// Tauri IPC ラッパー。Rust 側コマンドの薄い型付け窓口。
import { invoke, Channel } from "@tauri-apps/api/core";

export type DisplayDto = {
  id: number;
  width: number;
  height: number;
  name: string;
};

export type ClientEvent =
  | { kind: "connected" }
  | { kind: "host_info"; displays: DisplayDto[]; audio: boolean }
  | { kind: "pong"; rtt_us: number }
  | { kind: "stats"; bitrate_kbps: number; fps: number; rtt_us: number; loss_permille: number }
  | { kind: "file_result"; id: number; ok: boolean; message: string }
  | { kind: "disconnected"; reason: string };

export type InputEvent =
  | { MouseMove: { x: number; y: number } }
  | { MouseButton: { button: "Left" | "Right" | "Middle"; down: boolean } }
  | { MouseWheel: { delta: number; horizontal: boolean } }
  | { Key: { vk: number; down: boolean } };

export async function connectToHost(
  host: string,
  password: string,
  eventChannel: Channel<ClientEvent>,
  videoChannel: Channel<ArrayBuffer>,
): Promise<void> {
  await invoke("connect_to_host", { host, password, eventChannel, videoChannel });
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

export async function sendFileOffer(id: number, name: string, size: number): Promise<void> {
  await invoke("send_file_offer", { id, name, size });
}

export async function sendFileChunk(
  id: number,
  offset: number,
  data: number[],
  last: boolean,
): Promise<void> {
  await invoke("send_file_chunk", { id, offset, data, last });
}

export async function requestFile(id: number, path: string): Promise<void> {
  await invoke("request_file", { id, path });
}

/// ホストの Explorer に投影する共有フォルダ (ProjFS ドライブリダイレクト) を設定する。
export async function setShareRoot(path: string): Promise<void> {
  await invoke("set_share_root", { path });
}

/// 接続先 host の履歴を読み込む (最新が先頭)。
export async function loadRecentHosts(): Promise<string[]> {
  return await invoke<string[]>("load_recent_hosts");
}

/// 接続成功した host を履歴へ保存する。
export async function saveRecentHost(host: string): Promise<void> {
  await invoke("save_recent_host", { host });
}
