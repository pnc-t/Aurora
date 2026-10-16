//! OS 統合とローカル永続化をまとめる集約モジュール。
//!
//! - [`settings`]: 非機密ローカル設定 (待受ポート) の平文保存。
//! - [`autostart`]: Windows「ログオン時」自動起動タスクの登録 / 解除。
//! - [`instance`]: 常駐 host の単一インスタンス保証 (`crate::system::instance::acquire` で参照)。
//! - [`desktop`]: capture / input スレッドを入力デスクトップ (UAC・ロック画面の Winlogon) へ追従させる。
//! - [`state`]: PC 単位の状態ディレクトリ (`%ProgramData%\Aurora\host`) の解決と ACL 保護。
//! - [`service`]: SYSTEM サービス (`--service`) と、対話セッションへの本体起動 (`crate::system::service::*` で参照)。
//!
//! どちらもホストと OS/ユーザープロファイルの結合点なので `system/` 配下に集約する。呼び出し側は
//! `crate::system::<名前>` で参照する (旧 `crate::settings` / `crate::autostart` を平坦再エクスポート)。

pub mod autostart;
pub mod desktop;
pub mod instance;
pub mod service;
pub mod settings;
pub mod state;

pub use autostart::*;
pub use settings::*;
