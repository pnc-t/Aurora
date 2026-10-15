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
//!
//! **デバイス形式は決め打ちにしない**: 実機で既定出力デバイスが 44100Hz だった環境があり、
//! 48000Hz/2ch/f32 固定の `StreamConfig` では `build_input_stream` が
//! "requested stream configuration is not supported" で失敗し、音声機能ごと無効になった
//! (2026-07-29)。ループバックで開く形式は必ず `device.default_output_config()` の値を使う。
//!
//! **cpal 0.15 の罠**: 出力デバイスに対して `default_input_config()` /
//! `supported_input_configs()` を呼んではいけない (エラーまたは空を返す)。ループバック用の
//! 形式の正本は常に `default_output_config()` である。

use anyhow::{Result, anyhow};
use aurora_common::framing::MAX_PAYLOAD;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// 既定出力デバイス変更を検知する/初回失敗をリトライするポーリング間隔。
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
///
/// リサンプラ状態 ([`Resampler`]) はここには含めない。デバイスごとにソースレートが
/// 変わるため、EncodeState (プロセス生存中ずっと 1 つ) ではなく 1 ストリームごとに
/// 作り直す ([`open_stream_for_default_device`] のローカル変数として持つ)。
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

    /// 48kHz/ステレオ済みのサンプルを蓄積し、20ms フレームが貯まるたびに Opus エンコードして
    /// `tx` へ送る。cpal コールバック本体から呼ぶ (T→f32 変換とリサンプルは呼び出し側の責務)。
    fn push_and_encode(&mut self, samples: &[f32], tx: &mpsc::Sender<EncodedAudio>) {
        if self.exhausted {
            return;
        }
        self.acc.extend_from_slice(samples);
        while self.acc.len() >= FRAME_INTERLEAVED {
            match self
                .encoder
                .encode_float(&self.acc[..FRAME_INTERLEAVED], &mut self.out_buf)
            {
                Ok(n) => {
                    let frame = EncodedAudio {
                        seq: self.seq,
                        data: self.out_buf[..n].to_vec(),
                    };
                    self.seq = match self.seq.checked_add(1) {
                        Some(s) => s,
                        None => {
                            tracing::error!("audio nonce space exhausted; stopping audio capture");
                            self.exhausted = true;
                            self.acc.clear();
                            return;
                        }
                    };
                    // バックプレッシャ時は捨てる (音声は次フレームで自然回復)。
                    let _ = tx.try_send(frame);
                }
                Err(e) => tracing::warn!(error = %e, "opus encode failed"),
            }
            self.acc.drain(..FRAME_INTERLEAVED);
        }
    }
}

/// デバイスのネイティブ形式 (任意チャンネル数/任意サンプルレート) から Opus 前提の
/// 48kHz/ステレオへ変換するリサンプラ。ストリーム 1 本ごとに生成し (`EncodeState` には
/// 混ぜない)、コールバックをまたいで小数位置 (`pos`) と未消費サンプル (`pending`) を
/// 持ち越すことで、20ms 境界ごとのプチノイズ (補間の不連続) を防ぐ。
struct Resampler {
    src_channels: u16,
    /// 出力 1 サンプルあたり読み進める入力フレーム数 (src_rate / 48000)。等倍なら 1.0。
    step: f64,
    /// 次に生成する出力フレームに対応する `pending` 内の読み取り位置 (フレーム単位、小数)。
    pos: f64,
    /// ダウンミックス済み (2ch) だがまだリサンプルしていない、入力レートのままの
    /// インターリーブ蓄積バッファ。
    pending: Vec<f32>,
}

impl Resampler {
    fn new(src_channels: u16, src_rate: u32) -> Self {
        Self {
            src_channels,
            step: src_rate as f64 / SAMPLE_RATE as f64,
            pos: 0.0,
            pending: Vec::new(),
        }
    }

    /// f32 変換済み・デバイスのネイティブチャンネル数でインターリーブされた入力を、
    /// ダウンミックス + 線形補間リサンプルして `out` (呼び出し側の `EncodeState.acc` 相当) へ
    /// 追記する。
    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.src_channels as usize;
        if ch == 0 {
            return;
        }
        for frame in input.chunks_exact(ch) {
            match ch {
                // モノラルは L/R へ複製。
                1 => {
                    self.pending.push(frame[0]);
                    self.pending.push(frame[0]);
                }
                // ステレオはそのまま。
                2 => {
                    self.pending.push(frame[0]);
                    self.pending.push(frame[1]);
                }
                // 3ch 以上 (5.1 サラウンド等) はチャンネルレイアウト情報が無いと正しい
                // ダウンミックスができない。音声は本アプリの主機能ではないため、先頭 2ch
                // (大抵は フロント L/R) をそのまま採用する簡易近似で割り切る。
                _ => {
                    self.pending.push(frame[0]);
                    self.pending.push(frame[1]);
                }
            }
        }

