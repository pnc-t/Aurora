//! 映像パイプライン (画面取り込み → エンコード) を別プロセス (取り込みワーカー) で動かす。
//!
//! - `supervisor`: 本体側。ワーカーを起動・監視し、net との口を中継する ([`start`])。
//! - `worker`: ワーカー側 (`aurora-host --capture-worker`)。capture/encode スレッドを動かす。
//! - `wire`: 両者の間のメッセージと枠付け。
//!
//! 分けている理由: UAC/ロック画面とのデスクトップ切替や HDR/ACM 機のロック遷移で、DXGI Desktop
//! Duplication はプロセス単位で壊れ、プロセスを作り直すまで戻らない (実機確認済み)。取り込みが
//! 本体と同じプロセスだと作り直すたびに TCP/UDP も切れ、クライアントが再接続を繰り返す。
//! ワーカーだけを作り直せば、通信・入力・音声・トレイは止まらず、映像が 1 秒前後止まるだけで済む。

mod supervisor;
mod wire;
mod worker;

pub(crate) use supervisor::*;
pub(crate) use worker::{request_takeover_restore, run as run_worker};

/// 本体がワーカーを起動するときの引数。
pub const WORKER_ARG: &str = "--capture-worker";
/// ワーカーが「取り込みが固着したので作り直してほしい」で終了するときの終了コード。
pub const WORKER_RESTART_EXIT: i32 = 3;
