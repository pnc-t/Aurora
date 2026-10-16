//! Aurora のローカル状態ディレクトリ解決 (host / client デスクトップ共通)。
//!
//! 秘密ストア・設定・ログ・トポロジ保存など、プロセスが書くローカル状態は全てこの配下に
//! 置く。解決を 1 箇所に集約するのは、保存先の変更 (アプリ名変更・ポータブル化等) の際に
//! 一部のファイルだけ旧パスへ取り残されて状態が分裂する事故を防ぐため。
//!
//! **Android では使えない**: [`data_dir`] は `$HOME` の存在を前提にしており、Android には
//! 書き込み可能な `$HOME` が無い (環境変数自体が無いか、あっても書けない)。Android クライアント
//! は代わりに Tauri のプラットフォーム別パス解決 (`AppHandle::path().app_log_dir()` 等) を
//! 直接使う (`crates/client/src-tauri/src/lib.rs::resolve_log_dir` 参照)。iOS も未検証だが
//! 同様に非対応と見なす。

use std::path::PathBuf;

/// Aurora のローカル状態ベースディレクトリ (`%LOCALAPPDATA%\Aurora`、非 Windows デスクトップ
/// (Mac/Linux) は `~/.local/share/Aurora`) を返す。環境変数が無ければ `None`。
/// Android/iOS では意味のあるパスを返さない (モジュール doc 参照)。
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

/// PC 単位 (全ユーザー共通) の状態ディレクトリ (`%ProgramData%\Aurora`) を返す。環境変数が
/// 無ければ `None`。非 Windows は [`data_dir`] と同じ。
///
/// host を SYSTEM サービスで動かすと `%LOCALAPPDATA%` が SYSTEM のプロファイルを指し、サインイン
/// 前にも状態を読む必要があるため、host はこちらに置く (client は [`data_dir`] のまま)。
/// **既定の ACL は一般ユーザーにも読み取り/作成を許す**ので、機密や改ざんされては困る状態を置く
/// 呼び出し側が、作成・使用前にディレクトリの所有者と ACL を固めること。
pub fn machine_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        Some(PathBuf::from(std::env::var_os("ProgramData")?).join("Aurora"))
    }
    #[cfg(not(windows))]
    {
        data_dir()
    }
}
