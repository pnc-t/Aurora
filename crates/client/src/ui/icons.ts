// アプリ内アイコンの単一ソース。
// ナビ (Hosts/Settings)、操作 (追加/再読込/編集/削除/chevron)、ウィンドウ操作
// (最小化/最大化/閉じる) の全アイコンをここに集約する。各所はここを参照する。
//
// 2 系統をサポートする:
//   1. 組み込み SVG (既定)。currentColor を使う 1em の SVG で、色は CSS の `color`、
//      大きさは `font-size` で制御 (テーマのアクセント色に追従)。
//   2. 画像ファイル (png/jpg/jpeg/webp/svg/gif)。`src/assets/icons/<name>.<ext>` に
//      置くと同名の組み込み SVG より **優先** される。差し替えたいアイコンだけ
//      ファイルを置けばよい (例: assets/icons/hosts.png)。
// アイコンを増減・差し替えるときはこのモジュール (と assets/icons/) だけ触ればよい。

export type IconName =
  | "hosts"
  | "settings"
  | "add"
  | "edit"
  | "delete"
  | "more"
  | "up"
  | "down"
  | "grip"
  | "chevron"
  | "minimize"
  | "close"
  | "power"
  | "fullscreen"
  | "mouse"
  | "mouse-left"
  | "mouse-middle"
  | "mouse-right"
  | "modifier"
  | "files"
  | "stats"
  | "display"
  | "pin"
  | "volume"
  | "mute"
  | "eye-off"
  | "eye"
  | "refresh"
  | "tune"
  | "resolution"
  | "key"
  | "check"
  | "bolt"
  | "clipboard"
  | "touchpad"
  | "stick";