        // 線形補間でソースレート → 48kHz へ。等倍 (step == 1.0) のときは frac が常に 0 に
        // なり実質コピーになる (無変換パスを別途持たない簡略化。オーディオは軽量なので許容)。
        loop {
            let idx = self.pos.floor() as usize;
            if idx + 1 >= self.pending.len() / 2 {
                break;
            }
            let frac = (self.pos - idx as f64) as f32;
            let (l0, r0) = (self.pending[idx * 2], self.pending[idx * 2 + 1]);
            let (l1, r1) = (self.pending[idx * 2 + 2], self.pending[idx * 2 + 3]);
            out.push(l0 + (l1 - l0) * frac);
            out.push(r0 + (r1 - r0) * frac);
            self.pos += self.step;
        }

        // 消費済みフレームを破棄し pos を巻き戻す。次コールバックへ持ち越すのは補間に
        // 必要な直近フレーム分の余白のみ (= 境界をまたいだ連続性の根拠)。
        let consumed = (self.pos.floor() as usize).min(self.pending.len() / 2);
        if consumed > 0 {
            self.pending.drain(..consumed * 2);
            self.pos -= consumed as f64;
        }
    }
}

/// 音声キャプチャを開始する。成功すれば専用スレッドでストリームが回り続ける。
///
/// 返り値の `Arc<AtomicBool>` は「今この瞬間キャプチャが動いているか」を表す動的フラグ。
/// 初回オープンに失敗しても `Err` にはせず (デバイス側の一時的な問題は致命的ではない)、
/// フラグ `false` のまま返して以後バックグラウンドでリトライを続ける。`Err` になるのは
/// Opus エンコーダ初期化やスレッド起動そのものが失敗する、本当に回復不能な場合のみ。
pub fn spawn_capture(tx: mpsc::Sender<EncodedAudio>) -> Result<Arc<AtomicBool>> {
    // seq/encoder/exhausted はデバイス再生成をまたいで連続させる必要があるため、
    // 個々のストリーム生成の外側 (このスレッド関数のスコープ) で保持し、
    // cpal コールバックとは Arc<Mutex<_>> で共有する。
    let state = Arc::new(Mutex::new(EncodeState::new()?));
    let available = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();

    let thread_state = state.clone();
    let thread_available = available.clone();
    std::thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || run_capture_loop(tx, thread_state, thread_available, ready_tx))?;

    // 初回オープン試行 (成否問わず) の完了だけを待つ。失敗していてもここでは Err にせず、
    // available=false のまま返す (呼び出し元の起動ログ整合のための同期待ちに過ぎない)。
    let _ = ready_rx.recv();

    Ok(available)
}

/// 専用スレッド本体。初回ストリーム生成を試みた後 (成否は `ready_tx` へ一度だけ通知)、
/// 既定出力デバイスの変化とストリーム断を `DEVICE_POLL_INTERVAL` ごとにポーリングして
/// 追従・再試行し続ける。
fn run_capture_loop(
    tx: mpsc::Sender<EncodedAudio>,
    state: Arc<Mutex<EncodeState>>,
    available: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<()>,
) {
    let host = cpal::default_host();
    let mut current_stream: Option<cpal::Stream> = None;
    let mut current_device_name = String::new();
    let mut ready_sent = false;
    // ストリームのエラーコールバック (デバイス消失等) が立てる。立っていたら今のストリームを捨てて
    // 開き直す。これが無いと、デバイス名が変わらないままストリームだけ死んだときに音声が
    // 黙って止まり続ける (下の「ストリームを持っていない」条件に到達しない)。
    let stream_failed = Arc::new(AtomicBool::new(false));

    loop {
        if stream_failed.swap(false, Ordering::Relaxed) && current_stream.is_some() {
            tracing::warn!("audio loopback stream failed; reopening");
            current_stream = None;
        }
        let latest_name = host
            .default_output_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();

        // 再オープンする条件は「デバイス名が変わった」だけでなく「今ストリームを持って
        // いない」も含める。初回失敗後や、ストリーム断後にデバイス名が変わらないまま
        // 復旧させたいケース (同じデバイスの一時的な使用不可等) を取りこぼさないため。
        if current_stream.is_none() || latest_name != current_device_name {
            match open_stream_for_default_device(&host, &state, &tx, &stream_failed) {
                Ok((stream, name)) => {
                    if current_stream.is_some() {
                        tracing::info!(device = %name, "audio loopback device switched");
                    } else {
                        tracing::info!(device = %name, "audio loopback device");
                    }
                    current_device_name = name;
                    current_stream = Some(stream);
                    available.store(true, Ordering::Relaxed);
                }
                Err(e) => {
                    // 失敗は致命的にしない。旧ストリーム (あれば) はそのまま保持し、
                    // 次のポーリングで再試行する。旧ストリームが無い (初回 or 断後) なら
                    // 「今は使えない」ことをフラグへ反映する。
                    tracing::warn!(error = %e, "failed to open audio loopback stream; will retry");
                    if current_stream.is_none() {
                        available.store(false, Ordering::Relaxed);
                    }
                }
            }
        }

        if !ready_sent {
            ready_sent = true;
            let _ = ready_tx.send(());
        }

        std::thread::sleep(DEVICE_POLL_INTERVAL);
    }
}

