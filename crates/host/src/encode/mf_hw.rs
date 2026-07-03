//! Media Foundation 非同期 HW H.264 エンコーダ (NVIDIA NVENC / AMD AMF / Intel QSV)。
//!
//! `MFTEnumEx(MFT_ENUM_FLAG_HARDWARE)` でベンダ非依存に HW MFT を選び、
//! `IMFDXGIDeviceManager` 経由で共有 D3D11 デバイスを渡して **GPU 上の NV12 テクスチャ
//! を直接** `MFCreateDXGISurfaceBuffer` でラップして食わせる (ゼロコピー)。
//!
//! エンコーダ選択は既定で**キャプチャデバイスと同一 LUID** の MFT に限定する
//! (`MFTEnum2` + `MFT_ENUM_ADAPTER_LUID`。別 GPU の MFT を誤選択して映像が全滅するのを防ぐ)。
//! `--encode-gpu` で別 GPU を指定した場合のみクロスアダプタ経路になり、D3D マネージャを
//! 渡さず staging readback → システムメモリサンプルで入力する (ゼロコピー禁則の意図的例外。
//! Optimus ノートの iGPU キャプチャ + dGPU NVENC 用)。
//!
//! HW MFT は非同期 (async) なので `IMFMediaEventGenerator` を `GetEvent` (ブロッキング)
//! で回し、`METransformNeedInput`→`ProcessInput` / `METransformHaveOutput`→`ProcessOutput`
//! を駆動する。encode スレッドは 1 本なので IMFAsyncCallback は使わない。
//!
//! 出力は Annex-B (start code + in-band SPS/PPS) を前提とする (client `decoder.ts` 互換)。

use std::collections::VecDeque;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{FALSE, LUID};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMF2DBuffer,
    IMF2DBuffer2, IMFActivate, IMFAttributes, IMFMediaBuffer, IMFMediaEventGenerator, IMFSample,
    IMFTransform, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, METransformHaveOutput,
    METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_TRANSFORM_ASYNC_UNLOCK, MF2DBuffer_LockFlags_Write,
    MFCreate2DMediaBuffer, MFCreateAttributes, MFCreateDXGISurfaceBuffer, MFCreateSample,
    MFMediaType_Video, MFSampleExtension_CleanPoint, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnum2, MFVideoFormat_H264,
    MFVideoFormat_NV12,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{Interface, PWSTR};

use crate::capture::Frame;
use crate::capture::SharedDevice;
use crate::encode::mf::{
    FRAME_DURATION_100NS, configure_low_latency, configure_rate_control, is_keyframe_annexb,
    make_h264_output_type, make_nv12_input_type, set_vbv_buffer, variant_u32,
};
use crate::encode::{DEFAULT_BITRATE_BPS, EncodedNal, VideoEncoder};

/// `IMFMediaEventGenerator::GetEvent` のノンブロッキング取得フラグ (MF_EVENT_FLAG_NO_WAIT)。
/// キューが空なら待たずに `MF_E_NO_EVENTS_AVAILABLE` を返す。
const MF_EVENT_FLAG_NO_WAIT: u32 = 0x0000_0001;

pub struct MfHwH264Encoder {
    device: SharedDevice,
    inner: Option<Inner>,
    bitrate: u32,
    /// ピーク (最大) ビットレート上限 (bps)。PeakConstrainedVBR の上限。
    max_bitrate: u32,
    frame_id: u16,
    force_keyframe: bool,
    /// エンコードに使う GPU の LUID (未指定ならキャプチャデバイスと同一 GPU)。
    encode_luid: LUID,
    /// `encode_luid` がキャプチャ共有デバイスと異なる GPU か。true ならクロスアダプタ
    /// (CPU 経由) 入力経路になる。
    cross: bool,
}

struct Inner {
    transform: IMFTransform,
    event_gen: IMFMediaEventGenerator,
    /// 実行時パラメータ変更 (ビットレート等)。HW MFT によっては未提供。
    codec_api: Option<ICodecAPI>,
    width: u32,
    height: u32,
    bitrate: u32,
    sample_time_100ns: i64,
    out_queue: VecDeque<EncodedNal>,
    /// ProcessInput 済みで未だ出力されていないフレーム ID (出力順マッピング用 FIFO)。
    in_flight: VecDeque<u16>,
    /// 受信済みで未消化の `METransformNeedInput` 数 (= MFT の空き入力スロット)。
    /// submit 呼び出しをまたいで保持し、トークンを 1 個も捨てないことでストールを防ぐ。
    pending_need_input: u32,
    /// クロスアダプタ入力用の staging readback (`build_cpu_sample` 専用)。共有デバイス
    /// (キャプチャ側 GPU) の `Arc` を保持する。
    device: SharedDevice,
    /// true ならエンコーダは別 GPU 上にあり、入力は CPU 経由 (staging readback) になる。
    cross: bool,
    /// クロスアダプタ readback 用の staging テクスチャ 2 面 (交互使用)。cross=false では未使用。
    staging: [Option<ID3D11Texture2D>; 2],
    /// 次回 `build_cpu_sample` が使う `staging` の面インデックス (0/1 交互)。
    staging_idx: usize,
}

