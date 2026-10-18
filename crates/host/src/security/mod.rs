//! 接続を許可する相手の制御をまとめる集約モジュール。
//!
//! - [`secret`]: 接続パスワード (知識要素) の DPAPI ローカル暗号化保存。
//! - [`devices`]: 信頼するクライアント静的公開鍵の許可リスト = デバイスピン留め (所持要素)。
//!
//! いずれも「誰を通すか」を決める関心事なので `security/` 配下に集約する。呼び出し側は
//! `crate::security::<名前>` で参照する (旧 `crate::secret` / `crate::devices` を平坦再エクスポート)。

pub mod devices;
pub mod secret;

pub use devices::*;
pub use secret::*;
