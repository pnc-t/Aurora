//! Noise XX-PSK ハンドシェイク + transport ラッパー。
//!
//! 使い方:
//! ```ignore
//! let psk = psk_from_password("hunter2");
//! let mut hs = HandshakeState::new_responder(&psk)?;
//! // TCP で 3 メッセージ往復: read_msg / write_msg を交互に呼ぶ
//! let session = hs.into_transport()?;
//! let mut out = Vec::new();
//! session.encrypt(b"hi", &mut out)?;
//! ```

use snow::{Builder, HandshakeState as SnowHandshake, StatelessTransportState};
use thiserror::Error;

pub const NOISE_PARAMS: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";

/// Nonce 空間のドメイン分割。
///
/// TCP 制御・UDP 映像・UDP 音声は同一 UDP/TCP 接続上で **同一 Noise 方向 (= 同一鍵)**
/// を共有する。ChaChaPoly は nonce 再利用が致命的なため、各ストリームを上位ビットで
/// 別ドメインに隔離し、ストリーム内のカウンタ (制御は連番、映像/音声は seq) だけが
/// 下位ビットを使う。これにより 3 ストリームを跨いだ nonce 衝突を防ぐ。
///
/// 注意: 映像/音声は seq (u32) を nonce 下位に使うため、seq 一周 (約 42 億パケット) で
/// nonce が一巡する。MVP の 1 セッション長では到達しない前提。
pub const NONCE_DOMAIN_VIDEO: u64 = 0;
pub const NONCE_DOMAIN_AUDIO: u64 = 1 << 62;
pub const NONCE_DOMAIN_CTRL: u64 = 1 << 63;