impl MfHwH264Encoder {
    pub fn new(device: SharedDevice, max_bitrate: u32, encode_luid: Option<LUID>) -> Result<Self> {
        crate::capture::device::init_thread_com()?;
        // 起動時に HW H.264 エンコーダの存在を確認する。無ければ Err を返し、呼び出し側
        // (PlatformEncoder::new) が SW MFT へフォールバックできるようにする。ここで確認
        // しないと、HW なし環境で push_frame が毎フレーム失敗し続けてフォールバックが
        // 発動しない (実 MFT は解像度確定後に Inner::new で改めて構築する)。列挙のみで
        // インスタンス化はしない (エンコーダセッションを二重に確保しないため)。
        if !hw_h264_encoder_present()? {
            return Err(anyhow!("no hardware H.264 encoder MFT found"));
        }
        let encode_luid = encode_luid.unwrap_or(device.adapter_luid);
        let cross = encode_luid != device.adapter_luid;
        Ok(Self {
            device,
            inner: None,
            bitrate: DEFAULT_BITRATE_BPS.min(max_bitrate),
            max_bitrate,
            frame_id: 0,
            force_keyframe: true,
            encode_luid,
            cross,
        })
    }

    fn ensure_inner(&mut self, width: u32, height: u32) -> Result<()> {
        let need_new = match &self.inner {
            Some(inner) => inner.width != width || inner.height != height,
            None => true,
        };
        if need_new {
            tracing::info!(
                width,
                height,
                bitrate = self.bitrate,
                "building HW H.264 MFT"
            );
            self.inner = match Inner::new(
                &self.device,
                width,
                height,
                self.bitrate,
                self.max_bitrate,
                self.encode_luid,
                self.cross,
            ) {
                Ok(inner) => Some(inner),
                // dGPU が省電力設定/ドライバ不備等で掴めない環境でも映像を止めないため、
                // 一度だけキャプチャ GPU 側 (常に掴めるはずの経路) へ切り替えて再試行する。
                Err(e) if self.cross => {
                    tracing::warn!(
                        error = %e,
                        "cross-GPU HW encoder init failed; falling back to capture GPU encoder"
                    );
                    self.cross = false;
                    self.encode_luid = self.device.adapter_luid;
                    Some(Inner::new(
                        &self.device,
                        width,
                        height,
                        self.bitrate,
                        self.max_bitrate,
                        self.encode_luid,
                        self.cross,
                    )?)
                }
                Err(e) => return Err(e),
            };
            self.force_keyframe = true;
        }
        Ok(())
    }
}

impl Inner {
    fn new(
        device: &SharedDevice,
        width: u32,
        height: u32,
        bitrate: u32,
        max_bitrate: u32,
        target_luid: LUID,
        cross: bool,
    ) -> Result<Self> {
        let (transform, friendly) = enum_hw_encoder(target_luid)?;
        tracing::info!(encoder = %friendly, cross, "selected HW H.264 MFT");

        // async HW MFT を使う前にアンロックする (必須)。
        // SAFETY: transform live。GetAttributes は IMFAttributes を返す。
        unsafe {
            let attrs = transform
                .GetAttributes()
                .context("IMFTransform::GetAttributes")?;
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .context("set MF_TRANSFORM_ASYNC_UNLOCK")?;
        }

        // 共有 D3D11 デバイスを MFT に渡す (ゼロコピー入力の前提)。クロスアダプタ (別 GPU の
        // MFT) では、このデバイスマネージャは対象 GPU から掴めず設定しても無意味 (最悪エラー)
        // なので渡さない。入力はシステムメモリサンプル (build_cpu_sample) に切り替える。
        if !cross {
            // SAFETY: manager は live。ulparam は IUnknown ポインタ。MFT が AddRef する。
            unsafe {
                let manager_ptr = device.manager.as_raw() as usize;
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager_ptr)
                    .context("MFT_MESSAGE_SET_D3D_MANAGER")?;
            }
        }

        // 出力 (H.264) → 入力 (NV12) の順に設定する。
        let out_type = make_h264_output_type(width, height, bitrate)?;
        // SAFETY: transform / type live。
        unsafe {
            transform
                .SetOutputType(0, &out_type, 0)
                .context("SetOutputType(H264)")?;
        }
        let in_type = make_nv12_input_type(width, height)?;
        // SAFETY: as above
        unsafe {
            transform
                .SetInputType(0, &in_type, 0)
                .context("SetInputType(NV12)")?;
        }

        // ICodecAPI は任意。低遅延モード + レート制御 (CBR 平均一定) を設定。
        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        if let Some(api) = &codec_api {
            // 低遅延モード + 大きな GOP (周期 IDR 抑制) + レート制御 (CBR)。
            configure_low_latency(api);
            configure_rate_control(api, bitrate, max_bitrate);
        }