/// 現在の既定出力デバイスで入力ストリームを 1 本構築して再生開始する。
/// `state` は複数回の呼び出し (= デバイス再生成) をまたいで共有される。
fn open_stream_for_default_device(
    host: &cpal::Host,
    state: &Arc<Mutex<EncodeState>>,
    tx: &mpsc::Sender<EncodedAudio>,
    failed: &Arc<AtomicBool>,
) -> Result<(cpal::Stream, String)> {
    // ループバック対象はシステムの既定出力デバイス。
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let name = device.name().unwrap_or_default();

    // 形式は `default_output_config()` から取る (決め打ちしない)。
    // cpal 0.15 では出力デバイスに `default_input_config()` / `supported_input_configs()` を
    // 呼んではいけない (エラー/空を返す。ファイル冒頭のモジュール doc 参照)。
    let supported = device
        .default_output_config()
        .map_err(|e| anyhow!("default_output_config: {e}"))?;
    let src_channels = supported.channels();
    let src_rate = supported.sample_rate().0;
    let sample_format = supported.sample_format();
    let config = supported.config();

    let needs_resample = src_channels != CHANNELS || src_rate != SAMPLE_RATE;
    tracing::info!(
        device = %name,
        input_rate = src_rate,
        input_channels = src_channels,
        sample_format = %sample_format,
        resampling = needs_resample,
        "opening audio loopback stream"
    );

    let cb_state = Arc::clone(state);
    let cb_tx = tx.clone();
    let resampler = Resampler::new(src_channels, src_rate);

    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            build_typed_stream::<f32>(&device, &config, cb_state, cb_tx, resampler, failed.clone())
        }
        cpal::SampleFormat::I16 => {
            build_typed_stream::<i16>(&device, &config, cb_state, cb_tx, resampler, failed.clone())
        }
        cpal::SampleFormat::U16 => {
            build_typed_stream::<u16>(&device, &config, cb_state, cb_tx, resampler, failed.clone())
        }
        other => Err(anyhow!("unsupported sample format: {other}")),
    }?;

    stream.play().map_err(|e| anyhow!("stream play: {e}"))?;
    Ok((stream, name))
}

/// サンプル型 `T` (f32/i16/u16) 固定でループバック入力ストリームを構築する。
/// `resampler` は `FnMut` クロージャに `move` して閉じ込め、コールバックをまたいで
/// 状態 (小数位置・持ち越しサンプル) を保持する (`EncodeState` の `Arc<Mutex<_>>` とは
/// 別に、1 ストリームだけが所有するため Mutex 不要)。
fn build_typed_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: Arc<Mutex<EncodeState>>,
    tx: mpsc::Sender<EncodedAudio>,
    mut resampler: Resampler,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let err_fn = move |e| {
        tracing::warn!(error = %e, "audio input stream error");
        failed.store(true, Ordering::Relaxed);
    };
    // T→f32 変換の作業バッファ。コールバックをまたいで再利用し毎回の確保を避ける。
    let mut float_buf: Vec<f32> = Vec::new();
    // リサンプル後 (48kHz/ステレオ) の出力を一旦ここへ溜め、まとめて EncodeState へ渡す。
    let mut resampled: Vec<f32> = Vec::new();

    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                float_buf.clear();
                float_buf.extend(data.iter().map(|s| f32::from_sample(*s)));

                resampled.clear();
                resampler.push(&float_buf, &mut resampled);
                if resampled.is_empty() {
                    return;
                }

                // ロックは 1 コールバック分の処理中のみ保持する (20ms 粒度なので許容範囲)。
                let mut guard = match state.lock() {
                    Ok(g) => g,
                    // Mutex poisoning は実質発生しない前提 (コールバック内 panic 禁止)。
                    // 万一発生してもこのフレームを諦めるだけに留め、スレッドを落とさない。
                    Err(_) => return,
                };
                guard.push_and_encode(&resampled, &tx);
            },
            err_fn,
            None,
        )
        .map_err(|e| anyhow!("build_input_stream: {e}"))?;

    Ok(stream)
}
