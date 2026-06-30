//! Media Foundation 同期 SW H.264 エンコーダ (HW 初期化失敗時のフォールバック)。
//!
//! 方針:
//! - 同期 (SW) MFT を使い、HW MFT のメッセージシーケンスを避ける
//! - 入力 NV12 / 出力 H.264 (Annex-B start code 付き、MF のデフォルト)
//! - 解像度は最初のフレームに追従し、変化したら MFT を作り直す
//! - 60fps / Baseline。ビットレートは初期 8Mbps、`ICodecAPI` で実行時可変
//! - 入力は GPU 上の NV12 テクスチャ。staging へ `CopyResource` → `Map` で 1 度だけ
//!   リードバックして SW MFT に渡す (AMD 以外 / VM / ドライバ不備でも必ず動く保険経路)

use std::collections::VecDeque;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMFMediaBuffer, IMFMediaType, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_INFO, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::GUID;
use windows::core::Interface;

/// MS H.264 Encoder MFT CLSID (CLSID_CMSH264EncoderMFT)。
/// windows-rs 0.58 では未エクスポートのため手定義。
const CLSID_CMSH264_ENCODER_MFT: GUID = GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);

use crate::capture::Frame;
use crate::capture::SharedDevice;
use crate::capture::device::DeviceBundle;
use crate::encode::{DEFAULT_BITRATE_BPS, EncodedNal, VideoEncoder};

pub(crate) const FPS: u32 = 60;
pub(crate) const FRAME_DURATION_100NS: i64 = (10_000_000i64) / FPS as i64;

/// VARIANT 型タグ VT_UI4 (unsigned long)。`windows::Win32::System::Variant` の
/// 追加 feature を避けるため数値を直接定義する。
const VT_UI4: u16 = 19;
/// VARIANT 型タグ VT_BOOL。
const VT_BOOL: u16 = 11;

/// 低遅延設定として明示する GOP サイズ (フレーム数)。`FPS * 10` ≒ 10 秒。
/// キーフレームは原則クライアントの `RequestKeyframe` 時のみ送る設計 (接続時・ロス回復時)。
/// エンコーダ任せ (GOP 0) だと数秒ごとに周期 IDR が入り、その都度帯域スパイク→遅延の山に
/// なるため、十分大きく取って周期 IDR を実質無効化する。10 秒の安全網は、要求が万一とりこぼ
/// された場合の最終的な自己回復を残すための妥協値。
const GOP_SIZE: u32 = FPS * 10;

pub struct MfH264Encoder {
    /// 共有デバイス (NV12 テクスチャの staging リードバックに使う)。
    device: SharedDevice,
    /// 実 MFT は最初のフレーム解像度が分かってから構築する。
    inner: Option<Inner>,
    /// 目標平均ビットレート (bps)。inner 構築時・set_bitrate 時に反映。
    bitrate: u32,
    /// ピーク (最大) ビットレート上限 (bps)。PeakConstrainedVBR の上限。
    max_bitrate: u32,
    frame_id: u16,
    /// 次フレームを強制 keyframe にするフラグ (初回 / 再構築 / 明示要求)。
    force_keyframe: bool,
}

/// 特定解像度に紐づく MFT 一式。
struct Inner {
    transform: IMFTransform,
    /// 実行時パラメータ変更用インターフェース。ビットレート適応で使う。
    codec_api: ICodecAPI,
    width: u32,
    height: u32,
    sample_time_100ns: i64,
    out_queue: VecDeque<EncodedNal>,
    nv12_buf: Vec<u8>,
    /// NV12 リードバック用 staging テクスチャと共有コンテキスト。
    context: ID3D11DeviceContext,
    readback: ID3D11Texture2D,
}