        let event_gen: IMFMediaEventGenerator = transform
            .cast()
            .context("QueryInterface IMFMediaEventGenerator")?;

        // SAFETY: ストリーミング開始通知。これ以降 MFT が NeedInput イベントを出す。
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
            event_gen,
            codec_api,
            width,
            height,
            bitrate,
            sample_time_100ns: 0,
            out_queue: VecDeque::new(),
            in_flight: VecDeque::new(),
            pending_need_input: 0,
            device: device.clone(),
            cross,
            staging: [None, None],
            staging_idx: 0,
        })
    }

    /// NV12 テクスチャを IMFSample 化して投入し、エンコーダが次入力を要求するまで
    /// イベントを回す (途中の出力は out_queue に積む)。
    fn submit(&mut self, texture: &ID3D11Texture2D, fid: u16, force_keyframe: bool) -> Result<()> {
        let sample = if self.cross {
            self.build_cpu_sample(texture)?
        } else {
            self.build_gpu_sample(texture)?
        };
        // SAFETY: sample は直前に構築した live な IMFSample。各 Set* は値をコピーする。
        unsafe {
            sample
                .SetSampleTime(self.sample_time_100ns)
                .context("SetSampleTime")?;
            sample
                .SetSampleDuration(FRAME_DURATION_100NS)
                .context("SetSampleDuration")?;
            if force_keyframe {
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }
        }
        self.sample_time_100ns += FRAME_DURATION_100NS;

        // このフレームの入力を 1 つだけ投入する。NeedInput トークン (空き入力スロット通知) は
        // submit をまたいで `pending_need_input` に貯め、**1 個も捨てない**。以前は投入後に
        // 次の NeedInput を受けて return しており、1 フレームにつき 2 個消費 (1 個破棄) して
        // いたため、MFT が起動時に先出しするスロット数ぶんを使い切ると GetEvent が永久ブロック
        // していた (数フレームでストール)。
        loop {
            if self.pending_need_input > 0 {
                self.pending_need_input -= 1;
                // SAFETY: MFT は sample を ref する。
                unsafe { self.transform.ProcessInput(0, &sample, 0) }.context("ProcessInput")?;
                self.in_flight.push_back(fid);
                break;
            }
            // 空きスロットが無ければブロッキングで待つ。待つ間に来た出力はドレインする。
            // SAFETY: event_gen live。dwflags=0 でブロッキング取得。
            let event = unsafe {
                self.event_gen
                    .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
            }
            .context("GetEvent")?;
            self.dispatch_event(&event)?;
        }

        // 既にキューにあるイベントをノンブロッキングで吸い出し、出力を速やかに流しつつ
        // 後続フレーム用の NeedInput トークンを貯める。
        loop {
            // SAFETY: event_gen live。NO_WAIT は空なら MF_E_NO_EVENTS_AVAILABLE を返す。
            let event = match unsafe {
                self.event_gen
                    .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(MF_EVENT_FLAG_NO_WAIT))
            } {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(anyhow!("GetEvent(nowait): {e}")),
            };
            self.dispatch_event(&event)?;
        }
        Ok(())
    }

    /// 既定経路: 同一 GPU 上の共有 D3D11 テクスチャをそのまま MFT に渡す (ゼロコピー)。
    fn build_gpu_sample(&self, texture: &ID3D11Texture2D) -> Result<IMFSample> {
        let nv12_size = self.width * self.height * 3 / 2;
        // SAFETY: texture は共有デバイス上の live な NV12。riid は ID3D11Texture2D。
        unsafe {
            let buffer: IMFMediaBuffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, FALSE)
                    .context("MFCreateDXGISurfaceBuffer")?;
            // 入力長を設定 (2D バッファの連続長、無ければ NV12 サイズ)。
            let len = match buffer.cast::<IMF2DBuffer>() {
                Ok(b2) => b2.GetContiguousLength().unwrap_or(nv12_size),
                Err(_) => nv12_size,
            };
            let _ = buffer.SetCurrentLength(len);

            let sample = MFCreateSample().context("MFCreateSample (in)")?;
            sample.AddBuffer(&buffer).context("AddBuffer (in)")?;
            Ok(sample)
        }
    }

    /// クロスアダプタ経路: staging へ readback した NV12 をシステムメモリサンプルとして渡す。
    ///
    /// ゼロコピー禁則 (CLAUDE.md §6) の意図的な例外。クロスアダプタでは D3D11 テクスチャを
    /// 別 GPU の MFT と共有できず、CPU 経由 (staging readback → システムメモリサンプル) が
    /// 唯一の経路。Optimus ノートで iGPU キャプチャ + dGPU NVENC を使うための追加コスト
    /// (1080p60 の NV12 で ~190MB/s) を意図して払う。
    fn build_cpu_sample(&mut self, texture: &ID3D11Texture2D) -> Result<IMFSample> {
        let idx = self.staging_idx;
        self.staging_idx = (self.staging_idx + 1) % 2;

        if self.staging[idx].is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            // SAFETY: device live、desc 全フィールド初期化済み。
            unsafe {
                self.device
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .context("CreateTexture2D(cross-GPU NV12 staging)")?;
            }
            self.staging[idx] = Some(tex.ok_or_else(|| anyhow!("cross-GPU staging texture null"))?);
        }
        // Map は直前フレームの GPU コピー完了までブロックするため、staging を 2 面交互に
        // 使って前フレームの完了待ちと今回のコピーを重ねる (完全な隠蔽ではないが、単一面より
        // ストールしにくい)。
        let staging = self.staging[idx]
            .as_ref()
            .ok_or_else(|| anyhow!("staging texture missing"))?;

        // SAFETY: staging/texture は同一デバイス・同フォーマット・同サイズ (NV12)。
        unsafe { self.device.context.CopyResource(staging, texture) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging は STAGING + CPU_ACCESS_READ。Map は安全に呼べる。
        unsafe {
            self.device
                .context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map(cross-GPU NV12 staging)")?;
        }

        let result = self.copy_mapped_nv12_into_2d_buffer(&mapped);

        // SAFETY: 直前の Map に対応する Unmap。コピーの成否によらず必ず呼ぶ (漏れ防止)。
        unsafe { self.device.context.Unmap(staging, 0) };
        result
    }

    /// `build_cpu_sample` の内側: Map 済みの staging から 2D メディアバッファへ NV12 をコピーし
    /// IMFSample を組み立てる。Unlock2D/Unmap を確実に対にするため、Lock2DSize 以降の失敗経路
    /// でも Unlock2D を必ず呼んでから return する。
    fn copy_mapped_nv12_into_2d_buffer(
        &self,
        mapped: &D3D11_MAPPED_SUBRESOURCE,
    ) -> Result<IMFSample> {
        let w = self.width;
        let h = self.height;
        let src_pitch = mapped.RowPitch as usize;

        // SAFETY: MFCreate2DMediaBuffer は幅高さから 2D バッファを確保する。fbottomup=false
        // (NV12 はトップダウンのみ想定)。
        let buffer: IMFMediaBuffer =
            unsafe { MFCreate2DMediaBuffer(w, h, MFVideoFormat_NV12.data1, false) }
                .context("MFCreate2DMediaBuffer")?;
        let buf2d: IMF2DBuffer2 = buffer.cast().context("cast IMF2DBuffer2")?;

        let mut scanline0: *mut u8 = std::ptr::null_mut();
        let mut pitch: i32 = 0;
        // バッファ全体の先頭 (アライメント等で scanline0 と異なりうる)。範囲検証は buf_len のみ
        // で行うため使わないが、Lock2DSize の OUT パラメータとして必須。
        let mut _buf_start: *mut u8 = std::ptr::null_mut();
        let mut buf_len: u32 = 0;
        // SAFETY: buf2d は live。Lock2DSize は各 OUT パラメータを埋める。
        unsafe {
            buf2d
                .Lock2DSize(
                    MF2DBuffer_LockFlags_Write,
                    &mut scanline0,
                    &mut pitch,
                    &mut _buf_start,
                    &mut buf_len,
                )
                .context("Lock2DSize")?;
        }

        // Lock2DSize 成功後は、この後のどの経路でも Unlock2D を必ず呼ぶ (早期 return しない)。
        let copy_result: Result<()> = if pitch < 0 {
            // NV12 は bottom-up を扱わない (想定外個体)。
            Err(anyhow!(
                "unexpected bottom-up NV12 2D buffer (negative pitch)"
            ))
        } else {
            let dst_pitch = pitch as usize;
            let need = dst_pitch * h as usize * 3 / 2;
            if (buf_len as usize) < need {
                Err(anyhow!("2D buffer too small: have {buf_len} need {need}"))
            } else {
                let src = mapped.pData as *const u8;
                // SAFETY: src は Map 済み staging の有効範囲 (h 行 + h/2 行、各 src_pitch
                // バイト)。dst (scanline0 起点) は Lock2DSize が返した buf_len (>= need を
                // 検証済み) の範囲内。
                unsafe {
                    for row in 0..h as usize {
                        std::ptr::copy_nonoverlapping(
                            src.add(row * src_pitch),
                            scanline0.add(row * dst_pitch),
                            w as usize,
                        );
                    }
                    let src_uv = src.add(src_pitch * h as usize);
                    let dst_uv = scanline0.add(dst_pitch * h as usize);
                    for row in 0..(h as usize) / 2 {
                        std::ptr::copy_nonoverlapping(
                            src_uv.add(row * src_pitch),
                            dst_uv.add(row * dst_pitch),
                            w as usize,
                        );
                    }
                }
                Ok(())
            }
        };

        // SAFETY: Lock2DSize 成功と対になる Unlock2D。コピーの成否によらず必ず呼ぶ。
        unsafe {
            let _ = buf2d.Unlock2D();
        }
        copy_result?;

        let len = w * h * 3 / 2;
        // SAFETY: buffer live。SetCurrentLength は長さのみ更新する。
        unsafe {
            buffer.SetCurrentLength(len).context("SetCurrentLength")?;
        }

        // SAFETY: buffer/sample は live。
        unsafe {
            let sample = MFCreateSample().context("MFCreateSample (cross)")?;
            sample.AddBuffer(&buffer).context("AddBuffer (cross)")?;
            Ok(sample)
        }
    }

    /// 投入済み (`in_flight`) だが未出力のフレームを、後続フレームの `submit` を待たずに
    /// 引き出す。非同期 HW MFT は `ProcessInput` 後しばらくして別スレッドで `HaveOutput` を
    /// 上げるため、`submit` 末尾のノンブロッキング吸い出しでは間に合わず滞留する。静止画面で
    /// 接続/キーフレーム要求が来たときに呼び、`in_flight` が空になるまでイベントループを
    /// 回して keyframe NAL を即時に吐かせる (黒画面が数秒残る問題の本対策)。
    ///
    /// `MFT_MESSAGE_COMMAND_DRAIN` は使わない: それはストリーム終了→再 START の重い遷移で、
    /// NeedInput トークン体系をリセットしてしまう。ここは submit と同じ `dispatch_event` 機構を
    /// 流用し、低遅延設定 (B フレーム/先読み無効) ゆえ後続入力なしに出る `HaveOutput` を回収する
    /// だけにとどめる。万一出力が出ないベンダ MFT でも締切で必ず抜け、従来挙動に劣化するのみ。
    fn pump_pending_outputs(&mut self) -> Result<()> {
        // 滞留 (in_flight) のうち最低 1 枚が出力されるまで回す。呼び出し側が直前に「後続入力」を
        // 1 枚与えているので、滞留していた IDR がこの 1 枚で押し出される。全滅まで待つと、押し出し
        // 用に与えた入力自身がさらに次の入力を要して締切まで空回りするため、1 枚回収で抜ける。
        let start_len = self.in_flight.len();
        if start_len == 0 {
            return Ok(());
        }
        // 低遅延 HW では HaveOutput は数 ms 以内に出る。締切でブロックを必ず有界化する
        // (encode スレッドを止めないため。出ないなら諦めて従来の数秒回復にフォールバック)。
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while self.in_flight.len() >= start_len && std::time::Instant::now() < deadline {
            // SAFETY: event_gen live。NO_WAIT は空なら MF_E_NO_EVENTS_AVAILABLE を返す。
            match unsafe {
                self.event_gen
                    .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(MF_EVENT_FLAG_NO_WAIT))
            } {
                Ok(ev) => self.dispatch_event(&ev)?,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    // まだ出力が用意されていない。HW の完了をわずかに待って再試行する。
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => return Err(anyhow!("GetEvent(nowait, flush): {e}")),
            }
        }
        Ok(())
    }

    /// MFT イベントを種別ごとに処理する。NeedInput はトークンとして加算 (消費は submit 側)、
    /// HaveOutput は即ドレインする。その他は無視する。
    fn dispatch_event(
        &mut self,
        event: &windows::Win32::Media::MediaFoundation::IMFMediaEvent,
    ) -> Result<()> {
        // SAFETY: event は live。GetType は値を返す。
        let et = unsafe { event.GetType() }.context("event GetType")?;
        if et == METransformNeedInput.0 as u32 {
            self.pending_need_input += 1;
        } else if et == METransformHaveOutput.0 as u32 {
            self.process_output()?;
        }
        Ok(())
    }

    fn process_output(&mut self) -> Result<()> {
        // SAFETY: MFT live。OUT を埋める。
        let stream_info = unsafe { self.transform.GetOutputStreamInfo(0)? };
        let provides = stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

        let sample = if provides {
            None
        } else {
            // 呼び出し側でバッファ確保 (通常 HW MFT は provides-samples)。
            // SAFETY: MF API。
            unsafe {
                use windows::Win32::Media::MediaFoundation::MFCreateMemoryBuffer;
                let buffer = MFCreateMemoryBuffer(stream_info.cbSize.max(1024 * 1024))?;
                let s = MFCreateSample()?;
                s.AddBuffer(&buffer)?;
                Some(s)
            }
        };

        let out_buf = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut outs = [out_buf];
        let mut status = 0u32;

        // SAFETY: outs は MFT が読み書きする。
        let r = unsafe { self.transform.ProcessOutput(0, &mut outs, &mut status) };
        match r {
            Ok(()) => {
                // SAFETY: ProcessOutput 成功時 pSample は live。take で所有権を取り出す。
                let sample = unsafe { std::mem::ManuallyDrop::take(&mut outs[0].pSample) }
                    .ok_or_else(|| anyhow!("ProcessOutput returned no sample"))?;
                // SAFETY: pEvents は使わないので手動 drop して解放する。
                unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pEvents) };

                // sample → contiguous buffer → bytes
                // SAFETY: sample は live。ConvertToContiguousBuffer は OUT を返し ? で確認。
                let buf: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer()? };
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut cur_len = 0u32;
                // SAFETY: Lock はポインタ+長さを返す。コピー後 Unlock する。
                unsafe {
                    buf.Lock(&mut ptr, None, Some(&mut cur_len))
                        .context("IMFMediaBuffer::Lock (out)")?;
                }
                let mut data = vec![0u8; cur_len as usize];
                // SAFETY: ptr は cur_len バイト有効。
                unsafe {
                    std::ptr::copy_nonoverlapping(ptr, data.as_mut_ptr(), cur_len as usize);
                    let _ = buf.Unlock();
                }

                let keyframe = is_keyframe_annexb(&data);
                // 投入時に積んだ frame_id と 1:1 で突き合わせる。キューが空 = 対応が崩れて
                // いる。frame_id を捏造 (0 等) して送るとクライアントの再組立てが静かに
                // 狂うため、このサンプルは破棄して警告を残す (次のキーフレームで回復する)。
                let Some(frame_id) = self.in_flight.pop_front() else {
                    tracing::warn!(
                        len = data.len(),
                        "encoder output without matching submitted frame; dropping sample"
                    );
                    return Ok(());
                };
                self.out_queue.push_back(EncodedNal {
                    data,
                    keyframe,
                    frame_id,
                });
                Ok(())
            }
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // SAFETY: ストリーム変更時も出力サンプルは無い。pSample を手動 drop して解放する。
                unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pSample) };
                // SAFETY: pEvents も同様に手動 drop して解放する。
                unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pEvents) };
                // 出力ストリーム変更要求: H.264 出力タイプを再設定して次回出力を待つ。
                let out_type = make_h264_output_type(self.width, self.height, self.bitrate)?;
                // SAFETY: transform / type live。
                unsafe {
                    self.transform
                        .SetOutputType(0, &out_type, 0)
                        .context("re-SetOutputType after stream change")?;
                }
                Ok(())
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                // SAFETY: 出力が無いので pSample を手動 drop して解放する。
                unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pSample) };
                // SAFETY: pEvents も同様に手動 drop して解放する。
                unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pEvents) };
                Ok(())
            }
            Err(e) => {
                // SAFETY: エラー時も出力サンプルは無い。pSample を手動 drop して解放する。
                unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pSample) };
                // SAFETY: pEvents も同様に手動 drop して解放する。
                unsafe { std::mem::ManuallyDrop::drop(&mut outs[0].pEvents) };
                Err(anyhow!("ProcessOutput: {e}"))
            }
        }
    }
}

