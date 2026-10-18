//! 取り込みワーカー (`aurora-host --capture-worker`、本体が起動する子プロセス) 側。
//!
//! 画面取り込み (capture スレッド) とエンコード (encode スレッド) だけを動かし、本体とは
//! 標準入出力のパイプでやり取りする ([`super::wire`])。取り込みが回復不能に固着したら
//! (DWM へのプロセス単位の接続破損・UAC/ロック画面とのデスクトップ切替) プロセスごと終了し、
//! 本体に作り直させる。本体は TCP/UDP の接続を保ったままなので、クライアントは切断されない。
//!
//! 標準入力が閉じたら (= 本体が終了した) 自分も終了する。ジョブに入っていなくても孤児として
//! GPU やディスプレイを握り続けない。

use std::io::{BufReader, BufWriter, Write};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use super::wire::{FromWorker, ToWorker, read_msg, write_msg};
use crate::capture::Frame;
use crate::encode::EncodedNal;
use crate::net::{CodecStatus, EncoderCommand};

/// ワーカーの出力 (本体への送信) 口。capture スレッドからの依頼 ([`request_takeover_restore`]) にも使う。
static OUT: OnceLock<SyncSender<FromWorker>> = OnceLock::new();

/// `--capture-worker`: 本体から Init を受けて取り込み・エンコードを始め、終了するまで戻らない。
pub fn run() -> Result<()> {
    let mut input = BufReader::new(std::io::stdin());
    let Some(ToWorker::Init {
        display: target,
        fps_cap,
        max_bitrate_bps,
        encode_gpu,
        perf,
    }) = read_msg::<ToWorker>(&mut input)?
    else {
        bail!("capture worker: expected Init as the first message");
    };
    tracing::info!(display = %target, fps_cap, "capture worker starting");

    // 本体へ送る口。エンコード済み映像が詰まったら encode スレッドまで背圧を返す (有界)。
    let (out_tx, out_rx) = sync_channel::<FromWorker>(32);
    let _ = OUT.set(out_tx.clone());
    std::thread::Builder::new()
        .name("worker-out".into())
        .spawn(move || {
            let mut out = BufWriter::new(std::io::stdout());
            for msg in out_rx {
                if write_msg(&mut out, &msg)
                    .and_then(|()| Ok(out.flush()?))
                    .is_err()
                {
                    // 本体が居ない。続けても誰にも届かない。
                    std::process::exit(0);
                }
            }
        })?;

    // ハイブリッド GPU で内蔵パネルを複製できない環境は、GPU プリファレンスを省電力側にして
    // 作り直させる (プリファレンスはプロセス起動時にしか効かない)。
    #[cfg(windows)]
    if crate::switch_to_igpu_if_needed(&target) {
        std::process::exit(super::WORKER_RESTART_EXIT);
    }

    let device = crate::capture::create_shared_device(&target)?;
    #[cfg(target_os = "windows")]
    crate::align_vdd_render_gpu(&device.adapter_name);

    let (frame_tx, frame_rx) = mpsc::channel::<(Instant, Frame)>(2);
    let (nal_tx, mut nal_rx) = mpsc::channel::<EncodedNal>(16);
    let (enc_cmd_tx, enc_cmd_rx) = mpsc::channel::<EncoderCommand>(8);
    let (codec_tx, mut codec_rx) = mpsc::channel::<CodecStatus>(4);
    let (select_tx, select_rx) = std::sync::mpsc::channel::<String>();
    let (fps_tx, fps_rx) = std::sync::mpsc::channel::<u32>();
    let (scale_tx, scale_rx) = std::sync::mpsc::channel::<u32>();
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel::<bool>();
    let (res_tx, res_rx) = std::sync::mpsc::sync_channel::<(u32, u32)>(1);
    let force_frame = Arc::new(AtomicBool::new(false));

    // 本体からの指示を各スレッドの口へ配る。標準入力が閉じたら終了する。
    {
        let force_frame = force_frame.clone();
        std::thread::Builder::new()
            .name("worker-in".into())
            .spawn(move || {
                loop {
                    let msg = match read_msg::<ToWorker>(&mut input) {
                        Ok(Some(m)) => m,
                        Ok(None) | Err(_) => {
                            tracing::info!("host closed the worker pipe; exiting");
                            std::process::exit(0);
                        }
                    };
                    match msg {
                        ToWorker::Init { .. } => {}
                        ToWorker::Encoder(cmd) => {
                            let _ = enc_cmd_tx.blocking_send(cmd);
                        }
                        ToWorker::SelectDisplay(name) => {
                            let _ = select_tx.send(name);
                        }
                        ToWorker::Fps(f) => {
                            let _ = fps_tx.send(f);
                        }
                        ToWorker::Scale(h) => {
                            let _ = scale_tx.send(h);
                        }
                        ToWorker::Cursor(on) => {
                            let _ = cursor_tx.send(on);
                        }
                        ToWorker::ForceFrame => {
                            force_frame.store(true, std::sync::atomic::Ordering::Release);
                        }
                        ToWorker::Takeover(active) => {
                            crate::display::mirror_takeover_active(active)
                        }
                    }
                }
            })?;
    }

    let cap_device = device.clone();
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = crate::capture_loop(
                cap_device,
                frame_tx,
                res_tx,
                select_rx,
                target,
                fps_cap,
                fps_rx,
                scale_rx,
                cursor_rx,
                force_frame,
            ) {
                tracing::error!(error = %e, "capture loop ended");
            }
        })?;

    // 最初のフレームの実寸法と、実際にエンコードできるコーデックを本体へ知らせる (本体は接続待受を
    // 始める前にこれを待つ)。
    let resolution = res_rx.recv_timeout(Duration::from_secs(10)).ok();
    let host_codecs = crate::encode::probe_encoders(&device, encode_gpu.as_deref());
    tracing::info!(?resolution, ?host_codecs, "capture worker ready");
    out_tx
        .send(FromWorker::Ready {
            resolution,
            host_codecs,
        })
        .context("send Ready")?;

    let stall_tx = out_tx.clone();
    std::thread::Builder::new()
        .name("encode".into())
        .spawn(move || {
            let on_stall = move || {
                let _ = stall_tx.send(FromWorker::EncodeStalled);
            };
            if let Err(e) = crate::encode_loop(
                device,
                frame_rx,
                nal_tx,
                enc_cmd_rx,
                perf,
                max_bitrate_bps,
                encode_gpu,
                codec_tx,
                &on_stall,
            ) {
                tracing::error!(error = %e, "encode loop ended");
            }
        })?;

    let codec_out = out_tx.clone();
    std::thread::Builder::new()
        .name("worker-codec".into())
        .spawn(move || {
            while let Some(status) = codec_rx.blocking_recv() {
                if codec_out.send(FromWorker::Codec(status)).is_err() {
                    break;
                }
            }
        })?;

    while let Some(nal) = nal_rx.blocking_recv() {
        if out_tx.send(FromWorker::Nal(nal)).is_err() {
            break;
        }
    }
    Ok(())
}

/// テイクオーバー中の固着で物理ディスプレイの復元を本体へ依頼する (capture スレッドから呼ぶ)。
/// 本体の復元を待たずに手元の状態を「テイクオーバー無し」にして、通常の作り直し判定へ進める
/// (本体も復元後に `Takeover(false)` を送ってくる)。
pub fn request_takeover_restore() {
    crate::display::mirror_takeover_active(false);
    if let Some(out) = OUT.get() {
        let _ = out.send(FromWorker::RestoreTakeover);
    }
}