impl MfH264Encoder {
    pub fn new(device: SharedDevice, max_bitrate: u32) -> Result<Self> {
        // COM/MF はこのスレッドで初期化済み前提だが冪等に保証する。
        crate::capture::device::init_thread_com()?;
        Ok(Self {
            device,
            inner: None,
            bitrate: DEFAULT_BITRATE_BPS.min(max_bitrate),
            max_bitrate,
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
            tracing::info!(
                width,
                height,
                bitrate = self.bitrate,
                "building SW H.264 MFT"
            );
            self.inner = Some(Inner::new(
                &self.device,
                width,
                height,
                self.bitrate,
                self.max_bitrate,
            )?);
            // 新しい MFT の最初の出力は keyframe にする。
            self.force_keyframe = true;
        }
        Ok(())
    }
}

impl Inner {
    fn new(
        device: &DeviceBundle,
        width: u32,
        height: u32,
        bitrate: u32,
        max_bitrate: u32,
    ) -> Result<Self> {
        // SAFETY: CoCreateInstance は OUT param を初期化。CLSID は MS H.264 encoder。
        let transform: IMFTransform = unsafe {
            CoCreateInstance(&CLSID_CMSH264_ENCODER_MFT, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance(H264 encoder)")?
        };

        // 出力メディアタイプを先に設定 (MFT の規約)
        let out_type = make_h264_output_type(width, height, bitrate)?;
        // SAFETY: transform live, type live
        unsafe {
            transform
                .SetOutputType(0, &out_type, 0)
                .context("SetOutputType")?
        };

        let in_type = make_nv12_input_type(width, height)?;
        // SAFETY: as above
        unsafe {
            transform
                .SetInputType(0, &in_type, 0)
                .context("SetInputType")?
        };

        // 実行時ビットレート変更用に ICodecAPI を取得 (MS H.264 encoder は対応)。
        let codec_api: ICodecAPI = transform.cast().context("QueryInterface ICodecAPI")?;
        // 低遅延モード + 大きな GOP (周期 IDR 抑制)。HW 経路と揃える。
        configure_low_latency(&codec_api);
        // レート制御を明示する (既定の緩い VBR は目標を大きく超過するため)。
        configure_rate_control(&codec_api, bitrate, max_bitrate);

        // SAFETY: メッセージ送信は副作用のみ。
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("START_OF_STREAM")?;
        }

        // NV12 リードバック用 staging テクスチャ (CPU READ)。
        let readback = {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
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
            // SAFETY: device live、desc 全フィールド初期化済み。
            unsafe {
                let mut tex = None;
                device
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .context("CreateTexture2D(NV12 readback staging)")?;
                tex.ok_or_else(|| anyhow!("readback staging null"))?
            }
        };

        Ok(Self {
            transform,
            codec_api,
            width,
            height,
            sample_time_100ns: 0,
            out_queue: VecDeque::new(),
            nv12_buf: vec![0u8; (width * height * 3 / 2) as usize],
            context: device.context.clone(),
            readback,
        })
    }

    /// GPU NV12 テクスチャを staging へコピーし `Map` で `nv12_buf` (連続 NV12) へ読み戻す。
    fn readback_nv12(&mut self, src: &ID3D11Texture2D) -> Result<()> {
        let w = self.width as usize;
        let h = self.height as usize;
        // SAFETY: 同 device・同フォーマット・同サイズ間の GPU 内コピー。
        unsafe { self.context.CopyResource(&self.readback, src) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: readback は STAGING + CPU_ACCESS_READ。Map は安全に呼べる。
        unsafe {
            self.context
                .Map(&self.readback, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map(NV12 readback)")?;
        }
        let pitch = mapped.RowPitch as usize;
        let (y_dst, uv_dst) = self.nv12_buf.split_at_mut(w * h);
        // SAFETY: pData は各行 pitch バイト読める。NV12 は luma(h 行) の後に chroma(h/2 行)。
        unsafe {
            let base = mapped.pData as *const u8;
            for j in 0..h {
                std::ptr::copy_nonoverlapping(
                    base.add(j * pitch),
                    y_dst.as_mut_ptr().add(j * w),
                    w,
                );
            }
            let chroma = base.add(pitch * h);
            for j in 0..h / 2 {
                std::ptr::copy_nonoverlapping(
                    chroma.add(j * pitch),
                    uv_dst.as_mut_ptr().add(j * w),
                    w,
                );
            }
            self.context.Unmap(&self.readback, 0);
        }
        Ok(())
    }

    fn drain_outputs(&mut self, current_frame_id: u16) -> Result<()> {
        // SAFETY: MFT live, out param 初期化される。
        let stream_info: MFT_OUTPUT_STREAM_INFO = unsafe { self.transform.GetOutputStreamInfo(0)? };

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
        let fid = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);

        // ensure_inner は再構築時に force_keyframe を立てるので、その後に読む。
        self.ensure_inner(frame.width, frame.height)?;
        let force_keyframe = self.force_keyframe;
        self.force_keyframe = false;
        let inner = self.inner.as_mut().unwrap();
        // GPU NV12 テクスチャを CPU へ読み戻す (フォールバック経路のリードバック)。
        inner.readback_nv12(&frame.texture.0)?;

        // IMFSample 生成
        let nv12_size = inner.nv12_buf.len() as u32;
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
            std::ptr::copy_nonoverlapping(inner.nv12_buf.as_ptr(), ptr, inner.nv12_buf.len());
            buffer.Unlock().context("Unlock (in buffer)")?;
            buffer
                .SetCurrentLength(nv12_size)
                .context("SetCurrentLength")?;
            sample.AddBuffer(&buffer).context("AddBuffer (in)")?;
            sample
                .SetSampleTime(inner.sample_time_100ns)
                .context("SetSampleTime")?;
            sample
                .SetSampleDuration(FRAME_DURATION_100NS)
                .context("SetSampleDuration")?;

            if force_keyframe {
                use windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint;
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }
        }
        inner.sample_time_100ns += FRAME_DURATION_100NS;

        // SAFETY: MFT は sample を ref する。
        unsafe {
            inner
                .transform
                .ProcessInput(0, &sample, 0)
                .context("ProcessInput")?;
        }

        inner.drain_outputs(fid)?;
        Ok(())
    }