impl VideoEncoder for MfHwH264Encoder {
    fn push_frame(&mut self, frame: &Frame) -> Result<()> {
        let fid = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);

        self.ensure_inner(frame.width, frame.height)?;
        let force_keyframe = self.force_keyframe;
        self.force_keyframe = false;
        let inner = self.inner.as_mut().unwrap();
        if force_keyframe {
            // HW MFT (特に AMD AMF) はサンプル属性 `MFSampleExtension_CleanPoint` を honor せず、
            // IDR を GOP 周期でしか出さない個体がある (実機: 再接続後ずっと黒く、GOP 周期の
            // FPS*10≈10〜20 秒後にようやく表示)。ICodecAPI の `AVEncVideoForceKeyFrame` が次フレームを
            // IDR に強制する正規ルートなので、これを立ててから ProcessInput する (auto-reset)。
            if let Some(api) = &inner.codec_api {
                // SAFETY: api live、VARIANT は VT_UI4。SetValue は値をコピーする。
                unsafe {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
                }
            }
        }
        inner.submit(&frame.texture.0, fid, force_keyframe)
    }

    fn poll_nal(&mut self) -> Result<Option<EncodedNal>> {
        Ok(self.inner.as_mut().and_then(|i| i.out_queue.pop_front()))
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn flush_pending(&mut self) -> Result<()> {
        match self.inner.as_mut() {
            Some(inner) => inner.pump_pending_outputs(),
            None => Ok(()),
        }
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.bitrate = bitrate_bps;
        let bounded = self.max_bitrate != u32::MAX;
        if let Some(inner) = &mut self.inner {
            inner.bitrate = bitrate_bps;
            if let Some(api) = &inner.codec_api {
                // SAFETY: api live、VARIANT は VT_UI4。
                unsafe {
                    api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate_bps))
                        .context("ICodecAPI::SetValue(MeanBitRate)")?;
                }
                // VBV バッファも新レートへ追従させ低遅延を保つ (無制限モードでは触らない)。
                if bounded {
                    set_vbv_buffer(api, bitrate_bps);
                }
            }
        }
        Ok(())
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: cleanup; errors ignored.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

