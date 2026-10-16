//! トレイ操作の本体 (UI から切り離したロジック)。
//!
//! トレイ UI は「要求 ([`TrayRequest`]) を投げて応答 ([`TrayReply`]) を表示する」だけにし、
//! 状態の読み書きと特権ガード (接続中拒否・現在パスワード照合) は全てここ ([`handle`]) で行う。
//! 開発実行・旧 ONLOGON 運用では同一プロセス内の [`LocalControl`] がそのまま呼び、SYSTEM
//! サービス運用ではユーザー権限のトレイプロセスが名前付きパイプ越しに本体へ送る
//! ([`super::ipc`])。どちらの経路でもガードはこの 1 箇所を通るので、経路ごとに防御が
//! 食い違わない (memory: host-privileged-paths-symmetry)。
//!
//! **PSK はトレイへ渡さない**: 現在パスワードの照合も本体側で行う。ユーザー権限のトレイに
//! PSK を渡すと、同じ PC の他プロセスがパイプ経由で取り出して遠隔認証に使えてしまう。

use std::sync::atomic::Ordering;

use aurora_common::noise::psk_from_password;
use serde::{Deserialize, Serialize};

use super::{
    Reload, SharedAuthFailures, SharedDevices, SharedHasPassword, SharedPort, SharedPsk,
    SharedTrayBus, TrayEvent,
};
use crate::update::SharedUpdate;

/// トレイ操作が触る本体側の共有状態一式。
#[derive(Clone)]
pub struct TrayContext {
    pub psk: SharedPsk,
    pub has_password: SharedHasPassword,
    pub port: SharedPort,
    pub reload: Reload,
    pub auth_failures: SharedAuthFailures,
    pub devices: SharedDevices,
    pub bus: SharedTrayBus,
    pub update: SharedUpdate,
}

/// 承認待ち端末 1 件 (表示用 + 承認時に鍵で対象を確定するための公開鍵)。
#[derive(Clone, Serialize, Deserialize)]
pub struct PendingInfo {
    pub pubkey: Vec<u8>,
    pub fingerprint: String,
    pub label: String,
}

/// トレイの表示に要る状態のスナップショット。1 秒周期のポーリングで取得する。
#[derive(Clone, Serialize, Deserialize)]
pub struct TrayStatus {
    pub connected: usize,
    pub last_peer: Option<String>,
    pub port: u16,
    pub auth_failures: u32,
    /// 利用可能な新バージョン (あれば)。
    pub update_version: Option<String>,
    /// 実パスワードが設定済みか (変更時に現在パスワードを求めるか)。
    pub has_password: bool,
    pub pinning_enabled: bool,
    pub pairing_armed: bool,
    pub pending: Vec<PendingInfo>,
    pub device_labels: Vec<String>,
    /// 前回の取得以降に溜まった通知イベント (取得で消費される)。
    pub events: Vec<TrayEvent>,
}

/// トレイから本体への操作要求。
#[derive(Serialize, Deserialize)]
pub enum TrayRequest {
    Status,
    /// 未設定 (`has_password == false`) のときだけ `current` を省略できる。
    ChangePassword {
        current: Option<String>,
        new: String,
    },
    ChangePort(u16),
    SetPinning(bool),
    ArmPairing,
    DisarmPairing,
    /// `label` はトレイが表示していた値。一覧が変わっていたら削除しない (取り違え防止)。
    RemoveDevice {
        index: usize,
        label: String,
    },
    ApprovePending {
        pubkey: Vec<u8>,
    },
    ClearDevices,
    RequestUpdate,
}

/// 操作要求への応答。`Done`/`Failed` の文字列はそのまま利用者に表示する。
#[derive(Serialize, Deserialize)]
pub enum TrayReply {
    Status(Box<TrayStatus>),
    Done(String),
    Failed(String),
}

/// トレイ UI が操作を投げる先。同一プロセス ([`LocalControl`]) かパイプ越し ([`super::ipc`]) か。
pub trait TrayControl {
    fn call(&self, req: TrayRequest) -> TrayReply;
}

