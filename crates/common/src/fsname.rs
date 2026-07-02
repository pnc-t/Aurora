//! 受信ファイル名のサニタイズと保存先ファイルの作成。
//!
//! ファイル転送 (アップロード/ダウンロード) で対向から受け取った名前を、保存先
//! ディレクトリ内の単一ファイルへ確実に閉じ込めるための共通処理。host (`open_upload`)
//! と client (`open_download`) で同一実装 ([`create_incoming_file`]) を使い、防御を
//! 対称に保つ (片側だけ強化して危険な方向が未強化のまま残るのを防ぐ)。

/// Windows の予約デバイス名 (大文字小文字無視)。拡張子を付けても予約扱い
/// (`aux.txt` 等) のため、最初の `.` より前の語幹で判定する。
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 受信ファイル名を保存先ディレクトリ内の安全な単一ファイル名へ正規化する。
///
/// - パス区切り (`/`, `\`) を含む名前は最終コンポーネントだけを採る
/// - 残った `:` (NTFS 代替データストリーム / ドライブ相対) と区切り文字は `_` に置換する。
///   これが無いと `a:b` のような名前で保存ディレクトリ内に「見えない」ADS を作れてしまう
/// - 先頭/末尾の `.` と空白を落とす。Windows は末尾の `.`/空白を Win32 層で黙って剥がす
///   ため、残すと「表示名と実体が食い違う」ファイルができる
/// - Windows の予約デバイス名 (`CON`, `aux.txt` 等) は `_` を前置して無害化する。
///   そのまま使うと `File::create` がデバイス名前空間に触れて失敗・誤動作する
/// - 空・`.`・`..` など実体を持たない名前は `default` にフォールバックする
pub fn sanitize_filename(name: &str, default: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .unwrap_or(default);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if matches!(c, ':' | '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(['.', ' ']);
    if trimmed.is_empty() {
        return default.to_string();
    }
    // 予約名判定: 語幹 (最初の `.` の手前、末尾空白除去) を大文字小文字無視で比較する。
    let stem = trimmed
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ');
    if WINDOWS_RESERVED
        .iter()
        .any(|r| stem.eq_ignore_ascii_case(r))
    {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// 受信ファイルの保存ルート (`%USERPROFILE%\Aurora\<subdir>`、非 Windows は `$HOME` 配下)。
///
/// トレイ常駐・ログオン時自動起動・ショートカット起動では CWD が `System32` 等の不定な
/// (書き込み不可もありうる) 場所になるため、CWD 相対ではなくユーザープロファイル配下の
/// 固定パスへ絶対化して discoverable にする。`USERPROFILE`/`HOME` が無ければ最後の手段
/// としてカレントディレクトリ。
fn incoming_dir(subdir: &str) -> std::path::PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("Aurora").join(subdir)
}

/// [`incoming_dir`] 配下に受信ファイルを作成する。ファイル名は [`sanitize_filename`] で
/// 単一ファイル名へ正規化する。戻り値は (作成した `File`, 表示用フルパス)。
pub async fn create_incoming_file(
    subdir: &str,
    name: &str,
    fallback: &str,
) -> std::io::Result<(tokio::fs::File, String)> {
    let dir = incoming_dir(subdir);
    tokio::fs::create_dir_all(&dir).await?;
    let safe = sanitize_filename(name, fallback);
    let path = dir.join(&safe);
    let file = tokio::fs::File::create(&path).await?;
    Ok((file, path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::sanitize_filename;

    #[test]
    fn strips_path_separators() {
        assert_eq!(sanitize_filename("a/b/c.txt", "x.bin"), "c.txt");
        assert_eq!(sanitize_filename(r"a\b\c.txt", "x.bin"), "c.txt");
    }

    #[test]
    fn neutralizes_ads_colon() {
        // `a:b` でディレクトリ内に代替データストリームを作らせない。
        assert!(!sanitize_filename("a:b", "x.bin").contains(':'));
    }

    #[test]
    fn falls_back_on_empty_or_dots() {
        assert_eq!(sanitize_filename("", "x.bin"), "x.bin");
        assert_eq!(sanitize_filename(".", "x.bin"), "x.bin");
        assert_eq!(sanitize_filename("..", "x.bin"), "x.bin");
        assert_eq!(sanitize_filename("...", "x.bin"), "x.bin");
    }

    #[test]
    fn neutralizes_windows_reserved_names() {
        // 予約デバイス名はそのまま `File::create` するとデバイスに触れる。`_` 前置で無害化。
        assert_eq!(sanitize_filename("CON", "x.bin"), "_CON");
        assert_eq!(sanitize_filename("con", "x.bin"), "_con");
        assert_eq!(sanitize_filename("aux.txt", "x.bin"), "_aux.txt");
        assert_eq!(sanitize_filename("COM1.log", "x.bin"), "_COM1.log");
        assert_eq!(sanitize_filename("lpt9", "x.bin"), "_lpt9");
        // 予約名を含むだけ (前方一致しない) なら通常名のまま。
        assert_eq!(sanitize_filename("CONFIG.txt", "x.bin"), "CONFIG.txt");
        assert_eq!(sanitize_filename("COM10.txt", "x.bin"), "COM10.txt");
    }

    #[test]
    fn trims_trailing_spaces_and_dots() {
        // Windows は末尾の空白/ドットを Win32 層で剥がすため、先に落として実体と一致させる。
        assert_eq!(sanitize_filename("file ", "x.bin"), "file");
        assert_eq!(sanitize_filename("file. ", "x.bin"), "file");
        assert_eq!(sanitize_filename("  ", "x.bin"), "x.bin");
    }
}
