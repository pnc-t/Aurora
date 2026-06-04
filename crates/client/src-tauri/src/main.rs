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

use net::{SessionState, ShareRoot};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aurora=info".into()),
        )
        .init();

    tauri::Builder::default()
        .manage(SessionState::default())
        .manage(ShareRoot::default())
        .invoke_handler(tauri::generate_handler![
            net::connect_to_host,
            net::disconnect,
            net::send_input,
            net::request_keyframe,
            net::select_display,
            net::set_video_params,
            net::send_file_offer,
            net::send_file_chunk,
            net::request_file,
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
