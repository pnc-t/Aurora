//! 映像エンコーダ抽象 (Port)。

use aurora_common::proto::VideoCodec;

use crate::capture::Frame;
#[cfg(target_os = "windows")]
use crate::capture::SharedDevice;

#[derive(Debug, Clone)]
pub struct EncodedNal {
    /// 選択中コーデックのビットストリーム (H.264/HEVC は Annex-B `00 00 00 01` start code 付き、
    /// AV1 は OBU (Open Bitstream Unit) 列)。
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub frame_id: u16,
}

pub trait VideoEncoder: Send {
    fn push_frame(&mut self, frame: &Frame) -> anyhow::Result<()>;
    fn poll_nal(&mut self) -> anyhow::Result<Option<EncodedNal>>;
    fn request_keyframe(&mut self);
    /// 平均ビットレート (bps) を実行時に変更する。適応制御から呼ばれる。
    fn set_bitrate(&mut self, bitrate_bps: u32) -> anyhow::Result<()>;
    /// 投入済みだが未出力のフレームをエンコーダ内部から引き出す (低遅延フラッシュ)。
    /// 非同期 HW MFT は後続フレームの `submit` が無いと滞留フレームの `HaveOutput` を回収
    /// できず、静止画面で接続/キーフレーム要求が来ても keyframe NAL が出ないまま黒画面が
    /// 数秒続く。強制送出フレームを push した直後にこれを呼んで滞留分を即時に吐かせる。
    /// 同期エンコーダ (SW MFT / ffmpeg) は push 時点で出力済みのため既定 no-op。
    fn flush_pending(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// エンコーダ初期ビットレート (bps)。適応制御の開始点。
pub const DEFAULT_BITRATE_BPS: u32 = 8_000_000;
/// 適応制御で許容する下限 / 上限ビットレート (bps)。PLAN.md の帯域目標に基づく。
pub const MIN_BITRATE_BPS: u32 = 1_000_000;
pub const MAX_BITRATE_BPS: u32 = 15_000_000;

#[cfg(target_os = "windows")]
mod mf;
#[cfg(target_os = "windows")]
mod mf_hw;

/// Windows のプラットフォームエンコーダ。HW (NVENC/AMF/QSV) を優先し、初期化に失敗
/// したら SW (Microsoft H.264 MFT) へフォールバックする。どちらも GPU NV12 `Frame` を消費。
/// SW は H.264 のみ対応 (HEVC/AV1 の SW フォールバックは持たない)。
#[cfg(target_os = "windows")]
pub enum PlatformEncoder {
    Hw(mf_hw::MfHwVideoEncoder),
    Sw(mf::MfH264Encoder),
}

/// プラットフォームエンコーダを生成する。`encode_gpu` は使用 GPU の名前部分一致指定
/// (None = キャプチャと同一 GPU)。Optimus ノートでキャプチャ (iGPU) と別の dGPU (NVENC)
/// を使うための入口で、名前→LUID の解決と失敗時のフォールバックはここで行う。
///
/// `codec` は要求コーデック。HW MFT がその codec で初期化できなければ H.264 (SW) へ
/// フォールバックする。戻り値は `(エンコーダ, 実際に使われたコーデック)` — 呼び出し側は
/// 要求と食い違ったら (= フォールバック発生) ネゴシエーション状態を更新し直す必要がある。
#[cfg(target_os = "windows")]
pub fn create(
    device: SharedDevice,
    max_bitrate_bps: u32,
    encode_gpu: Option<&str>,
    codec: VideoCodec,
) -> anyhow::Result<(PlatformEncoder, VideoCodec)> {
    let encode_luid = encode_gpu.and_then(|q| match crate::capture::find_adapter_luid(q) {
        Some((luid, name)) => {
            tracing::info!(query = %q, gpu = %name, "encode GPU resolved (--encode-gpu)");
            Some(luid)
        }
        None => {
            tracing::warn!(query = %q, "encode GPU not found; using capture GPU");
            None
        }
    });
    if std::env::var_os("AURORA_FORCE_SW_ENCODER").is_some() {
        tracing::warn!("AURORA_FORCE_SW_ENCODER set: forcing software H.264 MFT");
        return Ok((
            PlatformEncoder::Sw(mf::MfH264Encoder::new(device, max_bitrate_bps)?),
            VideoCodec::H264,
        ));
    }
    match mf_hw::MfHwVideoEncoder::new(device.clone(), max_bitrate_bps, encode_luid, codec) {
        Ok(e) => {
            tracing::info!(?codec, "video encoder: hardware MFT (zero-copy NV12)");
            Ok((PlatformEncoder::Hw(e), codec))
        }
        Err(e) => {
            // SW フォールバックは H.264 のみ持つため、要求が H.264 以外だったらここで
            // コーデックが実質ダウングレードされる (呼び出し側が VideoCodecChanged を再送する)。
            tracing::warn!(
                error = %e,
                ?codec,
                "HW encoder unavailable for requested codec; falling back to software H.264 MFT (NV12 read-back)"
            );
            Ok((
                PlatformEncoder::Sw(mf::MfH264Encoder::new(device, max_bitrate_bps)?),
                VideoCodec::H264,
            ))
        }
    }
}

/// ホストが実際にエンコードできるコーデックの一覧を返す (実フレーム投入はせず、MFT の
/// activate + MediaType 設定まで試す)。起動時に 1 度だけ呼び、ネゴシエーションの
/// 「host 側能力」として使う。H.264 は SW フォールバックが必ずあるため常に含める。
#[cfg(target_os = "windows")]
pub fn probe_encoders(device: &SharedDevice, encode_gpu: Option<&str>) -> Vec<VideoCodec> {
    let encode_luid = encode_gpu.and_then(|q| crate::capture::find_adapter_luid(q).map(|(l, _)| l));
    let mut out = Vec::new();
    for codec in [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264] {
        if mf_hw::probe_hw_encoder(device, encode_luid, codec) {
            out.push(codec);
        }
    }
    // H.264 は SW MFT (Microsoft H.264 Encoder) が常に使えるため、HW 探索に失敗しても含める。
    if !out.contains(&VideoCodec::H264) {
        out.push(VideoCodec::H264);
    }
    out
}

#[cfg(target_os = "windows")]
impl VideoEncoder for PlatformEncoder {
    fn push_frame(&mut self, frame: &Frame) -> anyhow::Result<()> {
        match self {
            Self::Hw(e) => e.push_frame(frame),
            Self::Sw(e) => e.push_frame(frame),
        }
    }
    fn poll_nal(&mut self) -> anyhow::Result<Option<EncodedNal>> {
        match self {
            Self::Hw(e) => e.poll_nal(),
            Self::Sw(e) => e.poll_nal(),
        }
    }
    fn request_keyframe(&mut self) {
        match self {
            Self::Hw(e) => e.request_keyframe(),
            Self::Sw(e) => e.request_keyframe(),
        }
    }
    fn set_bitrate(&mut self, bitrate_bps: u32) -> anyhow::Result<()> {
        match self {
            Self::Hw(e) => e.set_bitrate(bitrate_bps),
            Self::Sw(e) => e.set_bitrate(bitrate_bps),
        }
    }
    fn flush_pending(&mut self) -> anyhow::Result<()> {
        match self {
            // SW MFT は同期出力なので既定 no-op で足りる。HW MFT (非同期) だけ実フラッシュ。
            Self::Hw(e) => e.flush_pending(),
            Self::Sw(e) => e.flush_pending(),
        }
    }
}

// Linux / macOS は ffmpeg-next 経由の H.264 エンコーダを共有する
// (Linux: nvenc/libx264, macOS: videotoolbox)。
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod ffmpeg;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use ffmpeg::FfmpegH264Encoder as PlatformEncoder;

/// 非 Windows: GPU 選択は未対応 (ffmpeg 側が実行時に選ぶ)。指定は無視する。
/// HEVC/AV1 ネゴシエーションは Windows (Media Foundation) のみ対応のため、要求コーデックに
/// 関わらず常に H.264 を使う (呼び出し側は戻り値の実効コーデックでネゴシエーション状態を保つ)。
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn create(
    device: crate::capture::SharedDevice,
    max_bitrate_bps: u32,
    _encode_gpu: Option<&str>,
    _codec: VideoCodec,
) -> anyhow::Result<(PlatformEncoder, VideoCodec)> {
    Ok((
        PlatformEncoder::new(device, max_bitrate_bps)?,
        VideoCodec::H264,
    ))
}

/// 非 Windows: H.264 のみ対応。
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn probe_encoders(
    _device: &crate::capture::SharedDevice,
    _encode_gpu: Option<&str>,
) -> Vec<VideoCodec> {
    vec![VideoCodec::H264]
}
