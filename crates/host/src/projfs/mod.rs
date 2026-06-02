//! ProjFS によるドライブリダイレクト (RDP 風)。
//!
//! クライアントの共有フォルダをホストの仮想フォルダ (`ddd-remote/`) として投影し、
//! ホストの Explorer からそのまま閲覧・コピーできるようにする (読み取り方向のみ)。
//!
//! ProjFS のコールバックは ProjFS 所有の同期スレッドから呼ばれるため、tokio の制御
//! チャネルとは [`FsBridge`] が相関 ID 付き RPC で橋渡しする。ブロッキング待機は
//! ProjFS スレッド上で完結し、tokio タスクはブロックしない。
//!
//! Windows 以外は ProjFS 相当が無いため no-op (投影なし)。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ddd_common::proto::{ControlMsg, FsEntry};
use tokio::sync::mpsc::UnboundedSender;

#[cfg(windows)]
mod windows;

/// 同期コールバックへ返す FS RPC 応答。
pub enum FsReply {
    List {
        entries: Vec<FsEntry>,
        done: bool,
        error: Option<String>,
    },
    Read {
        data: Vec<u8>,
        error: Option<String>,
    },
}

/// RPC のタイムアウト。Tailnet 往復 + クライアントのディスク I/O を見込む。
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// ディレクトリ一覧の短期キャッシュ TTL。Explorer の連続コールバックの往復を抑える。
const DIR_CACHE_TTL: Duration = Duration::from_secs(2);

/// ProjFS 同期コールバック ↔ tokio net ループの橋渡し。
pub struct FsBridge {
    /// コールバック → net ループへ要求を投入する。`send` は非同期でないので同期スレッドから安全。
    req_tx: UnboundedSender<ControlMsg>,
    /// 相関 ID → 返信先 (同期チャネル)。net ループが [`FsBridge::complete`] で発火する。
    pending: Mutex<HashMap<u64, std::sync::mpsc::Sender<FsReply>>>,
    next_id: AtomicU64,
    /// ディレクトリ一覧の短期キャッシュ (相対パス → (取得時刻, エントリ))。
    dir_cache: Mutex<HashMap<String, (Instant, Vec<FsEntry>)>>,
}

impl FsBridge {
    pub fn new(req_tx: UnboundedSender<ControlMsg>) -> Self {
        Self {
            req_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            dir_cache: Mutex::new(HashMap::new()),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 一方向メッセージ (書き戻し: FsCreate/FsWrite/FsDelete/FsRename) を net ループへ送る。
    /// 応答は待たない (ベストエフォート)。
    pub fn push(&self, msg: ControlMsg) -> bool {
        self.req_tx.send(msg).is_ok()
    }

    /// net ループが返信受信時に呼ぶ。pending から相関 ID を引いて発火する。
    pub fn complete(&self, req_id: u64, reply: FsReply) {
        if let Ok(mut map) = self.pending.lock() {
            if let Some(tx) = map.remove(&req_id) {
                let _ = tx.send(reply);
            }
        }
    }

    /// 1 件の RPC を発行し、応答 (または timeout) を同期待ちする。
    fn call(&self, make: impl FnOnce(u64) -> ControlMsg) -> Result<FsReply, String> {
        let id = self.next_id();
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| "bridge poisoned".to_string())?
            .insert(id, tx);
        if self.req_tx.send(make(id)).is_err() {
            self.pending.lock().ok().and_then(|mut m| m.remove(&id));
            return Err("control channel closed".into());
        }
        match rx.recv_timeout(RPC_TIMEOUT) {
            Ok(r) => Ok(r),
            Err(_) => {
                self.pending.lock().ok().and_then(|mut m| m.remove(&id));
                Err("RPC timeout".into())
            }
        }
    }

    /// 共有ルート相対 `path` のディレクトリ全体を取得する (ページングを集約 + 短期キャッシュ)。
    pub fn list_all(&self, path: &str) -> Result<Vec<FsEntry>, String> {
        if let Ok(cache) = self.dir_cache.lock() {
            if let Some((at, entries)) = cache.get(path) {
                if at.elapsed() < DIR_CACHE_TTL {
                    return Ok(entries.clone());
                }
            }
        }

        let mut all = Vec::new();
        let mut offset = 0u32;
        loop {
            let reply = self.call(|req_id| ControlMsg::FsList {
                req_id,
                path: path.to_string(),
                offset,
            })?;
            let FsReply::List {
                entries,
                done,
                error,
            } = reply
            else {
                return Err("unexpected reply to FsList".into());
            };
            if let Some(e) = error {
                return Err(e);
            }
            let n = entries.len() as u32;
            all.extend(entries);
            if done || n == 0 {
                break;
            }
            offset += n;
        }

        if let Ok(mut cache) = self.dir_cache.lock() {
            cache.insert(path.to_string(), (Instant::now(), all.clone()));
        }
        Ok(all)
    }

    /// 共有ルート相対 `path` のファイルを `offset` から最大 `length` バイト読む。
    /// 空 Vec は EOF を意味する。
    pub fn read(&self, path: &str, offset: u64, length: u32) -> Result<Vec<u8>, String> {
        let reply = self.call(|req_id| ControlMsg::FsRead {
            req_id,
            path: path.to_string(),
            offset,
            length,
        })?;
        match reply {
            FsReply::Read { data, error } => match error {
                Some(e) => Err(e),
                None => Ok(data),
            },
            FsReply::List { .. } => Err("unexpected reply to FsRead".into()),
        }
    }
}

/// 投影セッション。Drop で仮想化を停止する (CLAUDE.md: OS リソースは Drop 解放)。
pub struct Virtualization {
    #[cfg(windows)]
    _inner: windows::WinVirtualization,
}

/// `root` をクライアント共有フォルダの投影先として仮想化を開始する。
#[cfg(windows)]
pub fn start(bridge: std::sync::Arc<FsBridge>, root: &Path) -> anyhow::Result<Virtualization> {
    Ok(Virtualization {
        _inner: windows::start(bridge, root)?,
    })
}

#[cfg(not(windows))]
pub fn start(_bridge: std::sync::Arc<FsBridge>, _root: &Path) -> anyhow::Result<Virtualization> {
    tracing::info!("ProjFS is Windows-only; drive redirection disabled on this OS");
    Ok(Virtualization {})
}
