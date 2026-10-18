//! ホスト到達性プローブ。launcher のカードにオンライン/オフラインのドットを出すため、
//! 制御ポートへの TCP 接続可否だけを（ハンドシェイクせず）確かめる。
use super::*;

/// プローブ対象 (launcher の保存ホスト 1 件分)。
#[derive(serde::Deserialize)]
pub struct ProbeTarget {
    pub host: String,
    pub port: u16,
}

/// プローブ結果。`host` は要求値をそのまま返す (launcher が突き合わせに使う)。
#[derive(Clone, serde::Serialize)]
pub struct ProbeResult {
    pub host: String,
    pub online: bool,
}

/// Tailnet 越しの応答遅延を考慮しつつ、UI を待たせすぎない上限。
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// 各対象へ並列に TCP 接続を試み、到達可否を返す。接続はすぐ閉じる (ハンドシェイクしない)。
#[tauri::command]
pub async fn probe_hosts(targets: Vec<ProbeTarget>) -> Result<Vec<ProbeResult>, String> {
    let handles: Vec<_> = targets
        .into_iter()
        .map(|t| {
            tauri::async_runtime::spawn(async move {
                let online = tokio::time::timeout(
                    PROBE_TIMEOUT,
                    TcpStream::connect((t.host.as_str(), t.port)),
                )
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
                ProbeResult {
                    host: t.host,
                    online,
                }
            })
        })
        .collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(r) = h.await {
            out.push(r);
        }
    }
    Ok(out)
}
