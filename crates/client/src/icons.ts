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
  | "close";

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
};

// assets/icons/ 配下の画像を URL として一括取り込み (ビルド時に解決)。
// 該当ファイルが無ければ空オブジェクトになるだけでエラーにはならない。
const imageModules = import.meta.glob("./assets/icons/*.{png,jpg,jpeg,webp,svg,gif}", {
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