    fn poll_nal(&mut self) -> Result<Option<EncodedNal>> {
        Ok(self.inner.as_mut().and_then(|i| i.out_queue.pop_front()))
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.bitrate = bitrate_bps;
        let bounded = self.max_bitrate != u32::MAX;
        if let Some(inner) = &self.inner {
            let v = variant_u32(bitrate_bps);
            // SAFETY: codec_api は生存しており、VARIANT は VT_UI4 で有効。SetValue は値をコピーする。
            unsafe {
                inner
                    .codec_api
                    .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &v)
                    .context("ICodecAPI::SetValue(MeanBitRate)")?;
            }
            // VBV バッファも新レートへ追従させ低遅延を保つ (無制限モードでは触らない)。
            if bounded {
                set_vbv_buffer(&inner.codec_api, bitrate_bps);
            }
        }
        Ok(())
    }
}

/// エンコーダのレート制御を **CBR (平均一定)** で明示設定する。
/// 既定の (無指定) レート制御は緩い VBR で、動きの多い場面で目標を大きく超過する
/// (実測で 15Mbps 目標に対し ~23Mbps)。さらに **AMD HW MFT は PeakConstrainedVBR の
/// 平均/ピーク上限も MF_MT_AVG_BITRATE も無視**し、軽いウィンドウ (エクスプローラ/メモ帳)
/// の高速ドラッグでも 17-21Mbps を割り当ててフレームが重くなり encode が伸びる
/// (2026-06-29 実測)。CBR は AMF が比較的 honor するため、平均を一定目標に固定して
/// フレームサイズを抑え encode を速くする狙い。HW MFT はベンダによって一部の knob を
/// 無視するため各設定は best-effort (失敗は debug ログのみ)。SW / HW 両エンコーダで共有。
pub(crate) fn configure_rate_control(codec_api: &ICodecAPI, mean_bps: u32, max_bps: u32) {
    // 無制限 (u32::MAX): レート制御モードを設定せず既定 VBR のまま、平均目標のみ与える
    // (コミット前の挙動と等価)。上限を課さないので大きなオーバーシュートを許容する。
    if max_bps == u32::MAX {
        // SAFETY: codec_api 生存、VARIANT は VT_UI4。
        unsafe {
            let _ = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(mean_bps));
        }
        return;
    }
    // CBR では平均のみを一定目標にする (ピーク/Max は使わない)。
    let _ = max_bps;
    // SAFETY: codec_api 生存、VARIANT は VT_UI4。SetValue は値をコピーする。
    unsafe {
        if let Err(e) = codec_api.SetValue(
            &CODECAPI_AVEncCommonRateControlMode,
            &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
        ) {
            tracing::debug!(error = %e, "set RateControlMode (CBR) unsupported");
        }
        let _ = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(mean_bps));
        // VBV/HRD バッファは**平均 (現在) レート基準で ~0.5 秒**に絞る。旧実装は「ピークの
        // 1 秒分」と大きく、画面スクロール等のバーストが最大 1 秒ぶん回線に積み上がって体感
        // 遅延のスパイク (バッファブロート) を生んでいた。set_bitrate でも追従更新する。
        set_vbv_buffer(codec_api, mean_bps);
    }
    tracing::info!(
        mode = "CBR",
        target_bps = mean_bps,
        "rate control configured"
    );
}

/// VBV/HRD バッファの目標時間 (ms)。小さいほど瞬間レートのバーストが抑えられ、回線に
/// 積み上がる遅延 (バッファブロート) が減って操作の体感遅延が安定する。大きすぎると逆に
/// バーストを許して遅延スパイクになり、小さすぎると動きの多い場面で品質が落ちる。0.5 秒は
/// その折衷 (実機で要調整余地あり)。
const VBV_BUFFER_MS: u32 = 500;

/// 低遅延向けの共通設定 (SW / HW 両エンコーダから呼ぶ)。
/// - `AVLowLatencyMode`: B フレーム/先読みを無効化し、出力遅延を最小化する。
/// - 大きな `GOPSize`: 周期 IDR を実質無効化し、キーフレームは原則クライアント要求時のみに
///   する (周期的な帯域スパイク = 遅延の山を防ぐ)。
///
/// HW MFT はベンダによって一部 knob を無視するため best-effort (失敗は無視)。
pub(crate) fn configure_low_latency(codec_api: &ICodecAPI) {
    // SAFETY: codec_api 生存。VARIANT は対応する型タグで構築し SetValue が値をコピーする。
    unsafe {
        let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
        let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &variant_u32(GOP_SIZE));
    }
}

