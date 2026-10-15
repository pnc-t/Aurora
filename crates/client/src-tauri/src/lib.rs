//! Aurora クライアント (Tauri) アプリケーション本体。
//!
//! デスクトップは `main.rs` の `fn main()` から、モバイル (Android/iOS) は Activity が
//! この共有ライブラリをロードして `run()` (= `tauri::mobile_entry_point`) から起動する。
//! Builder の組み立てをこちらへ集約するのはそのため (デスクトップ専用バイナリに
//! Builder を置くとモバイルからリンクできない)。

// 新たに追加する unsafe ブロックには直上に `// SAFETY:` を必須化する (規約の機械強制)。
#![warn(clippy::undocumented_unsafe_blocks)]

// JNI 共有基盤 (JNI_OnLoad は 1 プロセスに 1 つしかエクスポートできないため集約) と
// クリップボード書き込みは Android 専用 (jni クレート自体が Android ターゲットのみの依存)。
#[cfg(target_os = "android")]
mod android_jni;
mod audio;
mod config;
mod keyboard_grab;
mod log;
#[cfg(target_os = "android")]
mod mobile_clipboard;
mod net;
mod secret;
mod stats;

use net::SessionState;
use tauri::Manager;

/// ログ初期化。出力先ディレクトリの解決だけプラットフォームで分かれる:
/// - デスクトップ: 従来通り `common::paths::data_dir()` (`%LOCALAPPDATA%\Aurora\logs`)。
/// - モバイル: `common::paths::data_dir()` は `$HOME` 前提で Android では解決できず/書けない
///   (`HOME` 環境変数が無い、あっても書き込み不可) ため、Tauri のプラットフォーム別パス解決
///   (`app.path().app_log_dir()`) を使う。これには `AppHandle` が要る = `setup()` 内でしか
///   呼べないため、ログ初期化全体をここまで遅らせている (起動ごく初期・setup 前のログは
///   この変更で file には残らなくなるが、モバイルでログが一切出ない方が実害が大きいため許容)。
///
/// 返す `LogGuard` は呼び出し側が `app.manage()` してプロセス終了までライフタイムを繋ぐこと
/// (drop すると非ブロッキング writer が止まり以降のログが失われる)。
fn init_logging(app: &tauri::AppHandle) -> aurora_common::logging::LogGuard {
    let guard = aurora_common::logging::init(aurora_common::logging::LogOptions {
        file_stem: "client",
        filter: tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "aurora=info".into()),
        dir: resolve_log_dir(app),
        console: cfg!(debug_assertions),
    });

    // panic を client.log へ残す (release は WebView/コンソールが無く既定フックの stderr 出力が
    // 捨てられるため)。既定フックも連鎖させ debug 時の挙動は維持する。setup() 内で毎回呼ばれる
    // 関数だが、フック登録自体はプロセスに 1 度でよく、run() は 1 回しか呼ばれない前提。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "client panicked");
        default_hook(info);
    }));

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        debug = cfg!(debug_assertions),
        "aurora-client starting"
    );

    guard
}

/// ログ出力先ディレクトリの解決。デスクトップ/モバイルで方式が異なるため cfg で分ける
/// ([`init_logging`] のドキュメント参照)。
#[cfg(desktop)]
fn resolve_log_dir(_app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    aurora_common::logging::default_log_dir()
}

#[cfg(not(desktop))]
fn resolve_log_dir(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_log_dir().ok()
}

