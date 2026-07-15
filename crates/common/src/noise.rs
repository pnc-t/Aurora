//! Noise XX-PSK ハンドシェイク + transport ラッパー。
//!
//! 使い方:
//! ```ignore
//! let psk = psk_from_password("hunter2");
//! let mut hs = HandshakeState::new_responder(&psk, None)?;
//! // TCP で 3 メッセージ往復: read_msg / write_msg を交互に呼ぶ
//! let session = hs.into_transport()?;
//! let mut out = Vec::new();
//! session.encrypt(b"hi", &mut out)?;
//! ```

use snow::{Builder, HandshakeState as SnowHandshake, StatelessTransportState};
use thiserror::Error;

pub const NOISE_PARAMS: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";

/// 接続パスワードの最低長 (文字数)。
///
/// XX-PSK は接続を試みた相手にハンドシェイク転写を渡すため、弱いパスワードは Argon2id を
/// 通してもオフライン辞書攻撃で破られうる。単一要素認証 (パスワードのみ) であることを踏まえ、
/// 8 文字では総当たり探索空間が不十分と判断し 12 文字に引き上げてオフライン耐性を底上げする。
/// host の `--set-password` と tray のパスワード変更ダイアログの両方で強制すること (設定経路
/// を対称に硬化する)。
pub const MIN_PASSWORD_LEN: usize = 12;

/// 最低長は満たすが弱いことが多い代表的パスワード (小文字化して比較)。網羅的辞書ではなく、
/// 設定時にソフト警告を出すための代表例。
const COMMON_WEAK: &[&str] = &[
    "password",
    "password1",
    "passw0rd",
    "12345678",
    "123456789",
    "1234567890",
    "qwertyui",
    "qwerty123",
    "11111111",
    "00000000",
    "aaaaaaaa",
    "iloveyou",
    "admin123",
    "administrator",
    "letmein1",
    "welcome1",
    "abc12345",
    "football",
    "baseball",
    "sunshine",
    "princess",
    "trustno1",
    "superman",
    "starwars",
    "whatever",
    "computer",
    "changeme",
    "aurora123",
];

/// パスワードに弱い兆候があれば理由を返す (`None` = 明白な弱さ無し)。最低長
/// ([`MIN_PASSWORD_LEN`]) チェックを通った後の **ソフト警告** 用 (ハード禁止はしない)。
///
/// host の `--set-password` (CLI) と tray のパスワード変更ダイアログの両方で使い、設定経路の
/// 防御を対称に保つ。長いパスフレーズ (12 文字以上) は文字種が単一でも許容する (これらは
/// オフライン耐性が十分で、無用な警告を出さないため)。
pub fn weak_password_reason(password: &str) -> Option<&'static str> {
    let lower = password.to_ascii_lowercase();
    if COMMON_WEAK.contains(&lower.as_str()) {
        return Some("よく使われるパスワードです");
    }
    // 全部同じ文字 (例: "aaaaaaaa")。空文字列は None に落とす。
    if let Some(first) = password.chars().next()
        && password.chars().all(|c| c == first)
    {
        return Some("同じ文字の繰り返しです");
    }
    // 全部数字 (例: 生年月日・連番)。
    if !password.is_empty() && password.chars().all(|c| c.is_ascii_digit()) {
        return Some("数字だけで構成されています");
    }
    // 文字種が 1 種類だけ かつ 短め (12 文字未満)。長いパスフレーズは許容する。
    let has_lower = password.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = password.chars().any(|c| c.is_ascii_digit());
    let has_other = password.chars().any(|c| !c.is_ascii_alphanumeric());
    let kinds = [has_lower, has_upper, has_digit, has_other]
        .iter()
        .filter(|b| **b)
        .count();
    if kinds <= 1 && password.chars().count() < 12 {
        return Some("文字種が少なく短いパスワードです");
    }
    None
}

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

/// nonce の上位 2 ビット (ドメイン選択ビット)。下位はストリーム内カウンタ/seq。
/// `nonce & NONCE_DOMAIN_MASK` で所属ドメインを取り出せる。受信側が「想定ストリームの
/// ドメインか」を検証し、別ドメイン (例: TCP 制御暗号文の UDP への注入) を弾くのに使う。
pub const NONCE_DOMAIN_MASK: u64 = 0b11 << 62;

