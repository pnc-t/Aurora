//! 自動アップデートの minisign 署名検証。host の自己更新と client の更新マニフェスト検証で共有する。
//!
//! 署名は tauri updater と同じスキーム (`cargo tauri signer sign` の出力 = minisign 署名ファイル
//! 全体を base64 化したもの、公開鍵も base64 化した minisign 公開鍵ファイル)。prehashed 署名なので
//! `allow_legacy = false` で厳密に照合する。

use base64::Engine;

/// 署名検証の失敗。
#[derive(thiserror::Error, Debug)]
pub enum UpdateSigError {
    /// 公開鍵が base64 / UTF-8 / minisign 形式として不正。
    #[error("invalid update public key: {0}")]
    PublicKey(String),
    /// 署名が base64 / UTF-8 / minisign 形式として不正。
    #[error("invalid update signature: {0}")]
    Signature(String),
    /// 署名とデータが一致しない (改竄・破損・別鍵)。
    #[error("update signature verification failed")]
    Mismatch,
}

/// `data` を base64 化 minisign 公開鍵 `pubkey_b64` と base64 化署名 `signature_b64` で検証する。
pub fn verify(data: &[u8], pubkey_b64: &str, signature_b64: &str) -> Result<(), UpdateSigError> {
    let pubkey_text = decode_b64_text(pubkey_b64).map_err(UpdateSigError::PublicKey)?;
    let public_key = minisign_verify::PublicKey::decode(&pubkey_text)
        .map_err(|e| UpdateSigError::PublicKey(e.to_string()))?;
    let sig_text = decode_b64_text(signature_b64).map_err(UpdateSigError::Signature)?;
    let signature = minisign_verify::Signature::decode(&sig_text)
        .map_err(|e| UpdateSigError::Signature(e.to_string()))?;
    public_key
        .verify(data, &signature, false)
        .map_err(|_| UpdateSigError::Mismatch)
}

fn decode_b64_text(b64: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}
