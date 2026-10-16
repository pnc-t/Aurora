//! システムトレイ (接続状態の表示・パスワード/ポート変更・端末承認)。
//!
//! - [`TrayBus`]: net/encode/update → トレイへの状態・通知の受け渡し (全 OS)。
//! - [`control`]: トレイ操作のロジックと特権ガード (UI から独立)。
//! - `ui` (Windows): Win32 のトレイアイコン・メニュー・ダイアログ。
//! - [`ipc`] (Windows): SYSTEM サービス運用で、ユーザー権限のトレイ (`--tray`) と本体を
//!   名前付きパイプでつなぐ。
//!
//! CLI (`--set-password` / `--clear-password`) からの変更は**別プロセス**のため、本体の
//! 共有 PSK には届かない。稼働中ホストがある場合は再起動するまで反映されない (M-5)。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// host 全体で共有する接続 PSK。net 層が接続受理時に読み、tray がパスワード変更時に書く。
pub type SharedPsk = Arc<Mutex<[u8; 32]>>;

/// 実パスワードが設定済みかを共有するフラグ。設定済みなら変更時に現在パスワードを要求する。
pub type SharedHasPassword = Arc<AtomicBool>;

/// host 全体で共有する待受ポート。tray がポート変更で書き、net が reload 後に読み直す。
pub type SharedPort = Arc<AtomicU16>;

/// 認証失敗の累計カウンタ。net が接続受理で加算し、tray がメニューに表示する。
pub type SharedAuthFailures = Arc<AtomicU32>;

/// デバイスピン留めの実行時状態。net が照合し、tray が有効化/ペアリング/削除を操作する。
pub type SharedDevices = Arc<crate::security::DeviceGuard>;

/// ポート変更を net の accept ループへ伝える反映トリガ (`notify_one` で TCP/UDP 貼り直し)。
pub type Reload = Arc<tokio::sync::Notify>;

/// tray が `TrayBus` を共有して受け取るための別名。
pub type SharedTrayBus = Arc<TrayBus>;

/// net→tray へ「何が起きたか」を 1 件ずつ伝える通知イベント。tray が `WM_TIMER` で drain して
/// バルーン通知に変換する。文字列ラベルは表示用に整形済み (端末名 or peer IP)。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum TrayEvent {
    /// クライアント確立 (UDP hello 完了)。
    Connected(String),
    /// クライアント切断。
    Disconnected(String),
    /// ペアリング窓中に未知端末が接続し承認待ちになった (指紋 + 端末名)。
    PendingDevice { fingerprint: String, label: String },
    /// 新しいバージョンが利用可能になった (update スレッドが検知)。
    UpdateAvailable(String),
    /// エンコーダが `poll_nal` 失敗からの再構築リトライを使い果たし、映像配信が止まった。
    EncodeStalled,
}

/// 最大保持イベント数。あふれたら古いものを捨てる (常駐 UI なので無限に溜めない)。
const MAX_TRAY_EVENTS: usize = 32;

/// host の実行時ステータスと通知イベントを **net→tray で受け渡す共有バス**。
///
/// net (tokio タスク) が接続/切断/登録で更新し、tray (UI スレッド) が `WM_TIMER` でポーリングして
/// ツールチップ/メニュー/バルーン通知へ反映する。クロススレッドの `PostMessage` や `HWND` 共有を
/// 避け、本プロジェクトの actor 風 (メッセージ受け渡し) に倣って結合を切るための仕組み。
/// 中身はプレーンな同期プリミティブのみで Win32 に依存しないため、全 OS でコンパイルできる
/// (実体のある通知表示は Windows の tray 側だけ)。
pub struct TrayBus {
    /// 確立済みクライアント数。アイコン/ツールチップ/メニューの「接続中」表示に使う。
    connected: AtomicUsize,
    /// 直近に接続した端末の表示ラベル (ツールチップ/メニュー用)。全切断で `None` に戻す。
    last_peer: Mutex<Option<String>>,
    /// 未処理の通知イベント (bounded)。tray が drain する。
    events: Mutex<VecDeque<TrayEvent>>,
}

impl TrayBus {
    /// 空のバスを生成して共有ハンドルで返す。常に `Arc` 越しに使うため `Self` でなく共有別名を返す。
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> SharedTrayBus {
        Arc::new(Self {
            connected: AtomicUsize::new(0),
            last_peer: Mutex::new(None),
            events: Mutex::new(VecDeque::new()),
        })
    }

