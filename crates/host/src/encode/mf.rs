//! Media Foundation 同期 SW H.264 エンコーダ。
//!
//! MVP 方針:
//! - 同期 (SW) MFT を使い、HW MFT のメッセージシーケンスを避ける
//! - 入力 NV12 / 出力 H.264 (Annex-B start code 付き、MF のデフォルト)
//! - 1080p / 60fps / 8Mbps / Baseline 固定 (Step 9 で可変)
//! - BGRA→NV12 変換は CPU。GPU 化は Step 9

use std::collections::VecDeque;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFMediaType, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_LITE, MFStartup,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    eAVEncH264VProfile_Base,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::GUID;

/// MS H.264 Encoder MFT CLSID (CLSID_CMSH264EncoderMFT)。
/// windows-rs 0.58 では未エクスポートのため手定義。
const CLSID_CMSH264_ENCODER_MFT: GUID = GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);

/// MF_VERSION = (MF_SDK_VERSION << 16) | MF_API_VERSION = (2 << 16) | 0x0070
const MF_VERSION_CONST: u32 = (2u32 << 16) | 0x0070;

use crate::capture::Frame;
use crate::encode::{EncodedNal, VideoEncoder};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 60;
const BITRATE: u32 = 8_000_000;
const FRAME_DURATION_100NS: i64 = (10_000_000i64) / FPS as i64;

pub struct MfH264Encoder {
    transform: IMFTransform,
    frame_id: u16,
    sample_time_100ns: i64,
    out_queue: VecDeque<EncodedNal>,
    force_keyframe: bool,
    nv12_buf: Vec<u8>,
}

impl MfH264Encoder {
    pub fn new() -> Result<Self> {
        // SAFETY: COM の多重初期化は許可。同スレッドで以前 STA だと失敗するが MTA 期待。
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION_CONST, MFSTARTUP_LITE).context("MFStartup")?;
        }

        // SAFETY: CoCreateInstance は OUT param を初期化。CLSID は MS H.264 encoder。
        let transform: IMFTransform = unsafe {
            CoCreateInstance(&CLSID_CMSH264_ENCODER_MFT, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance(H264 encoder)")?
        };

        // 出力メディアタイプを先に設定 (MFT の規約)
        let out_type = make_h264_output_type()?;
        // SAFETY: transform live, type live
        unsafe { transform.SetOutputType(0, &out_type, 0).context("SetOutputType")? };

        let in_type = make_nv12_input_type()?;
        // SAFETY: as above
        unsafe { transform.SetInputType(0, &in_type, 0).context("SetInputType")? };

        // SAFETY: メッセージ送信は副作用のみ。
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("START_OF_STREAM")?;
        }

        Ok(Self {
            transform,
            frame_id: 0,
            sample_time_100ns: 0,
            out_queue: VecDeque::new(),
            force_keyframe: true, // 最初のフレームは強制 keyframe
            nv12_buf: vec![0u8; (WIDTH * HEIGHT * 3 / 2) as usize],
        })
    }

    fn drain_outputs(&mut self, current_frame_id: u16) -> Result<()> {
        // SAFETY: MFT live, out param 初期化される。
        let stream_info: MFT_OUTPUT_STREAM_INFO =
            unsafe { self.transform.GetOutputStreamInfo(0)? };

        loop {
            // MFT は自前で sample を作るパターン (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES) と
            // 呼び出し側が作るパターンがある。H264 encoder は呼び出し側提供。
            let sample = unsafe {
                let buffer = MFCreateMemoryBuffer(stream_info.cbSize.max(1024 * 1024))
                    .context("MFCreateMemoryBuffer (out)")?;
                let s = MFCreateSample().context("MFCreateSample (out)")?;
                s.AddBuffer(&buffer).context("AddBuffer (out)")?;
                s
            };

            let out_buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(Some(sample)),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let mut outs = [out_buf];

            // SAFETY: outs スライスは MFT が読み書きする。
            let r = unsafe { self.transform.ProcessOutput(0, &mut outs, &mut status) };
            match r {
                Ok(()) => {
                    let sample = unsafe { std::mem::ManuallyDrop::take(&mut outs[0].pSample) }
                        .ok_or_else(|| anyhow!("ProcessOutput returned no sample"))?;
                    // sample → contiguous buffer → bytes
                    let buf: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer()? };
                    let mut ptr: *mut u8 = std::ptr::null_mut();
                    let mut max_len = 0u32;
                    let mut cur_len = 0u32;
                    // SAFETY: Lock returns ptr+len; we copy then Unlock.
                    unsafe {
                        buf.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))
                            .context("MFMediaBuffer::Lock")?;
                    }
                    let mut data = vec![0u8; cur_len as usize];
                    unsafe {
                        std::ptr::copy_nonoverlapping(ptr, data.as_mut_ptr(), cur_len as usize);
                        let _ = buf.Unlock();
                    }

                    // keyframe 判定: NAL ユニットを走査 (Annex-B 想定)
                    let keyframe = is_keyframe_annexb(&data);

                    self.out_queue.push_back(EncodedNal {
                        data,
                        keyframe,
                        frame_id: current_frame_id,
                    });
                    // 続きがあるかもしれない
                    continue;
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    // pSample をクリーンアップ (ManuallyDrop)
                    unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pSample) };
                    unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pEvents) };
                    return Ok(());
                }
                Err(e) => {
                    unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pSample) };
                    unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pEvents) };
                    return Err(anyhow!("ProcessOutput: {e}"));
                }
            }
        }
    }
}