/// VBV/HRD バッファサイズ (bits) を現在のビットレートの `VBV_BUFFER_MS` 相当に設定する。
/// ビットレート適応のたびに呼び、レートに追従して常に一定時間ぶんに保つ (低遅延を維持)。
/// HW MFT は無視することがあるため best-effort (失敗は無視)。無制限モード (max=u32::MAX) では
/// 呼ばない (既定 VBR のまま大きなオーバーシュートを許す by-design 経路を変えない)。
pub(crate) fn set_vbv_buffer(codec_api: &ICodecAPI, bitrate_bps: u32) {
    // bits = bps * ms / 1000。u64 で計算してオーバーフローを避け、u32 へ飽和。
    let buf_bits = ((bitrate_bps as u64) * VBV_BUFFER_MS as u64 / 1000).min(u32::MAX as u64) as u32;
    // SAFETY: codec_api 生存、VARIANT は VT_UI4。SetValue は値をコピーする。
    unsafe {
        let _ = codec_api.SetValue(&CODECAPI_AVEncCommonBufferSize, &variant_u32(buf_bits));
    }
}

/// VT_UI4 の VARIANT を組み立てる。windows-core 0.58 は VARIANT 内部を非公開にしており
/// 整数からの構築 API が無いため、メモリレイアウトを手動で再現して transmute する。
pub(crate) fn variant_u32(value: u32) -> windows::core::VARIANT {
    // 64bit Windows の VARIANT レイアウト: vt(u16) + 予約 3*u16 + 16B 共用体。
    // VT_UI4 は共用体先頭 (offset 8) の ulVal(u32) に値を置く。
    #[repr(C)]
    struct RawVariant {
        vt: u16,
        r1: u16,
        r2: u16,
        r3: u16,
        val: u64,
        pad: u64,
    }
    let raw = RawVariant {
        vt: VT_UI4,
        r1: 0,
        r2: 0,
        r3: 0,
        val: value as u64,
        pad: 0,
    };
    // SAFETY: RawVariant は x86_64 Windows の VARIANT と同一レイアウト (24B)。サイズ不一致なら
    // transmute がコンパイルエラーになる。VT_UI4 は非ポインタ型なので Drop (VariantClear) も安全。
    unsafe { std::mem::transmute::<RawVariant, windows::core::VARIANT>(raw) }
}

/// VT_BOOL の VARIANT を組み立てる (`variant_u32` と同じレイアウト手法)。
pub(crate) fn variant_bool(value: bool) -> windows::core::VARIANT {
    #[repr(C)]
    struct RawVariant {
        vt: u16,
        r1: u16,
        r2: u16,
        r3: u16,
        val: u64,
        pad: u64,
    }
    let raw = RawVariant {
        vt: VT_BOOL,
        r1: 0,
        r2: 0,
        r3: 0,
        // VARIANT_BOOL: true = -1 (0xFFFF), false = 0。
        val: if value { 0xFFFF } else { 0 },
        pad: 0,
    };
    // SAFETY: RawVariant は x86_64 Windows の VARIANT と同一レイアウト。VT_BOOL は非ポインタ。
    unsafe { std::mem::transmute::<RawVariant, windows::core::VARIANT>(raw) }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: cleanup; errors ignored.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

// SAFETY: MfH264Encoder は 1 つのスレッド (encode thread) でのみ使用される。
// IMFTransform は MTA で COM 初期化されており、別スレッドへの move は安全。
unsafe impl Send for MfH264Encoder {}

pub(crate) fn make_h264_output_type(width: u32, height: u32, bitrate: u32) -> Result<IMFMediaType> {
    // SAFETY: all API are OUT/SETTER.
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType (out)")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(FPS, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
        // 色 VUI (matrix / nominal range) はあえて設定しない。MS H.264 エンコーダの
        // 挙動とデコーダ (WebCodecs) の解釈が噛み合わず、付けると色/明るさが破綻する。
        // VUI 無指定のとき WebCodecs は BT.601 limited と解釈するので、GPU 色変換
        // (convert.rs) も同係数の BT.601 limited に揃えてある (ほぼ無損失なラウンドトリップ)。
        Ok(t)
    }
}

pub(crate) fn make_nv12_input_type(width: u32, height: u32) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType (in)")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(FPS, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(t)
    }
}

pub(crate) fn pack_u64(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

/// Annex-B NAL 列を走査して IDR (nal_unit_type=5) を含むか判定。
pub(crate) fn is_keyframe_annexb(buf: &[u8]) -> bool {
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
