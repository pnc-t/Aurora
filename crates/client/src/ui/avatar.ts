// カード用アバターの共有ヘルパー。Hosts (launcher) と WoL (wol-panel) の両方が使う。
// 画像アイコン (data URL) があれば画像を、無ければ label/host 由来の頭文字 + 決定的な背景色を出す。

/** ラベル/host から決定的に色を作りアバター背景に使う。
 *  白いイニシャル文字とのコントラストを確保するため明度は低め (38%) に固定する
 *  (黄/緑系の hue でも白文字が読める)。 */
export function avatarColor(seed: string): string {
  let h = 0;
  for (let i = 0; i < seed.length; i++) h = (h * 31 + seed.charCodeAt(i)) % 360;
  return `hsl(${h}, 52%, 38%)`;
}

/** data URL の画像アバターか判定する (描画分岐・保存値の検証に使う)。 */
export function hasImageIcon(icon: string | null | undefined): icon is string {
  return !!icon && icon.startsWith("data:image/");
}

/** 設定 JSON に埋め込む小さなアバター。選択画像を中央クロップで 64px 正方の PNG data URL にする。
 *  ファイルコピー/asset protocol を使わず data URL でそのまま保存・描画できるため軽い。 */
export async function imageToIconDataUrl(file: File): Promise<string> {
  const SIZE = 64;
  const bitmap = await createImageBitmap(file);
  try {
    const canvas = document.createElement("canvas");
    canvas.width = SIZE;
    canvas.height = SIZE;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("canvas 2d がありません");
    // アスペクト比維持で正方形へ中央クロップ。
    const side = Math.min(bitmap.width, bitmap.height);
    const sx = (bitmap.width - side) / 2;
    const sy = (bitmap.height - side) / 2;
    ctx.drawImage(bitmap, sx, sy, side, side, 0, 0, SIZE, SIZE);
    return canvas.toDataURL("image/png");
  } finally {
    bitmap.close();
  }
}