// SAFETY: MfHwH264Encoder は encode スレッドでのみ使用される。COM は MTA、共有デバイスは
// multithread-protected。別スレッドへの move は安全 (MfH264Encoder と同じ流儀)。
unsafe impl Send for MfHwH264Encoder {}

/// HW H.264 エンコーダ MFT が 1 つ以上存在するか (インスタンス化せず列挙のみ)。
fn hw_h264_encoder_present() -> Result<bool> {
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx は activate 配列 (CoTaskMem) と件数を OUT で返す。
    unsafe {
        mft_enum_ex_call(flags, &out_info, &mut activates, &mut count)?;
        if !activates.is_null() {
            // 列挙された IMFActivate を解放してから配列メモリを返す。
            let slice = std::slice::from_raw_parts_mut(activates, count as usize);
            for slot in slice.iter_mut() {
                let _ = slot.take(); // drop = Release
            }
            CoTaskMemFree(Some(activates as *const _ as *const std::ffi::c_void));
        }
    }
    Ok(count > 0)
}

/// HW H.264 エンコーダ MFT を選ぶ。`target_luid` が非ゼロなら **まず同一 GPU の MFT に限定**して
/// 列挙する (`MFTEnum2` + `MFT_ENUM_ADAPTER_LUID`)。
///
/// 従来の「列挙順で総当たりして最初に activate できた MFT を無条件に採る」方式は、NVENC 等
/// 別 GPU の MFT が activate 可能な環境で「別 GPU の MFT + このキャプチャデバイス」という
/// 不整合構成を選び映像が全滅しうる (実機で発生しうる危険な組み合わせ)。そのため既定では
/// キャプチャデバイスと同一 LUID の MFT に絞ってから総当たりする。LUID フィルタが 0 件/失敗
/// (LUID 取得不可・ドライバ不備等) の場合のみ、ハイブリッド GPU 環境向けの全 MFT 総当たり
/// (`MFTEnumEx`、`MFTEnumEx` は LUID フィルタ非対応) にフォールバックする。
///
/// いずれの経路でも `ActivateObject` に失敗する候補が混在するのは想定内 (Optimus ではプロセス
/// GPU プリファレンス次第で一部ベンダの MFT が掴めない) なので、列挙順に総当たりして最初に
/// 起動できた MFT を採る。
fn enum_hw_encoder(target_luid: LUID) -> Result<(IMFTransform, String)> {
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);

    let has_luid = target_luid.LowPart != 0 || target_luid.HighPart != 0;
    let mut candidates = if has_luid {
        match enum_hw_encoder_by_luid(&out_info, flags, target_luid) {
            Ok(v) if !v.is_empty() => v,
            Ok(_) => {
                tracing::debug!(
                    "MFTEnum2(ADAPTER_LUID) matched no HW MFT; falling back to full enumeration"
                );
                Vec::new()
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "MFTEnum2(ADAPTER_LUID) failed; falling back to full enumeration"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    if candidates.is_empty() {
        candidates = enum_hw_encoder_all(&out_info, flags)?;
    }

    activate_first_candidate(candidates)
}

/// `target_luid` の GPU に限定して HW H.264 MFT を列挙する。
fn enum_hw_encoder_by_luid(
    out_info: &MFT_REGISTER_TYPE_INFO,
    flags: MFT_ENUM_FLAG,
    target_luid: LUID,
) -> Result<Vec<IMFActivate>> {
    // MFTEnumEx は LUID フィルタ非対応だが、MFTEnum2 は MFT_ENUM_ADAPTER_LUID 属性 (BLOB) で
    // 対象 GPU の HW MFT だけを列挙できる。キャプチャと別 GPU の MFT を誤選択しないための本命経路。
    let mut attrs: Option<IMFAttributes> = None;
    // SAFETY: MFCreateAttributes は OUT パラメータを初期化する。
    unsafe { MFCreateAttributes(&mut attrs, 1) }.context("MFCreateAttributes")?;
    let attrs = attrs.ok_or_else(|| anyhow!("IMFAttributes null"))?;
    let luid_blob: [u8; 8] = {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&target_luid.LowPart.to_le_bytes());
        b[4..].copy_from_slice(&target_luid.HighPart.to_le_bytes());
        b
    };
    // SAFETY: attrs は live。SetBlob は値をコピーする。
    unsafe { attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid_blob) }
        .context("SetBlob(ADAPTER_LUID)")?;

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnum2 は activate 配列 (CoTaskMem) と件数を OUT で返す。
    unsafe {
        mft_enum2_call(flags, out_info, &attrs, &mut activates, &mut count)?;
    }
    // SAFETY: MFTEnum2 が成功時に (0 件を含め) 有効な配列を返す。
    Ok(unsafe { collect_activates(activates, count) })
}

