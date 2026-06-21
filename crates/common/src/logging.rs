//! 共有ログ初期化。
//!
//! host / client(Tauri Rust 側)双方がこのモジュールで `tracing` を初期化し、同じ
//! フォーマット・出力先・ローテ方針を共有する。出力先は `%LOCALAPPDATA%\Aurora\logs`
//! 配下で、`tracing-appender` による**日次ローテ + 7 世代保持**、書き込みは**非ブロッキング**
//! (CLAUDE.md「tokio タスク内で同期 I/O を呼ばない」に準拠)。
//!
//! フィルタ (`EnvFilter`) の構築は各バイナリ側に委ねる (host は `--log-level` 引数を
//! 解釈する等、優先順位が異なるため)。本モジュールは出力レイヤの組み立てに専念する。

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// ローテ世代の保持数 (計画: 7 世代)。古い 7 ファイルを残し、それ以前は削除される。
const MAX_LOG_FILES: usize = 7;

/// [`init`] の引数。
pub struct LogOptions {
    /// ローテファイルのファイル名 stem。`host` なら `host.YYYY-MM-DD.log` を生成する。
    pub file_stem: &'static str,
    /// ログレベルフィルタ。`None` のとき console のみ・フィルタ既定 (`aurora=info`) になる。
    pub filter: EnvFilter,
    /// ファイル出力先ディレクトリ。`None` または作成失敗時はファイル出力を行わない
    /// (console へフォールバック)。通常は [`default_log_dir`] を渡す。
    pub dir: Option<PathBuf>,
    /// ファイル出力に加えて stdout にも出すか (デバッグビルド / CLI 実行時に true)。
    /// ファイル出力が無効な場合は、この値によらず stdout へフォールバックする。
    pub console: bool,
}

/// [`init`] の戻り値。非ブロッキング writer のワーカースレッドを生かし続けるガード。
///
/// **main の生存期間中ずっと保持すること。** drop すると残りのログが flush された後
/// ワーカーが停止し、以降のログが失われる (`let _guard = init(..)` で束縛する)。
#[must_use = "drop すると非ブロッキングログのワーカーが止まり、以降のログが失われる"]
pub struct LogGuard(#[allow(dead_code)] Option<WorkerGuard>);

/// 標準のログ用ディレクトリ (`%LOCALAPPDATA%\Aurora\logs`, 非 Windows は
/// `~/.local/share/Aurora/logs`) を返す。環境変数が無ければ `None`。
///
/// 「ログフォルダを開く」等の運用導線からも参照する。
pub fn default_log_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")?;
        let mut p = PathBuf::from(base);
        p.push("Aurora");
        p.push("logs");
        Some(p)
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var_os("HOME")?;
        let mut p = PathBuf::from(base);
        p.push(".local");
        p.push("share");
        p.push("Aurora");
        p.push("logs");
        Some(p)
    }
}

/// `tracing` を初期化する。ファイル出力 (日次ローテ + 7 世代) と、任意で console 出力を
/// 組み合わせる。プロセス起動初期に一度だけ呼ぶ。
pub fn init(opts: LogOptions) -> LogGuard {
    let LogOptions {
        file_stem,
        filter,
        dir,
        console,
    } = opts;

    // ファイル出力レイヤ (非ブロッキング)。ディレクトリ作成や appender 構築に失敗したら
    // None にフォールバックし、console 出力で運用を継続する。
    let file = dir.and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(file_stem)
            .filename_suffix("log")
            .max_log_files(MAX_LOG_FILES)
            .build(&dir)
            .ok()?;
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let layer = fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .with_writer(writer);
        Some((layer, guard))
    });

    let (file_layer, guard) = match file {
        Some((layer, guard)) => (Some(layer), Some(guard)),
        None => (None, None),
    };

    // ファイル出力が無いときは必ず console へフォールバックする (ログが消えないように)。
    let console_layer = if console || file_layer.is_none() {
        Some(fmt::layer().with_writer(std::io::stdout))
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(console_layer)
        .init();

    LogGuard(guard)
}
