//! UDP メディアデータグラムの暗号化と対向確定 (TCP 制御フレームは `aurora_common::framing`)。
use super::*;

/// UDP メディアデータグラムを構築する。
///
/// plaintext は `[stream tag(1B)] + framed` を `nonce` で暗号化し、受信側が復号前に
/// nonce を知れるよう **暗号文の前に nonce を 8B (BE) 平文で付与**する。
/// 出力: `[nonce(8B BE)] + [ciphertext]`。
pub(crate) fn encrypt_udp(
    session: &TransportSession,
    nonce: u64,
    stream: u8,
    framed: &[u8],
) -> Result<Vec<u8>> {
    let mut plain = Vec::with_capacity(1 + framed.len());
    plain.push(stream);
    plain.extend_from_slice(framed);
    let mut dgram = Vec::with_capacity(8 + plain.len() + 16);
    dgram.extend_from_slice(&nonce.to_be_bytes());
    session.encrypt(nonce, &plain, &mut dgram)?;
    Ok(dgram)
}

/// AURHELLO (初回 + client 自己修復の再送) を送信元 IP でセッションへ配るための登録簿。
/// 値は (登録した接続の conn_id, 通知先チャネル)。UDP ソケットは全接続で共有のため、複数の
/// セッションが個別に `recv_from` すると他セッション宛のパケットを横取りしうる。これを避け、
/// 受信は [`spawn_hello_dispatcher`] の 1 タスクへ集約し、送信元 IP で該当セッションへ配る。
pub(crate) type HelloRegistry =
    Arc<std::sync::Mutex<HashMap<std::net::IpAddr, (u64, mpsc::UnboundedSender<SocketAddr>)>>>;

/// この接続を `expected_ip` の hello 通知先として登録する。同一 IP の別接続 (複数タブ等) が
/// 後から登録すると上書きされる (共有 UDP ソケットの構造上の既知の制約。既存の初回 hello 待ちも
/// 同じ IP 曖昧性を持っていた)。
///
/// **実害の筋道**: 上書き後、先に接続していたタブ (旧登録) が UDP hello を自己修復のために
/// 再送すると、その AURHELLO は新登録 (後発タブ) の通知先チャネルへ配られる。後発タブは既に
/// peer を確定済みのため、届いた通知を無視するだけで何も起きない。結果、先発タブは
/// hello を再送しても [`wait_udp_hello`] のタイムアウトまで永久に対向が確定しない
/// (＝復旧できない)。マルチタブは本プロジェクトの主要機能のため、実際に踏みうるシナリオである。
///
/// **これは見落としではなく据え置きの判断**。直すには `HostInfo` に接続トークンを追加し、
/// client の AURHELLO にそのトークンを載せ、host が IP ではなくトークンで登録を引く必要がある
/// (プロトコル変更が必要なため今回のスコープ外)。
pub(crate) fn register_hello(
    registry: &HelloRegistry,
    expected_ip: std::net::IpAddr,
    conn_id: u64,
) -> mpsc::UnboundedReceiver<SocketAddr> {
    let (tx, rx) = mpsc::unbounded_channel();
    registry
        .lock()
        .expect("hello registry poisoned")
        .insert(expected_ip, (conn_id, tx));
    rx
}

/// セッション終了時に登録を外す。自分の登録 (conn_id が一致) のときだけ除去し、後から
/// 同じ IP で登録し直した別接続の枠を誤って消さないようにする。
pub(crate) fn unregister_hello(
    registry: &HelloRegistry,
    expected_ip: std::net::IpAddr,
    conn_id: u64,
) {
    let mut g = registry.lock().expect("hello registry poisoned");
    if g.get(&expected_ip).is_some_and(|(id, _)| *id == conn_id) {
        g.remove(&expected_ip);
    }
}

/// 全クライアント共有の UDP ソケットから "AURHELLO" を集約して読み、送信元 IP で登録済みの
/// セッションへ配る。bind ごとに 1 つ起動し、rebind (ポート変更) 時は呼び出し側が abort する。
pub(crate) fn spawn_hello_dispatcher(
    udp: Arc<UdpSocket>,
    registry: HelloRegistry,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            let (n, src) = match udp.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!(error = %e, "UDP recv error (hello dispatcher) ignored");
                    continue;
                }
            };
            if &buf[..n] != b"AURHELLO" {
                tracing::debug!(?src, n, "unexpected UDP probe");
                continue;
            }
            let hit = registry
                .lock()
                .expect("hello registry poisoned")
                .get(&src.ip())
                .map(|(_, tx)| tx.clone());
            match hit {
                Some(tx) => {
                    let _ = tx.send(src);
                }
                None => tracing::debug!(?src, "AURHELLO from unregistered source ignored"),
            }
        }
    })
}

/// クライアントの UDP "AURHELLO" を待ち、対向アドレスを確定する (初回のみ)。
///
/// hello は単発 UDP のためロスしうる。クライアントは映像受信が始まるまで再送するので、
/// ここでは受信に上限を設けるだけにし、長時間 hello が来なければ Err を返してセッションを
/// 諦める (無限ブロックで accept ループまで止めない安全弁)。送信元 IP の妥当性は
/// [`spawn_hello_dispatcher`] が登録簿の IP 一致で担保する (ハンドシェイクを完了した TCP ピアの
/// IP でのみ登録するため、別 IP からの hello はそもそもこの `rx` に届かない)。
pub(crate) async fn wait_udp_hello(
    rx: &mut mpsc::UnboundedReceiver<SocketAddr>,
) -> Result<SocketAddr> {
    use std::time::Duration;
    tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .context("timed out waiting for UDP hello")?
        .ok_or_else(|| anyhow::anyhow!("hello registry channel closed"))
}
