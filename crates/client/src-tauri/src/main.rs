//! Aurora クライアント (Tauri) エントリポイント。

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod audio;
mod config;
mod net;
mod secret;
mod stats;

use net::SessionState;
use tauri::Manager;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aurora=info".into()),
        )
        .init();

    tauri::Builder::default()
        .manage(SessionState::default())
        // ウィンドウ破棄時の後始末: capture ウィンドウはその session を切断、launcher
        // (main) を閉じたらアプリ全体を終了する。
        .on_window_event(|window, event| {
            if !matches!(event, tauri::WindowEvent::Destroyed) {
                return;
            }
            let label = window.label().to_string();
            if label == "main" {
                window.app_handle().exit(0);
            } else if label.starts_with("capture-") {
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
        })
        .invoke_handler(tauri::generate_handler![
            net::connect_to_host,
            net::disconnect,
            net::open_capture_window,
            net::stage_session,
            net::open_session_window,
            net::take_handoff,
            net::attach_session,
            net::connected_hosts,
            net::send_input,
            net::request_keyframe,
            net::select_display,
            net::set_video_params,
            net::set_audio_muted,
            net::set_video_paused,
            net::set_resolution,
            net::set_client_viewport,
            net::request_file,
            net::upload_file_from_path,
            net::set_share_root,
            config::load_recent_hosts,
            config::save_recent_host,
            config::set_recent_hosts,
            config::delete_recent_host,
            config::load_settings,
            config::save_settings,
            secret::saved_password_hosts,
            secret::delete_host_password,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
