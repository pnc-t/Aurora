//! 音声キャプチャアクター。
//!
//! Windows: WASAPI ループバック (既定の出力デバイスを入力ストリームとして開く) で
//! システム音声を取り込み、Opus で 20ms フレームにエンコードして mpsc へ流す。
//!
//! cpal の `Stream` は `!Send` のため、ストリームは生成した専用スレッド上に留め置く。
//! cpal は Windows で既定出力デバイスの変更通知 API を持たないため、専用スレッドが
//! 一定間隔で `default_output_device()` の名前をポーリングし、変化を検知したら
//! ストリームを作り直して追従する。エンコード状態 (Opus encoder / seq / 蓄積バッファ)
//! はストリーム再生成をまたいで連続させる必要がある (下記 nonce 連続性コメント参照) ため、
//! 個々のストリーム生成の外側 (`Arc<Mutex<EncodeState>>`) に持ち上げて共有する。
//! エンコード済みフレームのみ `tokio::sync::mpsc` でネットワーク層へ渡す。

use anyhow::{Result, anyhow};
use aurora_common::framing::MAX_PAYLOAD;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Opus が要求するサンプリングレート。
const SAMPLE_RATE: u32 = 48_000;
/// ステレオ固定。
const CHANNELS: u16 = 2;
/// 20ms フレーム = 1ch あたり 960 サンプル。
const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * 20;
/// インターリーブされた 1 フレームの f32 サンプル数。
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * CHANNELS as usize;
/// 既定出力デバイス変更を検知するポーリング間隔。
/// cpal に Windows 向けの変更通知 API が無いための次善策。
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// エンコード済み音声フレーム (Opus)。
#[derive(Debug, Clone)]
pub struct EncodedAudio {
    pub seq: u32,
    pub data: Vec<u8>,
}

/// cpal コールバックとデバイス監視ループが共有するエンコード状態。
///
/// デバイス切替でストリーム (`cpal::Stream`) を作り直しても、この状態は使い回す。
/// net 層は音声パケットの nonce に `NONCE_DOMAIN_AUDIO | seq` を使うため、
/// seq がストリーム再生成のたびに 0 へ戻ると同一セッション鍵で nonce が再利用され
/// ChaChaPoly の機密性・完全性が破れる。そのため seq (と exhausted 枯渇フラグ、
/// Opus encoder 本体) はストリームの生成/破棄をまたいで単調に持ち越す。
struct EncodeState {
    encoder: opus::Encoder,
    acc: Vec<f32>,
    out_buf: Vec<u8>,
    seq: u32,
    exhausted: bool,
}

impl EncodeState {
    fn new() -> Result<Self> {
        let mut encoder = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )
        .map_err(|e| anyhow!("opus encoder: {e}"))?;
        // 低遅延寄りに帯域を抑える (音声用途)。
        encoder
            .set_bitrate(opus::Bitrate::Bits(96_000))
            .map_err(|e| anyhow!("opus bitrate: {e}"))?;

        Ok(Self {
            encoder,
            // コールバックをまたいで貯めるインターリーブ蓄積バッファ。
            acc: Vec::with_capacity(FRAME_INTERLEAVED * 2),
            out_buf: vec![0u8; MAX_PAYLOAD],
            seq: 0,
            // nonce 空間 (u32 seq) 枯渇フラグ。音声スレッドはセッションをまたいで
            // プロセスと同寿命のため、枯渇時は切断ではなく音声送出を恒久停止する
            // (2^32 フレーム ≒ 連続 2.7 年で到達。映像側 net.rs の checked_add + 切断と対称)。
            exhausted: false,
        })
    }
}

/// 音声キャプチャを開始する。成功すれば専用スレッドでストリームが回り続ける。
/// 初期化 (デバイス取得 / ストリーム生成 / Opus 初期化) の成否は同期的に返す。
pub fn spawn_capture(tx: mpsc::Sender<EncodedAudio>) -> Result<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || run_capture_loop(tx, ready_tx))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(anyhow!("audio capture init failed: {e}")),
        Err(_) => Err(anyhow!("audio capture thread exited before init")),
    }
}