/// GPU 限定なしで HW H.264 MFT を総当たり列挙する (`MFTEnumEx`)。
fn enum_hw_encoder_all(
    out_info: &MFT_REGISTER_TYPE_INFO,
    flags: MFT_ENUM_FLAG,
) -> Result<Vec<IMFActivate>> {
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx は activate 配列 (CoTaskMem) と件数を OUT で返す。
    unsafe {
        mft_enum_ex_call(flags, out_info, &mut activates, &mut count)?;
    }
    if count == 0 || activates.is_null() {
        return Err(anyhow!("no hardware H.264 encoder MFT found"));
    }
    // SAFETY: MFTEnumEx が成功時に count 要素の配列を返す。
    Ok(unsafe { collect_activates(activates, count) })
}

/// 列挙結果 (CoTaskMem 配列) を Vec へ移し、配列メモリは即解放する。以後は Vec 上で扱い、
/// CoTaskMem の生ポインタを引きずらないようにする。
///
/// # Safety
/// `activates` は null か、`count` 要素の `Option<IMFActivate>` の CoTaskMem 配列であること。
unsafe fn collect_activates(activates: *mut Option<IMFActivate>, count: u32) -> Vec<IMFActivate> {
    if activates.is_null() {
        return Vec::new();
    }
    // SAFETY: 呼び出し元の契約により activates は count 要素の Option<IMFActivate> 配列。
    // 各要素を take して所有権を移す。
    unsafe {
        let slice = std::slice::from_raw_parts_mut(activates, count as usize);
        let v: Vec<IMFActivate> = slice.iter_mut().filter_map(|s| s.take()).collect();
        CoTaskMemFree(Some(activates as *const _ as *const std::ffi::c_void));
        v
    }
}

