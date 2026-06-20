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
            } else if let Some(session_id) = label.strip_prefix("capture-") {
                let app = window.app_handle().clone();
                let id = session_id.to_string();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<SessionState>();
                    let handle = state.inner.lock().await.remove(&id);
                    if let Some(h) = handle {
                        h.shutdown();
                    }
                });
            }
        })
        .invoke_handler(tauri::generate_handler![
            net::connect_to_host,
            net::disconnect,
            net::open_capture_window,
            net::send_input,
            net::request_keyframe,
            net::select_display,
            net::set_video_params,
            net::set_audio_muted,
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
