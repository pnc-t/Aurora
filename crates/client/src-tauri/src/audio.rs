//! クライアント側の音声再生。
//!
//! UDP で受け取った Opus フレーム (STREAM_AUDIO) をデコードし、cpal の出力ストリームへ流す。
//! cpal の `Stream` は `!Send` のため、デコードと再生はすべて専用スレッド上で完結させ、
//! 外部へは Opus バイト列を渡す送信ハンドル (`AudioPlayer`) だけを公開する。
//!
//! Android/iOS は cpal/opus が NDK クロスコンパイルに対応しないため、音声フェーズ2
//! (Oboe/AAudio 等のネイティブ実装) まで no-op スタブで応答する。公開 API (`AudioPlayer`/
//! `spawn`) はデスクトップと完全に同一なので、呼び出し側 (`net/` 配下) は変更不要。

pub use imp::{AudioPlayer, spawn};

#[cfg(desktop)]
mod imp {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    /// 再生リングバッファの上限 (インターリーブ f32 サンプル数)。約 200ms 分を超えたら
    /// 古いサンプルを捨てて遅延の累積を抑える。
    const MAX_BUFFERED: usize = (SAMPLE_RATE as usize) * CHANNELS as usize / 5;
    /// Opus 1 フレームの最大デコードサイズ (120ms/ch を上限とする) × ch。
    const MAX_DECODE_SAMPLES: usize = 5760 * CHANNELS as usize;

    /// 音声再生ハンドル。Opus フレームを再生スレッドへ渡す。
    pub struct AudioPlayer {
        tx: std::sync::mpsc::Sender<Vec<u8>>,
        /// 再生ゲイン (f32 bits を格納)。cpal コールバックはリアルタイム制約で
        /// ロックを取れないため、Mutex ではなく lock-free な atomic で共有する。
        gain: Arc<AtomicU32>,
    }

    impl AudioPlayer {
        /// Opus フレーム送信用のクローンを得る (udp 受信タスクへ渡す)。
        pub fn sender(&self) -> std::sync::mpsc::Sender<Vec<u8>> {
            self.tx.clone()
        }

        /// 再生音量を設定する (0.0〜1.0 にクランプ)。host へは送らない client 内部のゲイン。
        pub fn set_volume(&self, volume: f32) {
            self.gain
                .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    /// 音声再生スレッドを起動する。出力デバイス / ストリーム / Opus デコーダの初期化に
    /// 失敗した場合は Err を返す。成功後はハンドルが drop されると (送信端がすべて閉じると)
    /// スレッドは終了し、ストリームも停止する。
    pub fn spawn() -> Result<AudioPlayer> {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let gain_thread = gain.clone();

        std::thread::Builder::new()
            .name("audio-playback".into())
            .spawn(move || {
                let ring: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
                let stream = match build_output_stream(ring.clone(), gain_thread) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let mut decoder = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo) {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("opus decoder: {e}")));
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                let _stream = stream; // 再生スレッドが生きている間ストリームを保持

                let mut pcm = vec![0f32; MAX_DECODE_SAMPLES];
                loop {
                    // stop を兼ねてタイムアウト付き recv。全送信端が閉じたら終了。
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(frame) => match decoder.decode_float(&frame, &mut pcm, false) {
                            Ok(samples_per_ch) => {
                                let n = samples_per_ch * CHANNELS as usize;
                                let mut buf = ring.lock().unwrap();
                                buf.extend(&pcm[..n]);
                                // 上限超過分は古い側を捨てる
                                while buf.len() > MAX_BUFFERED {
                                    buf.pop_front();
                                }
                            }
                            Err(e) => tracing::debug!(error = %e, "opus decode failed"),
                        },
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(AudioPlayer { tx, gain }),
            Ok(Err(e)) => Err(anyhow!("audio playback init failed: {e}")),
            Err(_) => Err(anyhow!("audio playback thread exited before init")),
        }
    }

    fn build_output_stream(
        ring: Arc<Mutex<VecDeque<f32>>>,
        gain: Arc<AtomicU32>,
    ) -> Result<cpal::Stream> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?;
        tracing::info!(
            device = device.name().unwrap_or_default(),
            "audio output device"
        );

        let config = cpal::StreamConfig {
            channels: CHANNELS,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let err_fn = |e| tracing::warn!(error = %e, "audio output stream error");
        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut buf = ring.lock().unwrap();
                    let g = f32::from_bits(gain.load(Ordering::Relaxed));
                    for sample in out.iter_mut() {
                        *sample = buf.pop_front().unwrap_or(0.0) * g;
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| anyhow!("build_output_stream: {e}"))?;

        stream.play().map_err(|e| anyhow!("stream play: {e}"))?;
        Ok(stream)
    }
}

/// Android/iOS 向け no-op スタブ。Opus フレームを受け取って捨てるだけで、再生は行わない。
#[cfg(mobile)]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use anyhow::Result;

    /// 音声再生ハンドル (no-op)。`sender()` が返すチャンネルの受信側は存在しないため、
    /// 送信は即座に読み捨てられる (エラーにはならない)。
    pub struct AudioPlayer {
        tx: std::sync::mpsc::Sender<Vec<u8>>,
        gain: Arc<AtomicU32>,
    }

    impl AudioPlayer {
        pub fn sender(&self) -> std::sync::mpsc::Sender<Vec<u8>> {
            self.tx.clone()
        }

        pub fn set_volume(&self, volume: f32) {
            self.gain
                .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    /// 受信スレッドを起こさず、送信端だけ返す (受信側は即 drop、送られたフレームは捨てられる)。
    pub fn spawn() -> Result<AudioPlayer> {
        let (tx, _rx) = std::sync::mpsc::channel::<Vec<u8>>();
        Ok(AudioPlayer {
            tx,
            gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
        })
    }
}