/// 同一プロセス内の本体を直接操作する経路 (開発実行・旧 ONLOGON 運用)。
pub struct LocalControl(pub TrayContext);

impl TrayControl for LocalControl {
    fn call(&self, req: TrayRequest) -> TrayReply {
        handle(&self.0, req)
    }
}

const BUSY: &str = "クライアント接続中は変更できません。\n切断してから操作してください。";

/// 要求を処理する。ファイル書き込み (パスワード・設定・許可リスト) を伴うので、tokio タスクから
/// 呼ぶときは `spawn_blocking` 経由にすること。
pub fn handle(ctx: &TrayContext, req: TrayRequest) -> TrayReply {
    use TrayReply::{Done, Failed};
    match req {
        TrayRequest::Status => TrayReply::Status(Box::new(status(ctx))),
        TrayRequest::ChangePassword { current, new } => {
            // 接続中はリモート操作者による締め出し/乗っ取りを防ぐため変更を拒否する。
            if crate::net::is_active() {
                return Failed(BUSY.into());
            }
            if ctx.has_password.load(Ordering::Relaxed) {
                let expected = ctx.psk.lock().ok().map(|g| *g);
                let given = current.as_deref().map(psk_from_password);
                if expected.is_none() || given != expected {
                    return Failed("現在のパスワードが違います。".into());
                }
            }
            if new.chars().count() < aurora_common::noise::MIN_PASSWORD_LEN {
                return Failed(format!(
                    "パスワードは{}文字以上で入力してください。",
                    aurora_common::noise::MIN_PASSWORD_LEN
                ));
            }
            if let Err(e) = crate::security::save_password(&new) {
                return Failed(format!("保存に失敗しました: {e}"));
            }
            if let Ok(mut g) = ctx.psk.lock() {
                *g = psk_from_password(&new);
            }
            ctx.has_password.store(true, Ordering::Relaxed);
            Done("パスワードを変更しました (次の接続から有効)。".into())
        }
        TrayRequest::ChangePort(new_port) => {
            // 接続中はソケット貼り直しで相手を切ってしまうため拒否する。
            if crate::net::is_active() {
                return Failed(BUSY.into());
            }
            if new_port == 0 {
                return Failed("1〜65535 のポート番号を入力してください。".into());
            }
            if new_port == ctx.port.load(Ordering::Relaxed) {
                return Done(format!("待受ポートは既に {new_port} です。"));
            }
            // 新ポートで実際に bind できるか事前確認する。net 側の貼り直しが失敗すると host
            // ごと終了してしまうため、使用中ポート等は保存前にここで弾く。
            if let Err(e) = port_available(new_port) {
                return Failed(format!("ポート {new_port} は使用できません: {e}"));
            }
            if let Err(e) = crate::system::save_port(new_port) {
                return Failed(format!("保存に失敗しました: {e}"));
            }
            ctx.port.store(new_port, Ordering::Relaxed);
            // accept ループを起こして TCP/UDP を新ポートで貼り直させる。
            ctx.reload.notify_one();
            Done(format!("待受ポートを {new_port} に変更しました。"))
        }
        TrayRequest::SetPinning(on) => {
            // 無効化は接続中拒否: 接続中の操作者が入力注入でトレイを操作して 2 要素目を外すのを防ぐ。
            // 有効化は保護を強める方向なので接続中でも許可する。
            if !on && crate::net::is_active() {
                return Failed(BUSY.into());
            }
            ctx.devices.set_enabled(on);
            Done(if on {
                "デバイスピン留めを有効化しました。\n登録済みの端末のみ接続できます。\n新しい端末は「次の接続を登録」で追加してください。".into()
            } else {
                "デバイスピン留めを無効化しました (パスワードのみで接続可)。".into()
            })
        }
        TrayRequest::ArmPairing => {
            // 接続中の操作者が自分の別端末を信頼登録して永続アクセスを得るのを防ぐ。
            if crate::net::is_active() {
                return Failed(BUSY.into());
            }
            ctx.devices.arm_pairing();
            Done("ペアリング窓を開きました。\n登録したい端末から接続してください。\n接続すると、その端末の端末IDがこのメニューの「承認: ...」項目に表示されます。\n端末側に表示された端末IDと一致することを確認して承認してください。\n(一致しない項目は承認しないでください。別端末の可能性があります)".into())
        }
        TrayRequest::DisarmPairing => {
            // 取消は保護を弱めないので接続中でも許可する。
            ctx.devices.disarm_pairing();
            Done("ペアリング窓を閉じました (登録待機を取り消しました)。".into())
        }
        TrayRequest::RemoveDevice { index, label } => {
            if crate::net::is_active() {
                return Failed(BUSY.into());
            }
            if ctx.devices.labels().get(index) != Some(&label) {
                return Failed("削除できませんでした (一覧が変わった可能性)。".into());
            }
            match ctx.devices.remove(index) {
                Some(removed) => Done(format!("「{removed}」を削除しました。")),
                None => Failed("削除できませんでした (一覧が変わった可能性)。".into()),
            }
        }
        TrayRequest::ApprovePending { pubkey } => {
            // 接続中拒否ガードは付けない: 承認待ち端末は既に拒否済みで接続中ではないのが通常で、
            // 再試行中の is_active フラップで承認できなくなる。承認は鍵で対象を確定する (TOCTOU 対策)。
            match ctx.devices.approve_pending(&pubkey) {
                Some(label) => Done(format!("「{label}」を信頼登録しました。")),
                None => Failed("承認に失敗しました (一覧が変わった可能性)。".into()),
            }
        }
        TrayRequest::ClearDevices => {
            if crate::net::is_active() {
                return Failed(BUSY.into());
            }
            ctx.devices.clear();
            Done("登録端末をすべて削除しました。".into())
        }
        TrayRequest::RequestUpdate => {
            let Some(av) = ctx.update.available() else {
                return Failed("利用できる更新はありません。".into());
            };
            ctx.update.request_apply();
            Done(if crate::net::is_active() {
                format!(
                    "クライアント接続中のため、切断後に v{} へ更新します。\n準備ができ次第、自動でダウンロード・再起動します。",
                    av.version
                )
            } else {
                format!(
                    "v{} をダウンロードして更新します。\n完了すると Aurora Host が自動的に再起動します。",
                    av.version
                )
            })
        }
    }
}

