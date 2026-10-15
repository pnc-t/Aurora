//! クライアント側の音声再生。
//!
//! UDP で受け取った Opus フレーム (STREAM_AUDIO) をデコードし、cpal の出力ストリームへ流す。
//! cpal の `Stream` は `!Send` のため、デコードと再生はすべて専用スレッド上で完結させ、
//! 外部へは Opus バイト列を渡す送信ハンドル (`AudioPlayer`) だけを公開する。
//!
//! Android/iOS は cpal/opus が NDK クロスコンパイルに対応しないため、方式A (WebView 再生) を
//! 採る: host から届いた Opus フレームをデコードせず、そのまま WebView へ転送し、フロント側
//! WebCodecs `AudioDecoder` + Web Audio で再生する。呼び出し側 (`net/connect.rs`) は
//! デスクトップと異なり `Sink` (映像/イベントと同じ差し替え可能な転送先) を渡して `spawn` する。

pub use imp::{AudioPlayer, spawn};

#[cfg(desktop)]
mod imp {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    /// 再生リングバッファの上限 (インターリーブ f32 サンプル数)。約 200ms 分を超えたら
    /// 古いサンプルを捨てて遅延の累積を抑える。
    const MAX_BUFFERED: usize = (SAMPLE_RATE as usize) * CHANNELS as usize / 5;
    /// Opus 1 フレームの最大デコードサイズ (120ms/ch を上限とする) × ch。
    const MAX_DECODE_SAMPLES: usize = 5760 * CHANNELS as usize;
    /// 出力デバイスの追従 (既定デバイス変更・ストリーム断) を確認する間隔。
    const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);

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
                // 出力ストリームのエラーコールバック (デバイス消失等) が立てる。
                let stream_failed = Arc::new(AtomicBool::new(false));
                let host = cpal::default_host();
                let (stream, device_name) = match build_output_stream(
                    &host,
                    ring.clone(),
                    gain_thread.clone(),
                    stream_failed.clone(),
                ) {
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
                // 再生スレッドが生きている間ストリームを保持する。接続時の既定デバイスを握り続けると、
                // ヘッドホンの抜き差しや既定デバイスの切替で無音になるため、定期的に追従して作り直す。
                let mut stream = Some(stream);
                let mut device_name = device_name;
                let mut last_poll = Instant::now();

                let mut pcm = vec![0f32; MAX_DECODE_SAMPLES];
                loop {
                    if last_poll.elapsed() >= DEVICE_POLL_INTERVAL {
                        last_poll = Instant::now();
                        let latest = host
                            .default_output_device()
                            .and_then(|d| d.name().ok())
                            .unwrap_or_default();
                        let failed = stream_failed.swap(false, Ordering::Relaxed);
                        if failed || stream.is_none() || latest != device_name {
                            // 旧ストリームを先に止めてから開き直す (同一デバイスの再オープン競合を避ける)。
                            stream = None;
                            match build_output_stream(
                                &host,
                                ring.clone(),
                                gain_thread.clone(),
                                stream_failed.clone(),
                            ) {
                                Ok((s, name)) => {
                                    tracing::info!(device = %name, failed, "audio output stream reopened");
                                    stream = Some(s);
                                    device_name = name;
                                }
                                // 失敗しても次の確認で再試行する (その間は無音)。
                                Err(e) => tracing::warn!(error = %e, "audio output reopen failed; will retry"),
                            }
                        }
                    }
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
        host: &cpal::Host,
        ring: Arc<Mutex<VecDeque<f32>>>,
        gain: Arc<AtomicU32>,
        failed: Arc<AtomicBool>,
    ) -> Result<(cpal::Stream, String)> {
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?;
        let name = device.name().unwrap_or_default();
        tracing::info!(device = %name, "audio output device");

        let config = cpal::StreamConfig {
            channels: CHANNELS,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let err_fn = move |e| {
            tracing::warn!(error = %e, "audio output stream error");
            failed.store(true, Ordering::Relaxed);
        };
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
        Ok((stream, name))
    }
}

/// Android/iOS 向け実装 (方式A: WebView 再生)。デコードは行わず、受け取った Opus フレームを
/// `Sink` 経由でそのまま WebView へ転送する。再生 (デコード/ゲイン) はフロント側
/// (`core/audio-player.ts`) が担う。
#[cfg(mobile)]
mod imp {
    use anyhow::Result;
    use tauri::ipc::InvokeResponseBody;

    use crate::net::Sink;

    /// 音声転送ハンドル。`sender()` は udp 受信タスクへ渡す Opus フレーム送信口
    /// (デスクトップの `AudioPlayer` と同じインタフェース)。
    pub struct AudioPlayer {
        tx: std::sync::mpsc::Sender<Vec<u8>>,
    }

    impl AudioPlayer {
        pub fn sender(&self) -> std::sync::mpsc::Sender<Vec<u8>> {
            self.tx.clone()
        }

        /// 再生ゲインはフロント側 GainNode で当てるため no-op (呼び出し側とのインタフェース
        /// 互換のためだけに残す)。
        pub fn set_volume(&self, _volume: f32) {}
    }

    /// Opus フレームを sink 経由でフロントへ転送する専用スレッドを起動する。デコード/デバイス
    /// 初期化を伴わないため常に成功する (デスクトップの cpal/opus 初期化失敗に相当する経路が無い)。
    pub fn spawn(sink: Sink) -> Result<AudioPlayer> {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();

        std::thread::Builder::new()
            .name("audio-forward".into())
            .spawn(move || {
                // 全送信端 (AudioPlayer::sender の clone) が閉じたら recv が Err になり終了する。
                while let Ok(frame) = rx.recv() {
                    sink.send_audio(InvokeResponseBody::Raw(frame));
                }
            })?;

        Ok(AudioPlayer { tx })
    }
}
