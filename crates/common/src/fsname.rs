//! 受信ファイル名のサニタイズ。
//!
//! ファイル転送 (アップロード/ダウンロード) で対向から受け取った名前を、保存先
//! ディレクトリ内の単一ファイルへ確実に閉じ込めるための共通処理。host (`open_upload`)
//! と client (`open_download`) で同一規則を使い、防御を対称に保つ。

/// 受信ファイル名を保存先ディレクトリ内の安全な単一ファイル名へ正規化する。
///
/// - パス区切り (`/`, `\`) を含む名前は最終コンポーネントだけを採る
/// - 残った `:` (NTFS 代替データストリーム / ドライブ相対) と区切り文字は `_` に置換する。
///   これが無いと `a:b` のような名前で保存ディレクトリ内に「見えない」ADS を作れてしまう
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
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
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
}
