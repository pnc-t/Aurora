//! OS 統合とローカル永続化をまとめる集約モジュール。
//!
//! - [`settings`]: 非機密ローカル設定 (待受ポート) の平文保存。
//! - [`autostart`]: Windows「ログオン時」自動起動タスクの登録 / 解除。
//!
//! どちらもホストと OS/ユーザープロファイルの結合点なので `system/` 配下に集約する。呼び出し側は
//! `crate::system::<名前>` で参照する (旧 `crate::settings` / `crate::autostart` を平坦再エクスポート)。

pub mod autostart;
pub mod settings;

pub use autostart::*;
pub use settings::*;
