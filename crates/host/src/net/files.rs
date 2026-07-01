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

/// アップロード受信ディレクトリ (`%USERPROFILE%\Aurora\received`)。
///
/// トレイ常駐 + ログオン時自動起動だと CWD が `System32` 等になり得るため、CWD 相対では
/// 保存先が予測不能になる。ユーザープロファイル配下の固定パスに絶対化して discoverable にする。
/// `USERPROFILE` (非 Windows では `HOME`) が無ければ最後の手段としてカレントディレクトリを使う。
fn upload_dir() -> std::path::PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("Aurora").join("received")
}

/// `%USERPROFILE%\Aurora\received` 配下に保存先ファイルを作る。
/// ファイル名はパス区切りを除去して安全化する。
pub(crate) async fn open_upload(name: &str) -> Result<(tokio::fs::File, String)> {
    let dir = upload_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .context("create aurora-received dir")?;
    // パス区切り・ADS (`:`) を中和し、ファイル名成分のみを採用する (client open_download と対称)。
    let safe = aurora_common::fsname::sanitize_filename(name, "upload.bin");
    let path = dir.join(&safe);
    let file = tokio::fs::File::create(&path)
        .await
        .with_context(|| format!("create {}", path.display()))?;
    Ok((file, path.display().to_string()))
}

/// 要求されたファイルを開き、FileOffer + FileChunk 列を `out_tx` 経由でクライアントへ送る。
/// 読み取りエラーは FileResult(ok=false) で通知する。成功完了は last=true のチャンクで示す
/// (受信側がローカル保存の成否を判断するため、成功時の FileResult はホストからは送らない)。
pub(crate) async fn stream_file_to_client(id: u32, path: &str, out_tx: mpsc::Sender<ControlMsg>) {
    const CHUNK: usize = 32 * 1024;

    let mut file = match tokio::fs::File::open(path).await {
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
