//! ffmpeg-next ベースの H.264 ハードウェアエンコーダ (Linux / macOS 共有)。
//!
//! 方針:
//! - エンコーダは実行時に名前で探索して選択する
//!   - Linux: `h264_nvenc` → `libx264` (フォールバック)
//!   - macOS: `h264_videotoolbox`
//!   - VAAPI はハードウェアフレーム (hwframe upload) の構築が必要で、本 MVP の
//!     CPU フレーム経路から外れるため対象外。将来 hwframe を導入する際に追加する。
//! - 入力 BGRA (キャプチャ) → NV12 へ swscale 変換し、CPU フレームを送る
//! - 出力は Annex-B (start code 付き)。SPS/PPS をキーフレーム毎に再送させ、
//!   クライアントの WebCodecs が途中接続でも復号できるようにする
//! - ビットレート変更はエンコーダを作り直して反映する (適応制御の頻度なら許容)。
//!   作り直し時は新しい SPS/PPS と IDR が自然に出るため復号は途切れない。

use std::collections::VecDeque;

use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg_next::util::frame::video::Video as VideoFrame;

use crate::capture::Frame;
use crate::encode::{DEFAULT_BITRATE_BPS, EncodedNal, VideoEncoder};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 60;
/// キーフレーム間隔 (フレーム数)。約 2 秒。
const GOP: u32 = FPS * 2;

/// プラットフォーム毎の優先エンコーダ名 (先頭から順に試す)。
#[cfg(target_os = "linux")]
const ENCODER_CANDIDATES: &[&str] = &["h264_nvenc", "libx264"];
#[cfg(target_os = "macos")]
const ENCODER_CANDIDATES: &[&str] = &["h264_videotoolbox"];

pub struct FfmpegH264Encoder {
    encoder: ffmpeg::encoder::video::Encoder,
    scaler: Scaler,
    /// 選択されたエンコーダ名 (作り直し時に再利用)。
    codec_name: String,
    bitrate_bps: u32,
    frame_id: u16,
    pts: i64,
    out_queue: VecDeque<EncodedNal>,
    force_keyframe: bool,
}

impl FfmpegH264Encoder {
    pub fn new() -> Result<Self> {
        ffmpeg::init().context("ffmpeg init")?;

        let (encoder, codec_name) = build_encoder(DEFAULT_BITRATE_BPS)?;
        let scaler = build_scaler()?;

        tracing::info!(encoder = %codec_name, "ffmpeg H.264 encoder selected");

        Ok(Self {
            encoder,
            scaler,
            codec_name,
            bitrate_bps: DEFAULT_BITRATE_BPS,
            frame_id: 0,
            pts: 0,
            out_queue: VecDeque::new(),
            force_keyframe: true,
        })
    }

    /// 現在の `out_queue` に溜まっていないパケットをエンコーダから吸い出す。
    fn drain_packets(&mut self, frame_id: u16) -> Result<()> {
        let mut packet = ffmpeg::Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    if let Some(data) = packet.data() {
                        self.out_queue.push_back(EncodedNal {
                            data: data.to_vec(),
                            keyframe: packet.is_key(),
                            frame_id,
                        });
                    }
                }
                // EAGAIN: もう取り出せるパケットがない。EOF: 終端。どちらも正常終了。
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => {
                    return Ok(());
                }
                Err(ffmpeg::Error::Eof) => return Ok(()),
                Err(e) => return Err(anyhow!("receive_packet: {e}")),
            }
        }
    }
}

impl VideoEncoder for FfmpegH264Encoder {
    fn push_frame(&mut self, frame: &Frame) -> Result<()> {
        if frame.width != WIDTH || frame.height != HEIGHT {
            return Err(anyhow!(
                "frame {}x{} != expected {}x{}",
                frame.width,
                frame.height,
                WIDTH,
                HEIGHT
            ));
        }

        // BGRA 入力フレームを作り、キャプチャの stride を尊重して行ごとにコピーする。
        let mut src = VideoFrame::new(Pixel::BGRA, WIDTH, HEIGHT);
        copy_bgra_into(&mut src, &frame.bgra, frame.stride as usize);

        // NV12 へ変換。
        let mut dst = VideoFrame::new(Pixel::NV12, WIDTH, HEIGHT);
        self.scaler
            .run(&src, &mut dst)
            .context("swscale BGRA->NV12")?;

        dst.set_pts(Some(self.pts));
        self.pts += 1;
        if self.force_keyframe {
            // キーフレームを強制 (IDR)。エンコーダはこれを見て SPS/PPS を再送する。
            dst.set_kind(ffmpeg::picture::Type::I);
            self.force_keyframe = false;
        }

        self.encoder.send_frame(&dst).context("send_frame")?;

        let fid = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);
        self.drain_packets(fid)?;
        Ok(())
    }

    fn poll_nal(&mut self) -> Result<Option<EncodedNal>> {
        Ok(self.out_queue.pop_front())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if bitrate_bps == self.bitrate_bps {
            return Ok(());
        }
        // ffmpeg のエンコーダは open 後のビットレート変更を多くが無視するため、
        // 同名エンコーダを目標ビットレートで作り直す。新規 SPS/PPS + IDR が出る。
        let (encoder, _name) = build_encoder_named(&self.codec_name, bitrate_bps)?;
        self.encoder = encoder;
        self.scaler = build_scaler()?;
        self.bitrate_bps = bitrate_bps;
        self.pts = 0;
        self.force_keyframe = true;
        tracing::info!(bitrate_bps, "ffmpeg encoder rebuilt for new bitrate");
        Ok(())
    }
}