// viewBox 0 0 24 24 の path/shape 部分のみ (共通の <svg> ラッパーは icon() で付与)。
const PATHS: Record<IconName, string> = {
  hosts: `<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>`,
  settings: `<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>`,
  add: `<path d="M12 5v14M5 12h14"/>`,
  edit: `<path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/>`,
  delete: `<path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>`,
  more: `<circle cx="5" cy="12" r="1.6" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1.6" fill="currentColor" stroke="none"/><circle cx="19" cy="12" r="1.6" fill="currentColor" stroke="none"/>`,
  up: `<path d="M18 15l-6-6-6 6"/>`,
  down: `<path d="M6 9l6 6 6-6"/>`,
  grip: `<circle cx="9" cy="6" r="1.5" fill="currentColor" stroke="none"/><circle cx="15" cy="6" r="1.5" fill="currentColor" stroke="none"/><circle cx="9" cy="12" r="1.5" fill="currentColor" stroke="none"/><circle cx="15" cy="12" r="1.5" fill="currentColor" stroke="none"/><circle cx="9" cy="18" r="1.5" fill="currentColor" stroke="none"/><circle cx="15" cy="18" r="1.5" fill="currentColor" stroke="none"/>`,
  chevron: `<path d="M9 18l6-6-6-6"/>`,
  minimize: `<path d="M5 12h14"/>`,
  close: `<path d="M18 6L6 18M6 6l12 12"/>`,
  power: `<path d="M12 2v9"/><path d="M18.4 6.6a9 9 0 1 1-12.8 0"/>`,
  fullscreen: `<path d="M8 3H5a2 2 0 0 0-2 2v3M21 8V5a2 2 0 0 0-2-2h-3M3 16v3a2 2 0 0 0 2 2h3M16 21h3a2 2 0 0 0 2-2v-3"/>`,
  mouse: `<rect x="6" y="3" width="12" height="18" rx="6"/><path d="M12 7v4"/>`,
  "mouse-left": `<rect x="6" y="3" width="12" height="18" rx="6"/><path d="M12 7v4"/><path d="M12 3a6 6 0 0 0-6 6h6z" fill="currentColor" stroke="none"/>`,
  "mouse-middle": `<rect x="6" y="3" width="12" height="18" rx="6"/><rect x="10.8" y="6" width="2.4" height="5" rx="1.2" fill="currentColor" stroke="none"/>`,
  "mouse-right": `<rect x="6" y="3" width="12" height="18" rx="6"/><path d="M12 7v4"/><path d="M12 3a6 6 0 0 1 6 6h-6z" fill="currentColor" stroke="none"/>`,
  modifier: `<rect x="3" y="6" width="18" height="12" rx="2"/><path d="M8 14l4-4 4 4"/>`,
  files: `<path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><path d="M14 3v6h6"/>`,
  stats: `<path d="M3 3v18h18"/><path d="M19 9l-5 5-3-3-4 4"/>`,
  display: `<rect x="2" y="4" width="20" height="13" rx="2"/><path d="M8 21h8M12 17v4"/>`,
  pin: `<path d="M12 17v5"/><path d="M9 3h6l-1 6 3 3v2H7v-2l3-3-1-6z"/>`,
  volume: `<path d="M11 5 6 9H2v6h4l5 4z"/><path d="M15.5 8.5a5 5 0 0 1 0 7M19 5a9 9 0 0 1 0 14"/>`,
  mute: `<path d="M11 5 6 9H2v6h4l5 4z"/><path d="M22 9l-6 6M16 9l6 6"/>`,
  "eye-off": `<path d="M9.9 4.2A9.1 9.1 0 0 1 12 4c5 0 9 5 9 8a11 11 0 0 1-2.3 3.3M6.6 6.6A11.2 11.2 0 0 0 3 12c0 3 4 8 9 8a9 9 0 0 0 4.3-1.1M9.9 9.9a3 3 0 0 0 4.2 4.2M3 3l18 18"/>`,
  eye: `<path d="M2 12s4-7.5 10-7.5S22 12 22 12s-4 7.5-10 7.5S2 12 2 12z"/><circle cx="12" cy="12" r="3"/>`,
  refresh: `<path d="M22 4v6h-6"/><path d="M19.5 15a8.5 8.5 0 1 1-2-8.9L22 10"/>`,
  tune: `<path d="M4 15a8 8 0 0 1 16 0"/><path d="M12 15l4-4.5"/><circle cx="12" cy="15" r="1.3" fill="currentColor" stroke="none"/>`,
  resolution: `<rect x="4" y="4" width="6" height="6" rx="1"/><rect x="14" y="4" width="6" height="6" rx="1"/><rect x="4" y="14" width="6" height="6" rx="1"/><rect x="14" y="14" width="6" height="6" rx="1"/>`,
  key: `<circle cx="7.5" cy="15.5" r="4.5"/><path d="M10.7 12.3 21 2M16 7l3 3"/>`,
  check: `<path d="M20 6L9 17l-5-5"/>`,
  bolt: `<path d="M13 2 3 14h7l-1 8 10-12h-7l1-8z"/>`,
  clipboard: `<rect x="8" y="2" width="8" height="4" rx="1"/><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/>`,
  touchpad: `<rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="1.6" fill="currentColor" stroke="none"/>`,
  stick: `<circle cx="12" cy="12" r="8.5"/><circle cx="12" cy="12" r="3.2" fill="currentColor" stroke="none"/>`,
};

// assets/icons/ 配下の画像を URL として一括取り込み (ビルド時に解決)。
// 該当ファイルが無ければ空オブジェクトになるだけでエラーにはならない。
const imageModules = import.meta.glob("../assets/icons/*.{png,jpg,jpeg,webp,svg,gif}", {
  eager: true,
  query: "?url",
  import: "default",
}) as Record<string, string>;

// basename (拡張子なし) → URL の対応表。同名なら画像を優先する。
const imageByName: Record<string, string> = {};
for (const [path, url] of Object.entries(imageModules)) {
  const base = path.split("/").pop()?.replace(/\.[^.]+$/, "") ?? "";
  if (base) imageByName[base] = url;
}

/**
 * 指定アイコンのマークアップを返す (innerHTML へ差し込んで使う)。
 * assets/icons/<name>.<ext> があれば <img>、無ければ組み込み SVG を返す。
 */
export function icon(name: IconName): string {
  const img = imageByName[name];
  if (img) {
    return `<img class="icon icon-img" src="${img}" alt="" draggable="false" />`;
  }
  return `<svg class="icon" viewBox="0 0 24 24" width="1em" height="1em" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${PATHS[name]}</svg>`;
}
