//! Noise XX-PSK ハンドシェイクラッパー (実装はステップ2で本格化)。

use thiserror::Error;

pub const NOISE_PARAMS: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";

#[derive(Debug, Error)]
pub enum NoiseError {
    #[error("snow error: {0}")]
    Snow(#[from] snow::Error),
}

/// 接続パスワードから PSK (32B) を導出する。
/// 本実装ではArgon2を推奨。MVPでは blake2s でハッシュする簡易方式。
pub fn psk_from_password(password: &str) -> [u8; 32] {
    use blake2s_simd::Params;
    let hash = Params::new().hash_length(32).hash(password.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(hash.as_bytes());
    out
}
