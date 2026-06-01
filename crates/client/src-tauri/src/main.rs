//! DDD クライアント (Tauri) エントリポイント。

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod net;

use net::SessionState;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ddd=info".into()),
        )
        .init();

    tauri::Builder::default()
        .manage(SessionState::default())
        .invoke_handler(tauri::generate_handler![
            net::connect_to_host,
            net::disconnect,
            net::send_input,
            net::request_keyframe,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
