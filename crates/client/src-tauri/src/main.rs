//! Aurora クライアント (Tauri) デスクトップ実行バイナリのエントリポイント。
//!
//! モバイル (Android/iOS) は Activity が共有ライブラリを直接ロードするため、
//! Tauri Builder の組み立ては `lib.rs::run()` 側に置く。この `main` はデスクトップ
//! バイナリ用の薄い呼び出し口に過ぎない。

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    aurora_client_lib::run();
}
