//! ffmpeg-next ベースの H.264 ハードウェアエンコーダ (Linux / macOS 共有)。
//!
//! 方針:
//! - エンコーダは実行時に名前で探索して選択する
//!   - Linux: `h264_nvenc` → `libx264` (フォールバック)
//!   - macOS: `h264_videotoolbox`
//!   - VAAPI はハードウェアフレーム (hwframe upload) の構築が必要で、本 MVP の
//!     CPU フレーム経路から外れるため対象外。将来 hwframe を導入する際に追加する。
//! - 解像度は最初のフレームに追従し、変化したらエンコーダを作り直す
//! - 入力 BGRA (キャプチャ) → NV12 へ swscale 変換し、CPU フレームを送る
//! - 出力は Annex-B。SPS/PPS をキーフレーム毎に再送させ途中接続でも復号可能にする
//! - ビットレート変更はエンコーダを作り直して反映する (適応制御の頻度なら許容)

use std::collections::VecDeque;

use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg_next::util::frame::video::Video as VideoFrame;

use crate::capture::Frame;
use crate::encode::{DEFAULT_BITRATE_BPS, EncodedNal, VideoEncoder};

const FPS: u32 = 60;
/// キーフレーム間隔 (フレーム数)。約 2 秒。
const GOP: u32 = FPS * 2;

/// プラットフォーム毎の優先エンコーダ名 (先頭から順に試す)。
#[cfg(target_os = "linux")]
const ENCODER_CANDIDATES: &[&str] = &["h264_nvenc", "libx264"];
#[cfg(target_os = "macos")]
const ENCODER_CANDIDATES: &[&str] = &["h264_videotoolbox"];

pub struct FfmpegH264Encoder {
    /// 実エンコーダは最初のフレーム解像度が分かってから構築する。
    inner: Option<Inner>,
    /// 一度選択したエンコーダ名 (再構築時に再利用)。
    codec_name: Option<String>,
    bitrate_bps: u32,
    frame_id: u16,
    force_keyframe: bool,
}

/// 特定解像度に紐づくエンコーダ + スケーラ。
struct Inner {
    encoder: ffmpeg::encoder::video::Encoder,
    scaler: Scaler,
    width: u32,
    height: u32,
    pts: i64,
    out_queue: VecDeque<EncodedNal>,
}

impl FfmpegH264Encoder {
    /// `_device` は Windows 専用の共有 D3D11 デバイス (mac/linux では `()`)。
    /// `_max_bitrate_bps` のピーク上限適用は mac/linux 側では未対応 (signature 整合のみ)。
    pub fn new(_device: crate::capture::SharedDevice, _max_bitrate_bps: u32) -> Result<Self> {
        ffmpeg::init().context("ffmpeg init")?;
        Ok(Self {
            inner: None,
            codec_name: None,
            bitrate_bps: DEFAULT_BITRATE_BPS,
            frame_id: 0,
            force_keyframe: true,
        })
    }

    /// 指定解像度の Inner を保証する。未構築または解像度変化時に作り直す。
    fn ensure_inner(&mut self, width: u32, height: u32) -> Result<()> {
        let need_new = match &self.inner {
            Some(inner) => inner.width != width || inner.height != height,
            None => true,
        };
        if need_new {
            let (inner, name) =
                Inner::new(width, height, self.bitrate_bps, self.codec_name.as_deref())?;
            tracing::info!(width, height, encoder = %name, "ffmpeg H.264 encoder built");
            self.codec_name = Some(name);
            self.inner = Some(inner);
            self.force_keyframe = true;
        }
        Ok(())
    }
}

impl Inner {
    /// `preferred` が指定されればそのエンコーダを使い、無ければ候補から探索する。
    fn new(
        width: u32,
        height: u32,
        bitrate_bps: u32,
        preferred: Option<&str>,
    ) -> Result<(Self, String)> {
        let (encoder, name) = match preferred {
            Some(name) => (
                build_encoder_named(name, width, height, bitrate_bps)?,
                name.to_string(),
            ),
            None => build_encoder(width, height, bitrate_bps)?,
        };
        let scaler = build_scaler(width, height)?;
        Ok((
            Self {
                encoder,
                scaler,
                width,
                height,
                pts: 0,
                out_queue: VecDeque::new(),
            },
            name,
        ))
    }