#[derive(Debug, Error)]
pub enum NoiseError {
    #[error("snow error: {0}")]
    Snow(#[from] snow::Error),
    #[error("handshake not finished")]
    NotFinished,
    #[error("handshake already finished")]
    AlreadyFinished,
}

/// PSK 導出に使うアプリ固有の固定ソルト。
///
/// PSK は host/client が共有パスワードから **各自で独立に同じ値を計算**する必要があるため、
/// ソルトを通信で交換できない。よって固定ソルトを使う (パスワードストレージではなく
/// 共有鍵導出なので、毎回ユニークなソルトは要件ではない)。Argon2id のメモリ硬度
/// (m=64MiB, t=3, p=1) により、固定ソルトでもオフライン総当たりコストは blake2s 単発より
/// 桁違いに高い。
const PSK_SALT: &[u8] = b"aurora-noise-psk-v1";

/// 接続パスワードから PSK (32B) を導出する。
///
/// Argon2id (メモリ硬 KDF、m=64MiB/t=3/p=1) + 固定アプリソルト。弱いパスワードに対する
/// オフライン総当たり耐性を、単一ハッシュ (旧 blake2s) より大幅に高める。PSK 導出は 1 接続に
/// つき 1 回のみのため、このコストは遅延予算に影響しない。host/client で同一関数を使うため
/// 導出値は常に一致する。
///
/// **注意 (ワイヤ互換)**: このパラメータを変えると同じパスワードから導出される PSK が変わる
/// ため、host・client・aurora-wol を**すべて同一バージョンでリビルド**しないとハンドシェイクが
/// 一致しなくなる (旧バイナリとの混在運用不可)。
pub fn psk_from_password(password: &str) -> [u8; 32] {
    use argon2::{Algorithm, Argon2, Params, Version};
    // OWASP 最低ラインから引き上げ: m=64MiB, t=3, p=1。出力長 32B。
    let params = Params::new(64 * 1024, 3, 1, Some(32)).expect("valid argon2 params");
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), PSK_SALT, &mut out)
        .expect("argon2 derive psk");
    out
}

/// 永続的な静的鍵ペア (X25519)。デバイスの身元として使う。
///
/// XXpsk3 は双方の静的公開鍵を交換するが、既定では毎ハンドシェイクで使い捨て鍵を生成する
/// ため端末を識別できない。クライアントがこの鍵ペアを永続化し秘密鍵を毎回渡すことで、
/// `public` がその端末の安定した身元になり、host 側の許可リスト照合 (デバイスピン留め) が
/// 可能になる。秘密鍵は機密 (DPAPI 等で保護)、公開鍵は非機密 (許可リストに平文保存可)。
#[derive(Clone)]
pub struct StaticKeypair {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

/// 永続 ID 用の静的鍵ペアを 1 つ生成する。秘密鍵は安全に保存し、以降
/// [`HandshakeState::new_initiator`] に渡して同じ身元で接続する。
pub fn generate_static_keypair() -> Result<StaticKeypair, NoiseError> {
    let kp = Builder::new(NOISE_PARAMS.parse()?).generate_keypair()?;
    Ok(StaticKeypair {
        private: kp.private,
        public: kp.public,
    })
}

pub struct HandshakeState {
    inner: Option<SnowHandshake>,
}

impl HandshakeState {
    /// イニシエータ (クライアント) を構築する。`static_priv` に永続秘密鍵を渡すと端末の
    /// 身元が安定する (host のデバイスピン留めで照合できる)。`None` なら毎回使い捨て鍵。
    pub fn new_initiator(psk: &[u8; 32], static_priv: Option<&[u8]>) -> Result<Self, NoiseError> {
        Self::build(psk, static_priv, true)
    }

    /// レスポンダ (ホスト) を構築する。host は許可リスト照合を相手公開鍵
    /// ([`Self::remote_static`]) で行うため、自分の静的鍵は使い捨て (`None`) でよい。
    pub fn new_responder(psk: &[u8; 32], static_priv: Option<&[u8]>) -> Result<Self, NoiseError> {
        Self::build(psk, static_priv, false)
    }

    fn build(
        psk: &[u8; 32],
        static_priv: Option<&[u8]>,
        initiator: bool,
    ) -> Result<Self, NoiseError> {
        // 永続鍵が無ければ使い捨て鍵を生成する (従来挙動)。`generated` は借用が生きる間保持。
        let generated;
        let priv_key: &[u8] = match static_priv {
            Some(k) => k,
            None => {
                generated = Builder::new(NOISE_PARAMS.parse()?)
                    .generate_keypair()?
                    .private;
                &generated
            }
        };
        let builder = Builder::new(NOISE_PARAMS.parse()?)
            .local_private_key(priv_key)
            .psk(3, psk);
        let hs = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };
        Ok(Self { inner: Some(hs) })
    }