/// 専用スレッド本体。初回ストリーム生成の成否を `ready_tx` で返した後、
/// 既定出力デバイスの変化をポーリングし続けてストリームを追従させる。
///
/// `current_stream` はポーリングのたびに使い回すだけで明示的に読み出すことはないが、
/// 生存させ続けること自体が目的 (drop されるとキャプチャが止まる) なので
/// `unused_assignments` の誤検知を許容する。
#[allow(unused_assignments, unused_variables)]
fn run_capture_loop(
    tx: mpsc::Sender<EncodedAudio>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) {
    // seq/encoder/exhausted はデバイス再生成をまたいで連続させる必要があるため、
    // 個々のストリーム生成の外側 (このスレッド関数のスコープ) で保持し、
    // cpal コールバックとは Arc<Mutex<_>> で共有する。
    let state = match EncodeState::new() {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
            return;
        }
    };

    let host = cpal::default_host();

    let (mut current_stream, mut current_device_name) =
        match open_stream_for_default_device(&host, &state, &tx) {
            Ok((stream, name)) => {
                tracing::info!(device = %name, "audio loopback device");
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                (stream, name)
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
        };

    loop {
        std::thread::sleep(DEVICE_POLL_INTERVAL);

        // cpal は Windows で既定出力デバイス変更の通知 API を持たないため、
        // ポーリングで検知するしかない。
        let latest_name = host
            .default_output_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();

        if latest_name != current_device_name {
            match open_stream_for_default_device(&host, &state, &tx) {
                Ok((stream, name)) => {
                    tracing::info!(device = %name, "audio loopback device switched");
                    current_device_name = name;
                    // 旧ストリームはここで drop されて停止する。
                    current_stream = stream;
                }
                Err(e) => {
                    // デバイス取得/構築失敗は致命的にしない。旧ストリーム (あれば)
                    // をそのまま保持し、次のポーリングで再試行する。
                    tracing::warn!(
                        error = %e,
                        "failed to switch audio loopback device; will retry"
                    );
                }
            }
        }
    }
}

/// 現在の既定出力デバイスで入力ストリームを 1 本構築して再生開始する。
/// `state` は複数回の呼び出し (= デバイス再生成) をまたいで共有される。
fn open_stream_for_default_device(
    host: &cpal::Host,
    state: &Arc<Mutex<EncodeState>>,
    tx: &mpsc::Sender<EncodedAudio>,
) -> Result<(cpal::Stream, String)> {
    // ループバック対象はシステムの既定出力デバイス。
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let name = device.name().unwrap_or_default();

    let config = cpal::StreamConfig {
        channels: CHANNELS,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    let cb_state = Arc::clone(state);
    let cb_tx = tx.clone();
    let err_fn = |e| tracing::warn!(error = %e, "audio input stream error");

    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                // ロックは 1 フレーム分の処理中のみ保持する (20ms 粒度なので許容範囲)。
                let mut guard = match cb_state.lock() {
                    Ok(g) => g,
                    // Mutex poisoning は実質発生しない前提 (コールバック内 panic 禁止)。
                    // 万一発生してもこのフレームを諦めるだけに留め、スレッドを落とさない。
                    Err(_) => return,
                };
                let EncodeState {
                    encoder,
                    acc,
                    out_buf,
                    seq,
                    exhausted,
                } = &mut *guard;

                if *exhausted {
                    acc.clear();
                    return;
                }
                acc.extend_from_slice(data);
                while acc.len() >= FRAME_INTERLEAVED {
                    match encoder.encode_float(&acc[..FRAME_INTERLEAVED], out_buf) {
                        Ok(n) => {
                            let frame = EncodedAudio {
                                seq: *seq,
                                data: out_buf[..n].to_vec(),
                            };
                            *seq = match seq.checked_add(1) {
                                Some(s) => s,
                                None => {
                                    tracing::error!(
                                        "audio nonce space exhausted; stopping audio capture"
                                    );
                                    *exhausted = true;
                                    return;
                                }
                            };
                            // バックプレッシャ時は捨てる (音声は次フレームで自然回復)。
                            let _ = cb_tx.try_send(frame);
                        }
                        Err(e) => tracing::warn!(error = %e, "opus encode failed"),
                    }
                    acc.drain(..FRAME_INTERLEAVED);
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| anyhow!("build_input_stream: {e}"))?;

    stream.play().map_err(|e| anyhow!("stream play: {e}"))?;
    Ok((stream, name))
}