/// 列挙済み候補を先頭から `ActivateObject` を試し、**実際に起動できた最初の 1 つ**を
/// (transform, friendly name) で返す。
///
/// ハイブリッド GPU (Optimus) のノートでは複数ベンダの HW MFT が列挙される。プロセスの GPU
/// プリファレンスを iGPU(Intel) に固定していると、列挙先頭に来がちな dGPU(NVENC) の MFT は
/// `ActivateObject` が失敗する (プロセスから dGPU を掴めない)。先頭だけを試すと QSV にたどり着けず
/// 毎フレーム失敗するため、列挙順に総当たりして最初に起動できた MFT を採る。
fn activate_first_candidate(candidates: Vec<IMFActivate>) -> Result<(IMFTransform, String)> {
    if candidates.is_empty() {
        return Err(anyhow!("no hardware H.264 encoder MFT found"));
    }
    let mut skipped: Vec<String> = Vec::new();
    for activate in &candidates {
        let friendly = read_friendly_name(activate).unwrap_or_else(|| "unknown HW MFT".to_string());
        // SAFETY: activate は live。ActivateObject は MFT を 1 つ生成するか失敗を返す。
        match unsafe { activate.ActivateObject::<IMFTransform>() } {
            Ok(transform) => {
                if !skipped.is_empty() {
                    tracing::warn!(
                        selected = %friendly,
                        skipped = ?skipped,
                        "HW H.264 MFT: earlier candidate(s) failed to activate; using next"
                    );
                }
                return Ok((transform, friendly));
            }
            Err(e) => {
                // debug から warn に昇格: 既定ログレベルに HRESULT が残らず実機診断が遅れたため
                // (2026-07-03)。
                tracing::warn!(
                    encoder = %friendly,
                    error = %e,
                    "HW H.264 MFT ActivateObject failed; trying next candidate"
                );
                skipped.push(friendly);
            }
        }
    }
    Err(anyhow!(
        "no activatable hardware H.264 encoder MFT (tried: {skipped:?})"
    ))
}