/// BGRA バイト列を `dst` フレームへ stride を考慮して行コピーする。
fn copy_bgra_into(dst: &mut VideoFrame, bgra: &[u8], src_stride: usize) {
    let w = WIDTH as usize;
    let h = HEIGHT as usize;
    let dst_stride = dst.stride(0);
    let dst_data = dst.data_mut(0);
    let row_bytes = w * 4;
    for y in 0..h {
        let src_off = y * src_stride;
        let dst_off = y * dst_stride;
        if src_off + row_bytes <= bgra.len() && dst_off + row_bytes <= dst_data.len() {
            dst_data[dst_off..dst_off + row_bytes]
                .copy_from_slice(&bgra[src_off..src_off + row_bytes]);
        }
    }
}

fn build_scaler() -> Result<Scaler> {
    Scaler::get(
        Pixel::BGRA,
        WIDTH,
        HEIGHT,
        Pixel::NV12,
        WIDTH,
        HEIGHT,
        Flags::BILINEAR,
    )
    .context("create swscale context")
}

/// 候補リストの先頭から利用可能なエンコーダを選んで構築する。
fn build_encoder(bitrate_bps: u32) -> Result<(ffmpeg::encoder::video::Encoder, String)> {
    let mut last_err: Option<anyhow::Error> = None;
    for name in ENCODER_CANDIDATES {
        match build_encoder_named(name, bitrate_bps) {
            Ok((enc, n)) => return Ok((enc, n)),
            Err(e) => {
                tracing::warn!(encoder = name, error = %e, "encoder unavailable, trying next");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("no H.264 encoder candidates configured")))
}

fn build_encoder_named(
    name: &str,
    bitrate_bps: u32,
) -> Result<(ffmpeg::encoder::video::Encoder, String)> {
    let codec = ffmpeg::encoder::find_by_name(name)
        .ok_or_else(|| anyhow!("encoder '{name}' not found in this ffmpeg build"))?;

    let context = ffmpeg::codec::context::Context::new_with_codec(codec);
    let mut video = context.encoder().video().context("encoder().video()")?;

    video.set_width(WIDTH);
    video.set_height(HEIGHT);
    video.set_format(Pixel::NV12);
    video.set_time_base(ffmpeg::Rational(1, FPS as i32));
    video.set_frame_rate(Some(ffmpeg::Rational(FPS as i32, 1)));
    video.set_bit_rate(bitrate_bps as usize);
    video.set_max_bit_rate(bitrate_bps as usize);
    video.set_gop(GOP);
    // B フレームなしで遅延を最小化する。
    video.set_max_b_frames(0);

    let opts = encoder_options(name);
    let encoder = video
        .open_as_with(codec, opts)
        .with_context(|| format!("open encoder '{name}'"))?;
    Ok((encoder, name.to_string()))
}

/// エンコーダ毎の低遅延 / Annex-B ヘッダ再送オプション。
fn encoder_options(name: &str) -> ffmpeg::Dictionary<'static> {
    let mut d = ffmpeg::Dictionary::new();
    match name {
        "libx264" => {
            d.set("preset", "veryfast");
            d.set("tune", "zerolatency");
            // IDR 毎に SPS/PPS を再送 (途中接続クライアント対策)。
            d.set("x264-params", "repeat-headers=1");
        }
        "h264_nvenc" => {
            d.set("preset", "p1"); // 最低遅延プリセット
            d.set("tune", "ll"); // low latency
            d.set("rc", "cbr");
            d.set("zerolatency", "1");
            // キーフレーム毎に SPS/PPS/VPS を再送。
            d.set("repeat-headers", "1");
        }
        "h264_videotoolbox" => {
            d.set("realtime", "1");
            d.set("allow_sw", "1");
        }
        _ => {}
    }
    d
}

// SAFETY: エンコーダは encode スレッド 1 本でのみ生成・使用される。ffmpeg のエンコーダ
// コンテキストはスレッドローカルな所有を前提にしており、別スレッドへの move は安全。
unsafe impl Send for FfmpegH264Encoder {}
