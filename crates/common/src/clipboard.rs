//! クリップボード同期アクター。arboard はブロッキング API のため専用 OS スレッドで動かす。
//!
//! - `apply_tx`: リモートから受信したテキストをローカルクリップボードへ書き込む
//! - `change_rx`: ローカルクリップボードの変化を検知してテキストを流す (リモートへ送る用)
//!
//! ループバック抑止: 最後に観測 / 書き込んだテキストを保持し、同一なら send/apply とも何もしない。
//!
//! Android/iOS は arboard が NDK クロスコンパイルに対応しないため、`imp` を
//! デスクトップ実装とモバイル no-op スタブに分ける。公開 API (`ClipboardHandle`/`spawn`) は
//! 完全に同一で、呼び出し側 (host/client 双方) は cfg 分岐せずに使える。
//!
//! `common` crate は tauri-build を経由しないため `cfg(desktop)`/`cfg(mobile)` は使えず、
//! Cargo.toml の target セレクタと揃えて `target_os` で直接分岐する。

pub use imp::{ClipboardHandle, spawn};

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use tokio::sync::mpsc;

    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    /// 同期する最大テキスト長 (バイト)。これを超えるものは無視する。
    ///
    /// 制御メッセージは TCP フレーム (長さ u16 = 最大 65535B) に収める必要がある。
    /// `ControlMsg::Clipboard{text}` は text 本体に加え bincode の enum タグ・文字列長
    /// プレフィックス・Noise AEAD タグ (16B) が乗るため、これらのオーバーヘッドを引いた
    /// 安全側の 60KiB を上限にする。これより大きいと送信がフレーム超過で失敗し、host/client
    /// とも `?` でセッションごと切断していた (長文コピーで自爆する可用性バグ)。超過分は
    /// 黙って無視し、同期しないだけで接続は維持する。
    const MAX_TEXT_BYTES: usize = 60 * 1024;

    /// クリップボードアクターのハンドル。`apply_tx` は複数所有者で送れるよう Clone 可能。
    /// 別途返される `change_rx` を読むのは単一の所有者。
    #[derive(Clone)]
    pub struct ClipboardHandle {
        pub apply_tx: mpsc::UnboundedSender<String>,
        shutdown: Arc<AtomicBool>,
    }

    impl ClipboardHandle {
        pub fn shutdown(&self) {
            self.shutdown.store(true, Ordering::Relaxed);
        }
    }

    pub fn spawn() -> (ClipboardHandle, mpsc::UnboundedReceiver<String>) {
        let shutdown = Arc::new(AtomicBool::new(false));
        let (apply_tx, mut apply_rx) = mpsc::unbounded_channel::<String>();
        let (change_tx, change_rx) = mpsc::unbounded_channel::<String>();

        let sd = shutdown.clone();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let mut cb = match arboard::Clipboard::new() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "clipboard unavailable");
                        return;
                    }
                };
                // 起動時の値を「最後に観測したテキスト」として記録 (初回ポーリングで発火させない)
                let mut last_text: Option<String> = cb.get_text().ok();

                while !sd.load(Ordering::Relaxed) {
                    // 1) リモートからの apply を吸う (キューにある分すべて)
                    while let Ok(text) = apply_rx.try_recv() {
                        if text.len() > MAX_TEXT_BYTES {
                            tracing::debug!(len = text.len(), "clipboard text too large, skip");
                            continue;
                        }
                        if Some(&text) == last_text.as_ref() {
                            continue;
                        }
                        match cb.set_text(text.clone()) {
                            Ok(()) => last_text = Some(text),
                            Err(e) => tracing::debug!(error = %e, "clipboard set failed"),
                        }
                    }

                    // 2) ローカル変化のポーリング
                    if let Ok(text) = cb.get_text()
                        && text.len() <= MAX_TEXT_BYTES
                        && Some(&text) != last_text.as_ref()
                    {
                        last_text = Some(text.clone());
                        if change_tx.send(text).is_err() {
                            return;
                        }
                    }

                    std::thread::sleep(POLL_INTERVAL);
                }
            })
            .expect("spawn clipboard thread");

        (ClipboardHandle { apply_tx, shutdown }, change_rx)
    }
}

/// Android/iOS 向け no-op スタブ。クリップボード同期はフェーズ2 (モバイル対応後続) のため、
/// 呼び出し側 (host/client) の networking/セッション管理を変更せずに済むよう、デスクトップと
/// 同一 API だけ提供し実処理はしない。
#[cfg(any(target_os = "android", target_os = "ios"))]
mod imp {
    use tokio::sync::mpsc;

    /// クリップボードアクターのハンドル (no-op)。`shutdown` は何もしない。
    #[derive(Clone)]
    pub struct ClipboardHandle {
        pub apply_tx: mpsc::UnboundedSender<String>,
    }

    impl ClipboardHandle {
        pub fn shutdown(&self) {}
    }

    /// 何も監視しない `change_rx` を返す (送信端を保持したまま放置し、リモート変化の通知は
    /// 永久に来ない = ローカル→リモート同期なし)。
    pub fn spawn() -> (ClipboardHandle, mpsc::UnboundedReceiver<String>) {
        let (apply_tx, _apply_rx) = mpsc::unbounded_channel::<String>();
        let (_change_tx, change_rx) = mpsc::unbounded_channel::<String>();
        (ClipboardHandle { apply_tx }, change_rx)
    }
}