#[derive(Debug, Error)]
pub enum NoiseError {
    #[error("snow error: {0}")]
    Snow(#[from] snow::Error),
    #[error("handshake not finished")]
    NotFinished,
    #[error("handshake already finished")]
    AlreadyFinished,
}

/// 接続パスワードから PSK (32B) を導出する。
/// MVP では blake2s。鍵強度が要るなら後で Argon2 に差し替える。
pub fn psk_from_password(password: &str) -> [u8; 32] {
    use blake2s_simd::Params;
    let hash = Params::new().hash_length(32).hash(password.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(hash.as_bytes());
    out
}

pub struct HandshakeState {
    inner: Option<SnowHandshake>,
}

impl HandshakeState {
    pub fn new_initiator(psk: &[u8; 32]) -> Result<Self, NoiseError> {
        let builder = Builder::new(NOISE_PARAMS.parse()?);
        let kp = builder.generate_keypair()?;
        let hs = Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(&kp.private)
            .psk(3, psk)
            .build_initiator()?;
        Ok(Self { inner: Some(hs) })
    }

    pub fn new_responder(psk: &[u8; 32]) -> Result<Self, NoiseError> {
        let builder = Builder::new(NOISE_PARAMS.parse()?);
        let kp = builder.generate_keypair()?;
        let hs = Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(&kp.private)
            .psk(3, psk)
            .build_responder()?;
        Ok(Self { inner: Some(hs) })
    }

    pub fn write_msg(&mut self, payload: &[u8], out: &mut Vec<u8>) -> Result<usize, NoiseError> {
        let hs = self.inner.as_mut().ok_or(NoiseError::AlreadyFinished)?;
        out.resize(payload.len() + 1024, 0);
        let n = hs.write_message(payload, out)?;
        out.truncate(n);
        Ok(n)
    }

    pub fn read_msg(&mut self, msg: &[u8], out: &mut Vec<u8>) -> Result<usize, NoiseError> {
        let hs = self.inner.as_mut().ok_or(NoiseError::AlreadyFinished)?;
        out.resize(msg.len() + 1024, 0);
        let n = hs.read_message(msg, out)?;
        out.truncate(n);
        Ok(n)
    }

    pub fn is_handshake_finished(&self) -> bool {
        self.inner
            .as_ref()
            .map(|h| h.is_handshake_finished())
            .unwrap_or(false)
    }

    pub fn into_transport(mut self) -> Result<TransportSession, NoiseError> {
        let hs = self.inner.take().ok_or(NoiseError::AlreadyFinished)?;
        if !hs.is_handshake_finished() {
            return Err(NoiseError::NotFinished);
        }
        let t = hs.into_stateless_transport_mode()?;
        Ok(TransportSession { inner: t })
    }
}

/// 明示 nonce 方式のセッション暗号。
///
/// snow の **stateless transport** を使い、各メッセージの nonce を呼び出し側が指定する。
/// これにより 1 本のハンドシェイク鍵を、信頼順序のある TCP 制御と、ロス・再順序のある
/// UDP メディアで安全に共有できる (暗黙カウンタ方式だと TCP/UDP 間で nonce がズレて
/// 復号が全滅する)。nonce は [`NONCE_DOMAIN_CTRL`] / [`NONCE_DOMAIN_VIDEO`] /
/// [`NONCE_DOMAIN_AUDIO`] でドメイン分割して再利用を防ぐこと。
pub struct TransportSession {
    inner: StatelessTransportState,
}

impl TransportSession {
    /// `nonce` で暗号化し、暗号文を `out` の末尾に追記する。戻り値は書き込んだバイト数。
    pub fn encrypt(&self, nonce: u64, plain: &[u8], out: &mut Vec<u8>) -> Result<usize, NoiseError> {
        let start = out.len();
        out.resize(start + plain.len() + 16, 0);
        let n = self.inner.write_message(nonce, plain, &mut out[start..])?;
        out.truncate(start + n);
        Ok(n)
    }

    /// `nonce` で復号する。暗号化時と同じ nonce でなければ失敗する。
    pub fn decrypt(&self, nonce: u64, cipher: &[u8], out: &mut Vec<u8>) -> Result<usize, NoiseError> {
        let start = out.len();
        out.resize(start + cipher.len(), 0);
        let n = self.inner.read_message(nonce, cipher, &mut out[start..])?;
        out.truncate(start + n);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xx_psk_roundtrip_and_transport() {
        let psk = psk_from_password("hunter2");
        let mut ini = HandshakeState::new_initiator(&psk).unwrap();
        let mut res = HandshakeState::new_responder(&psk).unwrap();

        let mut buf = Vec::new();
        let mut tmp = Vec::new();

        // -> e
        ini.write_msg(&[], &mut buf).unwrap();
        res.read_msg(&buf, &mut tmp).unwrap();
        // <- e, ee, s, es
        res.write_msg(&[], &mut buf).unwrap();
        ini.read_msg(&buf, &mut tmp).unwrap();
        // -> s, se, psk
        ini.write_msg(&[], &mut buf).unwrap();
        res.read_msg(&buf, &mut tmp).unwrap();

        assert!(ini.is_handshake_finished());
        assert!(res.is_handshake_finished());

        let ini_t = ini.into_transport().unwrap();
        let res_t = res.into_transport().unwrap();

        // 同一 nonce で暗号化/復号できる (方向ごとに鍵が異なる)。
        let mut ct = Vec::new();
        ini_t.encrypt(NONCE_DOMAIN_CTRL, b"hello", &mut ct).unwrap();
        let mut pt = Vec::new();
        res_t.decrypt(NONCE_DOMAIN_CTRL, &ct, &mut pt).unwrap();
        assert_eq!(pt, b"hello");

        let mut ct2 = Vec::new();
        res_t
            .encrypt(NONCE_DOMAIN_VIDEO | 7, b"world", &mut ct2)
            .unwrap();
        let mut pt2 = Vec::new();
        ini_t
            .decrypt(NONCE_DOMAIN_VIDEO | 7, &ct2, &mut pt2)
            .unwrap();
        assert_eq!(pt2, b"world");

        // nonce が違えば復号は失敗する。
        let mut bad = Vec::new();
        assert!(ini_t.decrypt(NONCE_DOMAIN_VIDEO | 8, &ct2, &mut bad).is_err());
    }

    #[test]
    fn into_transport_before_finish_errors() {
        let psk = [0u8; 32];
        let hs = HandshakeState::new_initiator(&psk).unwrap();
        assert!(matches!(hs.into_transport(), Err(NoiseError::NotFinished)));
    }
}
