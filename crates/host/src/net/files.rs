//! ファイル転送のホスト側 I/O (クライアント→ホストのアップロード受信、ホスト→クライアントのダウンロード送出)。
use super::*;

/// 受信中のファイルアップロード状態 (クライアント → ホスト)。1 セッション 1 転送のみ。
pub(crate) struct UploadState {
    pub(crate) id: u32,
    pub(crate) file: tokio::fs::File,
    pub(crate) written: u64,
    pub(crate) total: u64,
    pub(crate) path: String,
}

/// 操作対象ユーザーの `%USERPROFILE%\Aurora\received` 配下に保存先ファイルを作る。ファイル名の
/// 安全化は client `open_download` と共通の [`aurora_common::fsname::create_incoming_file_at`]
/// (防御の対称性はそちらで担保)。SYSTEM サービス配下ではログオンユーザーとして作る
/// ([`crate::system::user::as_session_user`]、サインイン前は拒否)。
pub(crate) async fn open_upload(name: &str) -> Result<(tokio::fs::File, String)> {
    let name = name.to_string();
    let (file, path) = tokio::task::spawn_blocking(move || {
        crate::system::user::as_session_user(|profile| {
            aurora_common::fsname::create_incoming_file_at(profile, "received", &name, "upload.bin")
        })
    })
    .await?
    .context("create upload file under %USERPROFILE%\\Aurora\\received")?;
    Ok((tokio::fs::File::from_std(file), path))
}

/// 要求されたファイルを開き、FileOffer + FileChunk 列を `out_tx` 経由でクライアントへ送る。
/// 読み取りエラーは FileResult(ok=false) で通知する。成功完了は last=true のチャンクで示す
/// (受信側がローカル保存の成否を判断するため、成功時の FileResult はホストからは送らない)。
pub(crate) async fn stream_file_to_client(id: u32, path: &str, out_tx: mpsc::Sender<ControlMsg>) {
    const CHUNK: usize = 32 * 1024;

    // 開く (= アクセス判定) だけを操作対象ユーザーとして行う。SYSTEM サービス配下で SYSTEM の
    // まま開くと、利用者が読めないファイルまでクライアントへ出せてしまう。
    let owned = path.to_string();
    let opened = tokio::task::spawn_blocking(move || {
        crate::system::user::as_session_user(|_| std::fs::File::open(&owned))
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)));
    let mut file = match opened.map(tokio::fs::File::from_std) {
        Ok(f) => f,
        Err(e) => {
            let _ = out_tx
                .send(ControlMsg::FileResult {
                    id,
                    ok: false,
                    message: format!("open failed: {e}"),
                })
                .await;
            return;
        }
    };
    // size=0 を誤って告知すると受信側が「告知サイズ超過」で初回チャンクから拒否して
    // しまうため、metadata 失敗は転送エラーとして通知する。
    let size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            let _ = out_tx
                .send(ControlMsg::FileResult {
                    id,
                    ok: false,
                    message: format!("metadata failed: {e}"),
                })
                .await;
            return;
        }
    };
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin")
        .to_string();

    if out_tx
        .send(ControlMsg::FileOffer { id, name, size })
        .await
        .is_err()
    {
        return;
    }

    let mut offset: u64 = 0;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                let _ = out_tx
                    .send(ControlMsg::FileResult {
                        id,
                        ok: false,
                        message: format!("read failed: {e}"),
                    })
                    .await;
                return;
            }
        };
        let last = n == 0 || offset + n as u64 >= size;
        if out_tx
            .send(ControlMsg::FileChunk {
                id,
                offset,
                data: buf[..n].to_vec(),
                last,
            })
            .await
            .is_err()
        {
            return;
        }
        offset += n as u64;
        if last {
            break;
        }
    }
    tracing::info!(id, size, "file download streamed");
}