/// アプリ本体の起動。デスクトップは `main()` から、モバイルは OS が直接この関数を呼ぶ。
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // モバイルは #[cfg(desktop)] ブロックで再代入しないため mut が不要になる。デスクトップ側の
    // 再代入は必要なので allow で吸収する (cfg ごとの mut 要否を個別に書き分けない)。
    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default();

    // updater/process プラグインと capture ウィンドウのライフサイクル管理は
    // デスクトップ専用 (モバイルは単一 WebView でウィンドウを生成しない)。
    #[cfg(desktop)]
    {
        builder = builder
            .plugin(tauri_plugin_updater::Builder::new().build())
            .plugin(tauri_plugin_process::init())
            // ウィンドウ破棄時の後始末: capture ウィンドウはその session を切断、launcher
            // (main) を閉じたらアプリ全体を終了する。
            .on_window_event(|window, event| match event {
                // launcher からの接続を既存 capture 窓のタブへルーティングするため、直近の
                // フォーカス窓を追跡する (open_capture_window が同期的に参照する)。
                tauri::WindowEvent::Focused(true) => {
                    let label = window.label().to_string();
                    if label.starts_with("capture-") {
                        let state = window.app_handle().state::<SessionState>();
                        if let Ok(mut g) = state.last_focused_capture.lock() {
                            *g = Some(label);
                        }
                    }
                }
                tauri::WindowEvent::Destroyed => {
                    let label = window.label().to_string();
                    if label == "main" {
                        window.app_handle().exit(0);
                    } else if label.starts_with("capture-") {
                        keyboard_grab::release_for_window(&label);
                        // 死んだ窓へ以後ルーティングしないよう、追跡していたなら外す。
                        let state = window.app_handle().state::<SessionState>();
                        if let Ok(mut g) = state.last_focused_capture.lock()
                            && g.as_deref() == Some(label.as_str())
                        {
                            *g = None;
                        }
                        // 1 capture ウィンドウに複数タブ (セッション) がぶら下がるため、このウィンドウ
                        // を所有ウィンドウとする全セッションをまとめて停止する (タブの取りこぼし防止)。
                        let app = window.app_handle().clone();
                        tauri::async_runtime::spawn(async move {
                            let state = app.state::<SessionState>();
                            let mut guard = state.inner.lock().await;
                            let ids: Vec<String> = guard
                                .iter()
                                .filter(|(_, h)| h.window_label == label)
                                .map(|(id, _)| id.clone())
                                .collect();
                            let mut stopped_ids = Vec::with_capacity(ids.len());
                            for id in ids {
                                if let Some(h) = guard.remove(&id) {
                                    h.shutdown();
                                    stopped_ids.push(id);
                                }
                            }
                            drop(guard);
                            // 停止した全セッション分、staging に保持していた接続資格情報も破棄する。
                            {
                                let mut staging = state.staging.lock().await;
                                for id in &stopped_ids {
                                    staging.remove(id);
                                }
                            }
                            // ランチャーへ接続中 host 一覧を通知 (ウィンドウを閉じた分の切断を反映)。
                            net::emit_connections(&app, &state).await;
                        });
                    }
                }
                _ => {}
            });
    }

    let builder = builder.manage(SessionState::default());

    // ログ初期化はここ (setup) まで遅らせる: モバイルの出力先解決 (app_log_dir) に
    // AppHandle が要るため ([`init_logging`] のドキュメント参照)。LogGuard は state として
    // 管理し、app (= プロセス) の生存期間ぶら下げる (drop すると以降のログが失われる)。
    let builder = builder.setup(|app| {
        let guard = init_logging(app.handle());
        app.manage(guard);
        Ok(())
    });

    // invoke_handler は `tauri::generate_handler!` がマクロのためチェーン中で条件分岐できず、
    // デスクトップ/モバイルで 2 本のリストを用意する。差分はウィンドウ生成・複数ウィンドウ間の
    // 引き継ぎ・キーボードフックのグローバル操作・ログのファイラ連携など、単一 WebView の
    // モバイルには存在しない (or 意味を持たない) コマンド群。
    #[cfg(desktop)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        net::connect_to_host,
        net::disconnect,
        net::disconnect_host,
        net::open_capture_window,
        net::stage_session,
        net::open_session_window,
        net::take_handoff,
        net::attach_session,
        net::unstage_session,
        net::find_capture_window_at,
        net::handoff_session_to_window,
        net::connected_hosts,
        net::focus_host_window,
        net::probe_hosts,
        net::send_input,
        net::request_keyframe,
        net::select_display,
        net::set_video_params,
        net::set_supported_codecs,
        net::set_audio_muted,
        net::set_audio_volume,
        net::set_clipboard_sync,
        net::set_video_paused,
        net::set_cursor_overlay,
        net::set_resolution,
        net::set_client_viewport,
        net::request_file,
        net::upload_file_from_path,
        net::set_file_sharing,
        net::wake_host,
        keyboard_grab::set_keyboard_grab,
        config::load_recent_hosts,
        config::save_recent_host,
        config::set_recent_hosts,
        config::delete_recent_host,
        config::load_settings,
        config::save_settings,
        config::list_wol_targets,
        config::save_wol_target,
        config::set_wol_targets,
        config::delete_wol_target,
        secret::saved_password_hosts,
        secret::delete_host_password,
        secret::my_device_fingerprint,
        log::frontend_log,
        log::perf_enabled,
        log::open_logs_folder,
        log::export_logs_zip,
    ]);
    #[cfg(mobile)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        net::connect_to_host,
        net::disconnect,
        net::disconnect_host,
        net::stage_session,
        net::unstage_session,
        net::connected_hosts,
        net::probe_hosts,
        net::send_input,
        net::request_keyframe,
        net::select_display,
        net::set_video_params,
        net::set_supported_codecs,
        net::set_audio_muted,
        net::set_audio_volume,
        net::attach_audio_channel,
        net::set_clipboard_sync,
        net::set_video_paused,
        net::set_cursor_overlay,
        net::set_resolution,
        net::set_client_viewport,
        net::request_file,
        net::upload_file_from_path,
        net::set_file_sharing,
        net::wake_host,
        config::load_recent_hosts,
        config::save_recent_host,
        config::set_recent_hosts,
        config::delete_recent_host,
        config::load_settings,
        config::save_settings,
        config::list_wol_targets,
        config::save_wol_target,
        config::set_wol_targets,
        config::delete_wol_target,
        secret::saved_password_hosts,
        secret::delete_host_password,
        secret::my_device_fingerprint,
        log::frontend_log,
        log::perf_enabled,
    ]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