    /// エンコーダから取り出せるパケットを全て out_queue へ移す。
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
        let fid = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);

        // ensure_inner は再構築時に force_keyframe を立てるので、その後に読む。
        self.ensure_inner(frame.width, frame.height)?;
        let force_keyframe = self.force_keyframe;
        self.force_keyframe = false;
        let inner = self.inner.as_mut().unwrap();
        let (w, h) = (inner.width, inner.height);

        // BGRA 入力フレームを作り、キャプチャの stride を尊重して行ごとにコピーする。
        let mut src = VideoFrame::new(Pixel::BGRA, w, h);
        copy_bgra_into(&mut src, &frame.bgra, frame.stride as usize, w, h);

        // NV12 へ変換。
        let mut dst = VideoFrame::new(Pixel::NV12, w, h);
        inner
            .scaler
            .run(&src, &mut dst)
            .context("swscale BGRA->NV12")?;

        dst.set_pts(Some(inner.pts));
        inner.pts += 1;
        if force_keyframe {
            // キーフレームを強制 (IDR)。エンコーダはこれを見て SPS/PPS を再送する。
            dst.set_kind(ffmpeg::picture::Type::I);
        }

        inner.encoder.send_frame(&dst).context("send_frame")?;
        inner.drain_packets(fid)?;
        Ok(())
    }

    fn poll_nal(&mut self) -> Result<Option<EncodedNal>> {
        Ok(self.inner.as_mut().and_then(|i| i.out_queue.pop_front()))
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if bitrate_bps == self.bitrate_bps {
            return Ok(());
        }
        self.bitrate_bps = bitrate_bps;
        // ffmpeg のエンコーダは open 後のビットレート変更を多くが無視するため、
        // 現在の解像度で作り直す。新規 SPS/PPS + IDR が出る。
        if let Some(inner) = &self.inner {
            let (w, h) = (inner.width, inner.height);
            let name = self.codec_name.clone();
            let (rebuilt, _name) = Inner::new(w, h, bitrate_bps, name.as_deref())?;
            self.inner = Some(rebuilt);
            self.force_keyframe = true;
            tracing::info!(bitrate_bps, "ffmpeg encoder rebuilt for new bitrate");
        }
        Ok(())
    }
}

/// BGRA バイト列を `dst` フレームへ stride を考慮して行コピーする。
fn copy_bgra_into(dst: &mut VideoFrame, bgra: &[u8], src_stride: usize, w: u32, h: u32) {
    let w = w as usize;
    let h = h as usize;
    let dst_stride = dst.stride(0);
    let dst_data = dst.data_mut(0);
    let row_bytes = w * 4;
    for y in 0..h {
        let src_off = y * src_stride;
        let dst_off = y * dst_stride;
        if src_off + row_bytes > bgra.len() || dst_off + row_bytes > dst_data.len() {
            // 想定外のバッファ不足 (stride×height と実バッファ長の不一致)。黙って行を
            // 落とすと「フレーム下端の乱れ」の原因調査が困難になるため、警告して打ち切る
            // (このフレームは不完全だが次フレームで回復する)。
            tracing::warn!(
                y,
                h,
                src_len = bgra.len(),
                dst_len = dst_data.len(),
                "BGRA row copy out of bounds; truncating frame"
            );
            return;
        }
        dst_data[dst_off..dst_off + row_bytes].copy_from_slice(&bgra[src_off..src_off + row_bytes]);
    }
}

fn build_scaler(width: u32, height: u32) -> Result<Scaler> {
    Scaler::get(
        Pixel::BGRA,
        width,
        height,
        Pixel::NV12,
        width,
        height,
        Flags::BILINEAR,
    )
    .context("create swscale context")
}

/// 候補リストの先頭から利用可能なエンコーダを選んで構築する。
fn build_encoder(
    width: u32,
    height: u32,
    bitrate_bps: u32,
) -> Result<(ffmpeg::encoder::video::Encoder, String)> {
    let mut last_err: Option<anyhow::Error> = None;
    for name in ENCODER_CANDIDATES {
        match build_encoder_named(name, width, height, bitrate_bps) {
            Ok(enc) => return Ok((enc, name.to_string())),
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
    width: u32,
    height: u32,
    bitrate_bps: u32,
) -> Result<ffmpeg::encoder::video::Encoder> {
    let codec = ffmpeg::encoder::find_by_name(name)
        .ok_or_else(|| anyhow!("encoder '{name}' not found in this ffmpeg build"))?;

    let context = ffmpeg::codec::context::Context::new_with_codec(codec);
    let mut video = context.encoder().video().context("encoder().video()")?;

    video.set_width(width);
    video.set_height(height);
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
    Ok(encoder)
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
            // キーフレーム毎に SPS/PPS を再送。
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