    /// ハンドシェイクで判明した相手の静的公開鍵を返す (XXpsk3 では相手の `s` 受信後に有効)。
    /// host がクライアントの身元として許可リスト照合に使う。
    pub fn remote_static(&self) -> Option<Vec<u8>> {
        self.inner.as_ref()?.get_remote_static().map(|s| s.to_vec())
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
    pub fn encrypt(
        &self,
        nonce: u64,
        plain: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<usize, NoiseError> {
        let start = out.len();
        out.resize(start + plain.len() + 16, 0);
        let n = self.inner.write_message(nonce, plain, &mut out[start..])?;
        out.truncate(start + n);
        Ok(n)
    }

    /// `nonce` で復号する。暗号化時と同じ nonce でなければ失敗する。
    pub fn decrypt(
        &self,
        nonce: u64,
        cipher: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<usize, NoiseError> {
        let start = out.len();
        out.resize(start + cipher.len(), 0);
        let n = self.inner.read_message(nonce, cipher, &mut out[start..])?;
        out.truncate(start + n);
        Ok(n)
    }
}

/// クライアント静的公開鍵から、オーナーが目視照合するための短い識別子を作る。host のペアリング
/// 承認 UI と client の自端末表示が**同じ値**を出す必要があるため common に置く。X25519 公開鍵は
/// 一様乱数なので先頭 8 バイトを 4 桁ずつ hex 表記する ("xxxx-xxxx-xxxx-xxxx"、64bit で目視照合に十分)。
pub fn device_fingerprint(pubkey: &[u8]) -> String {
    let hex: String = pubkey.iter().take(8).map(|b| format!("{b:02x}")).collect();
    hex.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xx_psk_roundtrip_and_transport() {
        let psk = psk_from_password("hunter2");
        let mut ini = HandshakeState::new_initiator(&psk, None).unwrap();
        let mut res = HandshakeState::new_responder(&psk, None).unwrap();

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
        assert!(
            ini_t
                .decrypt(NONCE_DOMAIN_VIDEO | 8, &ct2, &mut bad)
                .is_err()
        );
    }

    #[test]
    fn responder_sees_persistent_initiator_public_key() {
        // クライアントが永続鍵を使うと、host は同じ公開鍵を相手身元として観測でき、
        // かつ毎回同じ値になる (= 許可リスト照合の土台)。
        let psk = psk_from_password("hunter2");
        let id = generate_static_keypair().unwrap();

        let run = |id_priv: &[u8]| -> Vec<u8> {
            let mut ini = HandshakeState::new_initiator(&psk, Some(id_priv)).unwrap();
            let mut res = HandshakeState::new_responder(&psk, None).unwrap();
            let mut buf = Vec::new();
            let mut tmp = Vec::new();
            ini.write_msg(&[], &mut buf).unwrap();
            res.read_msg(&buf, &mut tmp).unwrap();
            res.write_msg(&[], &mut buf).unwrap();
            ini.read_msg(&buf, &mut tmp).unwrap();
            ini.write_msg(&[], &mut buf).unwrap();
            res.read_msg(&buf, &mut tmp).unwrap();
            res.remote_static()
                .expect("remote static known after handshake")
        };

        let seen1 = run(&id.private);
        let seen2 = run(&id.private);
        assert_eq!(
            seen1, id.public,
            "host observes the client's persistent key"
        );
        assert_eq!(seen1, seen2, "same identity yields a stable public key");
    }

    #[test]
    fn into_transport_before_finish_errors() {
        let psk = [0u8; 32];
        let hs = HandshakeState::new_initiator(&psk, None).unwrap();
        assert!(matches!(hs.into_transport(), Err(NoiseError::NotFinished)));
    }

    #[test]
    fn weak_password_flags_common_and_low_diversity() {
        // よくあるパスワード (大文字小文字無視)。
        assert!(weak_password_reason("password").is_some());
        assert!(weak_password_reason("PassW0rd").is_some());
        // 全部数字 / 全部同じ文字。
        assert!(weak_password_reason("12345678").is_some());
        assert!(weak_password_reason("aaaaaaaa").is_some());
        // 文字種 1 種類で短い。
        assert!(weak_password_reason("abcdefgh").is_some());
    }

    #[test]
    fn weak_password_allows_strong_and_long() {
        // 文字種が多い短めのパスワード。
        assert!(weak_password_reason("Tr0ub4dor&3").is_none());
        // 長いパスフレーズ (文字種が単一でも 12 文字以上なら許容)。
        assert!(weak_password_reason("correcthorsebatterystaple").is_none());
        assert!(weak_password_reason("correct-horse-battery-staple").is_none());
    }

    #[test]
    fn device_fingerprint_formats_and_is_stable() {
        let key = [0xabu8, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xff, 0xff];
        let fp = device_fingerprint(&key);
        assert_eq!(fp, "abcd-ef01-2345-6789");
        // 同じ鍵なら常に同じ値 (host/client 双方が同じ表示になる必要がある)。
        assert_eq!(fp, device_fingerprint(&key));
    }
}