    /// 確立済みクライアント数。
    pub fn connected_count(&self) -> usize {
        self.connected.load(Ordering::Relaxed)
    }

    /// 直近に接続した端末ラベル (あれば)。
    pub fn last_peer(&self) -> Option<String> {
        self.last_peer.lock().ok().and_then(|p| p.clone())
    }

    /// クライアント確立を記録する (net が UDP hello 完了後に呼ぶ)。
    pub fn on_connected(&self, label: String) {
        self.connected.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut p) = self.last_peer.lock() {
            *p = Some(label.clone());
        }
        self.push(TrayEvent::Connected(label));
    }

    /// クライアント切断を記録する (net がセッション後始末で呼ぶ)。全切断で直近ラベルを消す。
    pub fn on_disconnected(&self, label: String) {
        // 飽和減算で 0 未満にしない (確立/切断は対で呼ぶが、防御的に floor する)。
        let prev = self
            .connected
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .unwrap_or(0);
        if prev <= 1
            && let Ok(mut p) = self.last_peer.lock()
        {
            *p = None;
        }
        self.push(TrayEvent::Disconnected(label));
    }

    /// ペアリング窓中に未知端末が接続し承認待ちになったことを記録する (net が呼ぶ)。
    pub fn on_pending_device(&self, fingerprint: String, label: String) {
        self.push(TrayEvent::PendingDevice { fingerprint, label });
    }

    /// 新しいバージョンが利用可能になったことを記録する (update スレッドが呼ぶ)。
    pub fn on_update_available(&self, version: String) {
        self.push(TrayEvent::UpdateAvailable(version));
    }

    /// エンコーダの再構築リトライが尽き、映像配信が止まったことを記録する (encode スレッドが呼ぶ)。
    /// TCP/UDP 自体は生きたままなので、無人運用で気付けるようこのバルーンが唯一の手がかりになる。
    pub fn on_encode_stalled(&self) {
        self.push(TrayEvent::EncodeStalled);
    }

    /// 溜まった通知イベントを取り出す (tray の `WM_TIMER` が呼ぶ)。
    pub fn drain_events(&self) -> Vec<TrayEvent> {
        self.events
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    fn push(&self, ev: TrayEvent) {
        if let Ok(mut q) = self.events.lock() {
            if q.len() >= MAX_TRAY_EVENTS {
                q.pop_front();
            }
            q.push_back(ev);
        }
    }
}

pub mod control;
#[cfg(target_os = "windows")]
pub mod ipc;
#[cfg(target_os = "windows")]
mod ui;

pub use control::{LocalControl, TrayContext, TrayControl, TrayReply, TrayRequest, TrayStatus};

/// 同一プロセスのトレイ UI を別スレッドで起動する (開発実行・旧 ONLOGON 運用)。SYSTEM サービス
/// 配下の本体はこれを呼ばず、[`ipc::serve`] でユーザー権限のトレイからの要求を受ける。
#[cfg(target_os = "windows")]
pub fn spawn(ctx: TrayContext) {
    let _ = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            if let Err(e) = ui::run(Box::new(LocalControl(ctx)), ui::TrayMode::InProcess) {
                tracing::warn!(error = %e, "tray UI disabled");
            }
        });
}

#[cfg(not(target_os = "windows"))]
pub fn spawn(_ctx: TrayContext) {}

/// `--tray`: ユーザー権限の単独トレイを動かす (サービスがログオンユーザーとして起動する)。
/// 同じセッションで二重に出さないよう名前付き Mutex で 1 つに絞る。
#[cfg(target_os = "windows")]
pub fn run_standalone() -> anyhow::Result<()> {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::w;

    // SAFETY: 名前付き Mutex を作成 (既存なら取得) するだけ。ハンドルはプロセス終了まで保持する
    // (Drop しない = 終了時に OS が閉じる)。初期所有はしない。
    let _guard = unsafe { CreateMutexW(None, false, w!("Local\\AuroraHostTray")) }?;
    // SAFETY: 直前の CreateMutexW のエラー状態を読むだけ。
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        tracing::info!("tray already running in this session; exiting");
        return Ok(());
    }
    ui::run(Box::new(ipc::PipeControl::new()), ui::TrayMode::Standalone)?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn run_standalone() -> anyhow::Result<()> {
    anyhow::bail!("--tray is only supported on Windows")
}
