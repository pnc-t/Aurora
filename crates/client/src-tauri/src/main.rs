//! Aurora クライアント (Tauri) エントリポイント。

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
// 新たに追加する unsafe ブロックには直上に `// SAFETY:` を必須化する (規約の機械強制)。
#![warn(clippy::undocumented_unsafe_blocks)]

mod audio;
mod config;
mod keyboard_grab;
mod log;
mod net;
mod secret;
mod stats;

use net::SessionState;
use tauri::Manager;

fn main() {
    // ログを `%LOCALAPPDATA%\Aurora\logs\client.<日付>.log` へ日次ローテ (7 世代) + 非ブロッキング
    // で残す。release は GUI サブシステム (コンソールなし) のため、ファイルに残さないと現地の
    // 障害を後から調べられない。debug ビルドは加えて stdout にも出す。_guard は run() が返る
    // (プロセス終了) まで保持する (drop すると以降のログが失われる)。
    let _guard = aurora_common::logging::init(aurora_common::logging::LogOptions {
        file_stem: "client",
        filter: tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "aurora=info".into()),
        dir: aurora_common::logging::default_log_dir(),
        console: cfg!(debug_assertions),
    });

    // panic を client.log へ残す (release は WebView/コンソールが無く既定フックの stderr 出力が
    // 捨てられるため)。既定フックも連鎖させ debug 時の挙動は維持する。
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

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(SessionState::default())
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
                        for id in ids {
                            if let Some(h) = guard.remove(&id) {
                                h.shutdown();
                            }
                        }
                        drop(guard);
                        // ランチャーへ接続中 host 一覧を通知 (ウィンドウを閉じた分の切断を反映)。
                        net::emit_connections(&app, &state).await;
                    });
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
