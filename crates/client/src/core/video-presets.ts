// 画質プリセット。fps 上限とビットレート上限 (Mbps) の組をワンタッチで選べる。
// 設定画面 (launcher) と接続中ツールバーの両方から共有して使う。手動で fps/Mbps を
// 変えたときは「カスタム」扱いにし、プリセットと実値の表示を一致させる。

export type VideoPreset = {
  id: string;
  /** UI 表示名。 */
  label: string;
  /** 送出 fps 上限 (0 = 無制限)。 */
  fps: number;
  /** ビットレート上限 (Mbps)。 */
  mbps: number;
};

/** 既定のプリセット一覧。標準が出荷時の既定値 (60fps / 15Mbps) と一致する。 */
export const VIDEO_PRESETS: readonly VideoPreset[] = [
  { id: "light", label: "軽量 (30fps / 5Mbps)", fps: 30, mbps: 5 },
  { id: "standard", label: "標準 (60fps / 15Mbps)", fps: 60, mbps: 15 },
  { id: "high", label: "高画質 (60fps / 30Mbps)", fps: 60, mbps: 30 },
];

/** プリセットに一致しない手動値を表す id。 */
export const CUSTOM_PRESET_ID = "custom";

/** fps/mbps からマッチするプリセット id を返す。一致なしは custom。 */
export function matchPreset(fps: number, mbps: number): string {
  const m = VIDEO_PRESETS.find((p) => p.fps === fps && p.mbps === mbps);
  return m ? m.id : CUSTOM_PRESET_ID;
}

/** プリセット id から定義を引く (custom や不明 id は undefined)。 */
export function findPreset(id: string): VideoPreset | undefined {
  return VIDEO_PRESETS.find((p) => p.id === id);
}

/** `<select>` に入れる option の HTML を返す (末尾にカスタムを足す)。 */
export function presetOptionsHtml(): string {
  const items = VIDEO_PRESETS.map((p) => `<option value="${p.id}">${p.label}</option>`);
  items.push(`<option value="${CUSTOM_PRESET_ID}">カスタム</option>`);
  return items.join("");
}
