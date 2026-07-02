//! Aurora のローカル状態ディレクトリ解決 (host / client 共通)。
//!
//! 秘密ストア・設定・ログ・トポロジ保存など、プロセスが書くローカル状態は全てこの配下に
//! 置く。解決を 1 箇所に集約するのは、保存先の変更 (アプリ名変更・ポータブル化等) の際に
//! 一部のファイルだけ旧パスへ取り残されて状態が分裂する事故を防ぐため。

use std::path::PathBuf;

/// Aurora のローカル状態ベースディレクトリ (`%LOCALAPPDATA%\Aurora`、非 Windows は
/// `~/.local/share/Aurora`) を返す。環境変数が無ければ `None`。
///
/// ディレクトリの作成は呼び出し側の責務 (読むだけの経路で余計な作成をしないため)。
pub fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Aurora"))
    }
    #[cfg(not(windows))]
    {
        let mut p = PathBuf::from(std::env::var_os("HOME")?);
        p.push(".local");
        p.push("share");
        p.push("Aurora");
        Some(p)
    }
}
