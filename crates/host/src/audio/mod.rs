//! 音声キャプチャアクター。
//!
//! Windows: WASAPI ループバック (既定の出力デバイスを入力ストリームとして開く) で
//! システム音声を取り込み、Opus で 20ms フレームにエンコードして mpsc へ流す。
//!
//! cpal の `Stream` は `!Send` のため、ストリームは生成した専用スレッド上に留め置く
//! (park し続ける)。エンコード済みフレームのみ `tokio::sync::mpsc` でネットワーク層へ渡す。

use anyhow::{Result, anyhow};
use aurora_common::framing::MAX_PAYLOAD;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;

/// Opus が要求するサンプリングレート。
const SAMPLE_RATE: u32 = 48_000;
/// ステレオ固定。
const CHANNELS: u16 = 2;
/// 20ms フレーム = 1ch あたり 960 サンプル。
const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * 20;
/// インターリーブされた 1 フレームの f32 サンプル数。
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * CHANNELS as usize;

/// エンコード済み音声フレーム (Opus)。
#[derive(Debug, Clone)]
pub struct EncodedAudio {
    pub seq: u32,
    pub data: Vec<u8>,
}

/// 音声キャプチャを開始する。成功すれば専用スレッドでストリームが回り続ける。
/// 初期化 (デバイス取得 / ストリーム生成 / Opus 初期化) の成否は同期的に返す。
pub fn spawn_capture(tx: mpsc::Sender<EncodedAudio>) -> Result<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || match build_capture_stream(tx) {
            Ok(stream) => {
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                // `_stream` を保持し続ける (Drop で停止する)。park でスレッドを生かす。
                let _stream = stream;
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
            }
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(anyhow!("audio capture init failed: {e}")),
        Err(_) => Err(anyhow!("audio capture thread exited before init")),
    }
}

fn build_capture_stream(tx: mpsc::Sender<EncodedAudio>) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    // ループバック対象はシステムの既定出力デバイス。
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    tracing::info!(
        device = device.name().unwrap_or_default(),
        "audio loopback device"
    );

    let config = cpal::StreamConfig {
        channels: CHANNELS,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

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

    // コールバックをまたいで貯めるインターリーブ蓄積バッファ。
    let mut acc: Vec<f32> = Vec::with_capacity(FRAME_INTERLEAVED * 2);
    let mut out_buf = vec![0u8; MAX_PAYLOAD];
    let mut seq: u32 = 0;
    // nonce 空間 (u32 seq) 枯渇フラグ。net 層は `NONCE_DOMAIN_AUDIO | seq` を nonce に
    // 使うため、seq が一周すると同一セッション鍵で nonce が再利用され ChaChaPoly の
    // 機密性・完全性が破れる。映像側 (net.rs の checked_add + 切断) と対称の防御。
    // 音声スレッドはセッションをまたいでプロセスと同寿命のため、枯渇時は切断ではなく
    // 音声送出を恒久停止する (2^32 フレーム ≒ 連続 2.7 年で到達)。
    let mut exhausted = false;

    let err_fn = |e| tracing::warn!(error = %e, "audio input stream error");
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if exhausted {
                    acc.clear();
                    return;
                }
                acc.extend_from_slice(data);
                while acc.len() >= FRAME_INTERLEAVED {
                    match encoder.encode_float(&acc[..FRAME_INTERLEAVED], &mut out_buf) {
                        Ok(n) => {
                            let frame = EncodedAudio {
                                seq,
                                data: out_buf[..n].to_vec(),
                            };
                            seq = match seq.checked_add(1) {
                                Some(s) => s,
                                None => {
                                    tracing::error!(
                                        "audio nonce space exhausted; stopping audio capture"
                                    );
                                    exhausted = true;
                                    return;
                                }
                            };
                            // バックプレッシャ時は捨てる (音声は次フレームで自然回復)。
                            let _ = tx.try_send(frame);
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
    Ok(stream)
}