/// `MFTEnumEx` の薄いラッパー (unsafe 呼び出しを 1 箇所に閉じる)。
///
/// # Safety
/// OUT パラメータは MF が初期化する。呼び出し側が配列を解放する。
unsafe fn mft_enum_ex_call(
    flags: MFT_ENUM_FLAG,
    out_info: &MFT_REGISTER_TYPE_INFO,
    activates: *mut *mut Option<IMFActivate>,
    count: *mut u32,
) -> Result<()> {
    use windows::Win32::Media::MediaFoundation::MFTEnumEx;
    // SAFETY: activates/count は呼び出し側が渡す有効な OUT ポインタ。MFTEnumEx が結果を確保する。
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(out_info),
            activates,
            count,
        )
        .context("MFTEnumEx")
    }
}

/// `MFTEnum2` の薄いラッパー (unsafe 呼び出しを 1 箇所に閉じる)。
///
/// # Safety
/// OUT パラメータは MF が初期化する。呼び出し側が配列を解放する。
unsafe fn mft_enum2_call(
    flags: MFT_ENUM_FLAG,
    out_info: &MFT_REGISTER_TYPE_INFO,
    attrs: &IMFAttributes,
    activates: *mut *mut Option<IMFActivate>,
    count: *mut u32,
) -> Result<()> {
    // SAFETY: activates/count は呼び出し側が渡す有効な OUT ポインタ。MFTEnum2 が結果を確保する。
    unsafe {
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(out_info),
            attrs,
            activates,
            count,
        )
        .context("MFTEnum2")
    }
}

/// IMFActivate の friendly name を読む (ログ用、失敗は None)。
fn read_friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: activate live。成功時 p は CoTaskMem の wide 文字列。
    unsafe {
        activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len)
            .ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.as_ptr() as *const std::ffi::c_void));
        s
    }
}