fn status(ctx: &TrayContext) -> TrayStatus {
    TrayStatus {
        connected: ctx.bus.connected_count(),
        last_peer: ctx.bus.last_peer(),
        port: ctx.port.load(Ordering::Relaxed),
        auth_failures: ctx.auth_failures.load(Ordering::Relaxed),
        update_version: ctx.update.available().map(|a| a.version),
        has_password: ctx.has_password.load(Ordering::Relaxed),
        pinning_enabled: ctx.devices.enabled(),
        pairing_armed: ctx.devices.pairing_armed(),
        pending: ctx
            .devices
            .pending_devices()
            .into_iter()
            .map(|(pubkey, fingerprint, label)| PendingInfo {
                pubkey,
                fingerprint,
                label,
            })
            .collect(),
        device_labels: ctx.devices.labels(),
        events: ctx.bus.drain_events(),
    }
}

/// 新ポートで TCP/UDP を実際に bind できるか事前確認する。確認後ソケットは drop で解放され、
/// 直後に net 側が同ポートへ貼り直す (単一ユーザー host のため間の競合は実質起きない)。
fn port_available(port: u16) -> Result<(), String> {
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let _tcp = TcpListener::bind(addr).map_err(|e| format!("TCP {e}"))?;
    let _udp = UdpSocket::bind(addr).map_err(|e| format!("UDP {e}"))?;
    Ok(())
}