impl VideoEncoder for MfH264Encoder {
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
        bgra_to_nv12(&frame.bgra, frame.stride, WIDTH, HEIGHT, &mut self.nv12_buf);

        // IMFSample 生成
        let nv12_size = self.nv12_buf.len() as u32;
        // SAFETY: MF API; out params 初期化される。
        let (sample, buffer) = unsafe {
            let buffer = MFCreateMemoryBuffer(nv12_size).context("MFCreateMemoryBuffer (in)")?;
            let sample = MFCreateSample().context("MFCreateSample (in)")?;
            (sample, buffer)
        };

        // バッファに NV12 を書き込み
        unsafe {
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer
                .Lock(&mut ptr, None, None)
                .context("Lock (in buffer)")?;
            std::ptr::copy_nonoverlapping(self.nv12_buf.as_ptr(), ptr, self.nv12_buf.len());
            buffer.Unlock().context("Unlock (in buffer)")?;
            buffer
                .SetCurrentLength(nv12_size)
                .context("SetCurrentLength")?;
            sample.AddBuffer(&buffer).context("AddBuffer (in)")?;
            sample
                .SetSampleTime(self.sample_time_100ns)
                .context("SetSampleTime")?;
            sample
                .SetSampleDuration(FRAME_DURATION_100NS)
                .context("SetSampleDuration")?;

            if self.force_keyframe {
                use windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint;
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
                self.force_keyframe = false;
            }
        }
        self.sample_time_100ns += FRAME_DURATION_100NS;

        // SAFETY: MFT は sample を ref する。
        unsafe {
            self.transform
                .ProcessInput(0, &sample, 0)
                .context("ProcessInput")?;
        }

        let fid = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);
        self.drain_outputs(fid)?;
        Ok(())
    }

    fn poll_nal(&mut self) -> Result<Option<EncodedNal>> {
        Ok(self.out_queue.pop_front())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }
}

impl Drop for MfH264Encoder {
    fn drop(&mut self) {
        // SAFETY: cleanup; errors ignored.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

// SAFETY: MfH264Encoder は 1 つのスレッド (encode thread) でのみ使用される。
// IMFTransform は MTA で COM 初期化されており、別スレッドへの move は安全。
unsafe impl Send for MfH264Encoder {}

fn make_h264_output_type() -> Result<IMFMediaType> {
    // SAFETY: all API are OUT/SETTER.
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType (out)")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, BITRATE)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(WIDTH, HEIGHT))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(FPS, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
        Ok(t)
    }
}

fn make_nv12_input_type() -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType (in)")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(WIDTH, HEIGHT))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(FPS, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(t)
    }
}

fn pack_u64(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

/// BGRA8 (DXGI) → NV12 (BT.601 Limited)。MVP 用の素朴な実装。
fn bgra_to_nv12(bgra: &[u8], src_stride: u32, w: u32, h: u32, out: &mut [u8]) {
    let w = w as usize;
    let h = h as usize;
    let src_stride = src_stride as usize;
    let y_plane_size = w * h;
    let (y_plane, uv_plane) = out.split_at_mut(y_plane_size);

    for j in 0..h {
        let src_row = &bgra[j * src_stride..j * src_stride + w * 4];
        for i in 0..w {
            let b = src_row[i * 4] as i32;
            let g = src_row[i * 4 + 1] as i32;
            let r = src_row[i * 4 + 2] as i32;
            // BT.601
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[j * w + i] = y.clamp(0, 255) as u8;
        }
    }
    // UV: 2x2 ダウンサンプリング
    for j in (0..h).step_by(2) {
        for i in (0..w).step_by(2) {
            let mut sb = 0i32;
            let mut sg = 0i32;
            let mut sr = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let row = &bgra[(j + dy) * src_stride..];
                    let p = (i + dx) * 4;
                    sb += row[p] as i32;
                    sg += row[p + 1] as i32;
                    sr += row[p + 2] as i32;
                }
            }
            let b = sb / 4;
            let g = sg / 4;
            let r = sr / 4;
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            let uv_row = j / 2;
            let uv_col = (i / 2) * 2;
            let idx = uv_row * w + uv_col;
            uv_plane[idx] = u.clamp(0, 255) as u8;
            uv_plane[idx + 1] = v.clamp(0, 255) as u8;
        }
    }
}

/// Annex-B NAL 列を走査して IDR (nal_unit_type=5) を含むか判定。
fn is_keyframe_annexb(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < buf.len() {
        let start3 = buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1;
        let start4 = buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 0 && buf[i + 3] == 1;
        if start3 || start4 {
            let nal_start = i + if start4 { 4 } else { 3 };
            if nal_start < buf.len() {
                let nal_type = buf[nal_start] & 0x1f;
                if nal_type == 5 {
                    return true;
                }
            }
            i = nal_start;
        } else {
            i += 1;
        }
    }
    false
}
