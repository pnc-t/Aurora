//! GPU 色変換 (Windows): キャプチャテクスチャ (BGRA8 / scRGB FP16) → NV12 テクスチャ。
//!
//! フルスクリーン三角形 + ピクセルシェーダで NV12 の 2 プレーンへ別々にレンダする
//! (UAV 直書きより全ベンダで確実な王道手法)。
//!   - Y  パス: viewport WxH、RTV を NV12 plane0 上に R8_UNORM で作成
//!   - UV パス: viewport (W/2)x(H/2)、RTV を plane1 上に R8G8_UNORM で作成し 2x2 平均
//!
//! 色は BT.601 limited で、係数を `encode/mf.rs::bgra_to_nv12` (CPU 経路) と一致させる。
//! HDR は現行 `build_scrgb_lut` 経路 (scRGB/scale → [0,1] クランプ → sRGB OETF) を
//! シェーダで再現する。VUI は引き続き無指定。

use std::ffi::c_void;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D11_SRV_DIMENSION_TEXTURE2D, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD,
    D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_ZERO, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL,
    D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE, D3D11_CULL_NONE, D3D11_FILL_SOLID,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_MAP_WRITE_DISCARD,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_USAGE_IMMUTABLE,
    D3D11_VIEWPORT, ID3D11BlendState, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_TYPELESS, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM,
    DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16B16A16_TYPELESS,
    DXGI_SAMPLE_DESC,
};
use windows::core::s;

use super::device::DeviceBundle;

/// NV12 出力プールの枚数。async エンコーダが入力サーフェスを参照中に上書きしないため循環。
///
/// 前提: capture→encode は bounded(2) チャネル + try_send 間引きなので、同時に in-flight な
/// NV12 は「キュー 2 + エンコード中 1 + 生成中 1」≈4 枚に収まる。HW MFT が入力サーフェスを
/// これ以上 (4 枚超) 同時保持するエンコーダだと循環で参照中サーフェスを上書きし得るため、
/// チャネル容量を増やす場合はここも合わせて見直すこと。
const POOL_SIZE: usize = 4;

/// 色変換シェーダ (HLSL)。VSMain(full-screen triangle) + PSY(luma) + PSUV(chroma)。
/// HLSL コメントは ASCII のみ (raw 文字列 → as_bytes() で D3DCompile に渡すため)。
const SHADER_HLSL: &str = r#"
cbuffer Params : register(b0) {
    uint  g_is_hdr;
    float g_inv_scale;   // 1 / sdr_white_scale (HDR normalization)
    float2 g_pad;
};

Texture2D<float4> g_src : register(t0);

struct VSOut { float4 pos : SV_Position; };

VSOut VSMain(uint vid : SV_VertexID) {
    VSOut o;
    float2 uv = float2((vid << 1) & 2, vid & 2);
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

float srgb_encode(float l) {
    return (l <= 0.0031308) ? (12.92 * l) : (1.055 * pow(l, 1.0 / 2.4) - 0.055);
}

// sample -> [0,1] gamma-encoded RGB.
// SDR (BGRA8 UNORM): raw values as-is (no sRGB decode).
// HDR (scRGB FP16): scale-normalize -> [0,1] hard clamp -> sRGB OETF. Equals build_scrgb_lut.
float3 to_display(float3 c) {
    if (g_is_hdr != 0) {
        c = saturate(c * g_inv_scale);
        c = float3(srgb_encode(c.r), srgb_encode(c.g), srgb_encode(c.b));
    }
    return c;
}

// BT.601 limited (same 8bit fixed-point coefficients <<8 as bgra_to_nv12).
float bt601_y(float3 c) {
    float3 p = c * 255.0;
    return ((66.0 * p.r + 129.0 * p.g + 25.0 * p.b + 128.0) / 256.0 + 16.0) / 255.0;
}
float2 bt601_uv(float3 c) {
    float3 p = c * 255.0;
    float u = (-38.0 * p.r - 74.0 * p.g + 112.0 * p.b + 128.0) / 256.0 + 128.0;
    float v = (112.0 * p.r - 94.0 * p.g - 18.0 * p.b + 128.0) / 256.0 + 128.0;
    return float2(u, v) / 255.0;
}

float PSY(VSOut i) : SV_Target {
    int2 p = int2(i.pos.xy);
    float3 c = to_display(g_src.Load(int3(p, 0)).rgb);
    return bt601_y(c);
}

float2 PSUV(VSOut i) : SV_Target {
    int2 p = int2(i.pos.xy) * 2;
    float3 c0 = to_display(g_src.Load(int3(p + int2(0, 0), 0)).rgb);
    float3 c1 = to_display(g_src.Load(int3(p + int2(1, 0), 0)).rgb);
    float3 c2 = to_display(g_src.Load(int3(p + int2(0, 1), 0)).rgb);
    float3 c3 = to_display(g_src.Load(int3(p + int2(1, 1), 0)).rgb);
    float3 c = (c0 + c1 + c2 + c3) * 0.25;
    return bt601_uv(c);
}

// --- 縮小ブリット (解像度変更) ---
// src を線形サンプルして「小さい同フォーマット RT」へ描く前段パス。色/HDR には触れず
// (フォーマット維持の純リサンプル)、出力は縮小後サイズ。フル解像度時はこのパスを通さない。
SamplerState g_samp : register(s0);
struct BlitOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
BlitOut VSBlit(uint vid : SV_VertexID) {
    BlitOut o;
    float2 uv = float2((vid << 1) & 2, vid & 2);
    o.uv = uv;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}
float4 PSBlit(BlitOut i) : SV_Target {
    return g_src.Sample(g_samp, i.uv);
}

// --- カーソル合成 (src_copy へ alpha ブレンドで焼き込む小クアッド) ---
// DXGI Desktop Duplication はカーソルを映像に含めないため、別途取得した形状を
// ここで重ねる。CPU 側で全種別 (color / masked / monochrome) をストレートアルファの
// BGRA8 へ正規化済みなので、シェーダは単純な alpha 出力に徹し OM ブレンドで合成する。
cbuffer CursorParams : register(b1) {
    float4 g_rect;       // クアッドの NDC 座標 (x0,y0=左上, x1,y1=右下)
    uint   g_cur_hdr;    // 1 = HDR (scRGB linear へ変換して合成)
    float  g_cur_scale;  // SDR white scale (scRGB 正規化係数)
    float2 g_cur_pad;
};
Texture2D<float4> g_cursor : register(t1);
SamplerState g_cursamp : register(s1);

float3 srgb_decode(float3 c) {
    return float3(
        c.r <= 0.04045 ? c.r / 12.92 : pow((c.r + 0.055) / 1.055, 2.4),
        c.g <= 0.04045 ? c.g / 12.92 : pow((c.g + 0.055) / 1.055, 2.4),
        c.b <= 0.04045 ? c.b / 12.92 : pow((c.b + 0.055) / 1.055, 2.4));
}

struct CurOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

CurOut VSCursor(uint vid : SV_VertexID) {
    // 2 三角形でクアッドを張る。uv(0,0)=左上 → rect.xy、uv(1,1)=右下 → rect.zw。
    float2 corners[6] = {
        float2(0,0), float2(1,0), float2(0,1),
        float2(0,1), float2(1,0), float2(1,1)
    };
    float2 uv = corners[vid];
    CurOut o;
    o.uv = uv;
    o.pos = float4(lerp(g_rect.x, g_rect.z, uv.x), lerp(g_rect.y, g_rect.w, uv.y), 0.0, 1.0);
    return o;
}

float4 PSCursor(CurOut i) : SV_Target {
    float4 c = g_cursor.Sample(g_cursamp, i.uv);  // straight-alpha BGRA, sRGB encoded
    if (g_cur_hdr != 0) {
        // SDR の sRGB カーソル色を scRGB linear (SDR white 基準) へ。SDR は素通し
        // (src_copy も sRGB 8bit なので符号化空間のまま OM ブレンドで足し合わせる)。
        c.rgb = srgb_decode(c.rgb) * g_cur_scale;
    }
    return c;
}
"#;

#[repr(C)]
struct Params {
    is_hdr: u32,
    inv_scale: f32,
    _pad: [f32; 2],
}

/// カーソル合成クアッドの定数 (フレーム毎に更新するため DYNAMIC バッファへ書く)。
#[repr(C)]
struct CursorParams {
    /// クアッドの NDC 座標 (x0,y0 = 左上, x1,y1 = 右下)。
    rect: [f32; 4],
    is_hdr: u32,
    scale: f32,
    _pad: [f32; 2],
}

/// DXGI ポインタ形状の種別 (DXGI_OUTDUPL_POINTER_SHAPE_TYPE_*)。
const CURSOR_TYPE_MONOCHROME: u32 = 1;
const CURSOR_TYPE_COLOR: u32 = 2;
const CURSOR_TYPE_MASKED_COLOR: u32 = 4;

/// NV12 出力 1 枚と、その 2 プレーンへの RTV。
struct Nv12Target {
    tex: ID3D11Texture2D,
    rtv_y: ID3D11RenderTargetView,
    rtv_uv: ID3D11RenderTargetView,
}

/// 縮小ブリットの出力 (src と同フォーマット・縮小後サイズ)。RTV (描画先) + SRV (Y/UV 入力)。
struct ScaledTarget {
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
    #[allow(dead_code)] // tex は rtv/srv の寿命確保のため保持 (直接読まない)
    tex: ID3D11Texture2D,
}

pub struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    vs_blit: ID3D11VertexShader,
    ps_blit: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cbuffer: ID3D11Buffer,
    raster: ID3D11RasterizerState,

    // --- カーソル合成 ---
    vs_cursor: ID3D11VertexShader,
    ps_cursor: ID3D11PixelShader,
    cursor_sampler: ID3D11SamplerState,
    cursor_cbuffer: ID3D11Buffer,
    blend: ID3D11BlendState,
    /// HDR 合成のためシェーダへ渡す (is_hdr, sdr_white_scale)。
    is_hdr: bool,
    scale: f32,
    /// 現在のカーソル形状テクスチャ (ストレートアルファ BGRA8)。未取得なら None。
    cursor_tex: Option<ID3D11ShaderResourceView>,
    cursor_w: u32,
    cursor_h: u32,
    /// カーソルの現在位置 (キャプチャ出力の左上基準・ビットマップ左上 px) と可視性。
    cursor_x: i32,
    cursor_y: i32,
    cursor_visible: bool,
    /// 合成を行うか (クライアントのマウスキャプチャ中だけ ON)。OFF の間は焼き込まない。
    cursor_enabled: bool,

    // 入力をシェーダ可読化するコピー先 (Desktop Duplication のテクスチャは SRV 不可)。
    src_copy: Option<ID3D11Texture2D>,
    src_srv: Option<ID3D11ShaderResourceView>,
    src_rtv: Option<ID3D11RenderTargetView>,
    src_w: u32,
    src_h: u32,
    src_copy_format: DXGI_FORMAT,

    // 縮小ブリットの出力 (target_h で縮小するときだけ使う)。
    scaled: Option<ScaledTarget>,
    scaled_w: u32,
    scaled_h: u32,
    /// 目標の最大高さ (0 = フル解像度 = 縮小なし)。クライアントの SetResolution で更新。
    target_h: u32,

    // NV12 出力プール。
    pool: Vec<Nv12Target>,
    pool_idx: usize,
    nv12_w: u32,
    nv12_h: u32,
}

impl Converter {
    /// `is_hdr` / `scale` (= SDR white level / 80nit) をシェーダ定数に焼き込む。
    pub fn new(bundle: &DeviceBundle, is_hdr: bool, scale: f32) -> Result<Self> {
        let device = bundle.device.clone();
        let context = bundle.context.clone();

        // SAFETY: 各 D3DCompile / Create* は OUT パラメータを初期化する。HLSL は静的文字列。
        let (vs, ps_y, ps_uv, vs_blit, ps_blit, vs_cursor, ps_cursor) = unsafe {
            let vs_blob = compile_shader(s!("VSMain"), s!("vs_4_0"))?;
            let psy_blob = compile_shader(s!("PSY"), s!("ps_4_0"))?;
            let psuv_blob = compile_shader(s!("PSUV"), s!("ps_4_0"))?;
            let vsb_blob = compile_shader(s!("VSBlit"), s!("vs_4_0"))?;
            let psb_blob = compile_shader(s!("PSBlit"), s!("ps_4_0"))?;
            let vsc_blob = compile_shader(s!("VSCursor"), s!("vs_4_0"))?;
            let psc_blob = compile_shader(s!("PSCursor"), s!("ps_4_0"))?;

            let mut vs = None;
            device
                .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))
                .context("CreateVertexShader")?;
            let mut ps_y = None;
            device
                .CreatePixelShader(blob_bytes(&psy_blob), None, Some(&mut ps_y))
                .context("CreatePixelShader(Y)")?;
            let mut ps_uv = None;
            device
                .CreatePixelShader(blob_bytes(&psuv_blob), None, Some(&mut ps_uv))
                .context("CreatePixelShader(UV)")?;
            let mut vs_blit = None;
            device
                .CreateVertexShader(blob_bytes(&vsb_blob), None, Some(&mut vs_blit))
                .context("CreateVertexShader(blit)")?;
            let mut ps_blit = None;
            device
                .CreatePixelShader(blob_bytes(&psb_blob), None, Some(&mut ps_blit))
                .context("CreatePixelShader(blit)")?;
            let mut vs_cursor = None;
            device
                .CreateVertexShader(blob_bytes(&vsc_blob), None, Some(&mut vs_cursor))
                .context("CreateVertexShader(cursor)")?;
            let mut ps_cursor = None;
            device
                .CreatePixelShader(blob_bytes(&psc_blob), None, Some(&mut ps_cursor))
                .context("CreatePixelShader(cursor)")?;
            (
                vs.ok_or_else(|| anyhow!("VS null"))?,
                ps_y.ok_or_else(|| anyhow!("PS(Y) null"))?,
                ps_uv.ok_or_else(|| anyhow!("PS(UV) null"))?,
                vs_blit.ok_or_else(|| anyhow!("VS(blit) null"))?,
                ps_blit.ok_or_else(|| anyhow!("PS(blit) null"))?,
                vs_cursor.ok_or_else(|| anyhow!("VS(cursor) null"))?,
                ps_cursor.ok_or_else(|| anyhow!("PS(cursor) null"))?,
            )
        };

        // 縮小ブリット用の線形サンプラ (アスペクト維持・端はクランプ)。
        // SAFETY: device は live。CreateSamplerState は OUT に COM を書き、戻り値を ? で確認する。
        let sampler = unsafe {
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut s = None;
            device
                .CreateSamplerState(&desc, Some(&mut s))
                .context("CreateSamplerState")?;
            s.ok_or_else(|| anyhow!("sampler null"))?
        };

        // 定数バッファ (is_hdr / inv_scale)。ディスプレイ毎に固定なので IMMUTABLE。
        let params = Params {
            is_hdr: u32::from(is_hdr),
            inv_scale: 1.0 / scale.max(f32::MIN_POSITIVE),
            _pad: [0.0; 2],
        };
        // SAFETY: device は live。init.pSysMem=&params は size と整合。CreateBuffer の結果は ? で確認。
        let cbuffer = unsafe {
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Params>() as u32,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: &params as *const Params as *const c_void,
                SysMemPitch: 0,
                SysMemSlicePitch: 0,
            };
            let mut buf = None;
            device
                .CreateBuffer(&desc, Some(&init), Some(&mut buf))
                .context("CreateBuffer(constants)")?;
            buf.ok_or_else(|| anyhow!("constant buffer null"))?
        };

        // カーソル形状をくっきり重ねるための point サンプラ (端はクランプ)。
        // SAFETY: device は live。CreateSamplerState は OUT に COM を書き、戻り値を ? で確認する。
        let cursor_sampler = unsafe {
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut s = None;
            device
                .CreateSamplerState(&desc, Some(&mut s))
                .context("CreateSamplerState(cursor)")?;
            s.ok_or_else(|| anyhow!("cursor sampler null"))?
        };

        // カーソル定数バッファ (位置はフレーム毎に変わるため DYNAMIC・CPU 書き込み)。
        // SAFETY: device は live。CreateBuffer は OUT に COM を書き、戻り値を ? で確認 (初期データなし)。
        let cursor_cbuffer = unsafe {
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<CursorParams>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let mut buf = None;
            device
                .CreateBuffer(&desc, None, Some(&mut buf))
                .context("CreateBuffer(cursor)")?;
            buf.ok_or_else(|| anyhow!("cursor cbuffer null"))?
        };

        // ストレートアルファ合成用ブレンド (dst = src.rgb*src.a + dst*(1-src.a))。
        // SAFETY: device は live。CreateBlendState は OUT に COM を書き、戻り値を ? で確認する。
        let blend = unsafe {
            let rt = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_SRC_ALPHA,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_ZERO,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut desc = D3D11_BLEND_DESC::default();
            desc.RenderTarget[0] = rt;
            let mut b = None;
            device
                .CreateBlendState(&desc, Some(&mut b))
                .context("CreateBlendState(cursor)")?;
            b.ok_or_else(|| anyhow!("blend state null"))?
        };

        // フルスクリーン三角形の巻き順に依存しないよう cull を無効化。
        // SAFETY: device は live。CreateRasterizerState は OUT に COM を書き、戻り値を ? で確認する。
        let raster = unsafe {
            let desc = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_NONE,
                ..Default::default()
            };
            let mut rs = None;
            device
                .CreateRasterizerState(&desc, Some(&mut rs))
                .context("CreateRasterizerState")?;
            rs.ok_or_else(|| anyhow!("rasterizer state null"))?
        };

        Ok(Self {
            device,
            context,
            vs,
            ps_y,
            ps_uv,
            vs_blit,
            ps_blit,
            sampler,
            cbuffer,
            raster,
            vs_cursor,
            ps_cursor,
            cursor_sampler,
            cursor_cbuffer,
            blend,
            is_hdr,
            scale,
            cursor_tex: None,
            cursor_w: 0,
            cursor_h: 0,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: false,
            cursor_enabled: false,
            src_copy: None,
            src_srv: None,
            src_rtv: None,
            src_w: 0,
            src_h: 0,
            src_copy_format: DXGI_FORMAT_B8G8R8A8_TYPELESS,
            scaled: None,
            scaled_w: 0,
            scaled_h: 0,
            target_h: 0,
            pool: Vec::new(),
            pool_idx: 0,
            nv12_w: 0,
            nv12_h: 0,
        })
    }

    /// 目標の最大高さを設定する (0 = フル解像度)。次フレームの convert から反映する。
    /// アスペクト比は維持し、ソース高さがこれ以下なら縮小しない。
    pub fn set_target_height(&mut self, max_height: u32) {
        self.target_h = max_height;
    }

    /// キャプチャテクスチャ 1 枚を NV12 テクスチャへ変換する。返り値はプールの 1 枚 (COM 参照)。
    /// `target_h` で縮小指定があれば、Y/UV 変換の前に線形サンプルの縮小ブリットを 1 パス挟む
    /// (色/HDR 経路は不変)。フル解像度時は従来どおり縮小パスを通さない。
    pub fn convert(&mut self, src_tex: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: src_tex は live。GetDesc は OUT を埋める。
        unsafe { src_tex.GetDesc(&mut desc) };
        let (src_w, src_h, src_format) = (desc.Width, desc.Height, desc.Format);

        self.ensure_src_copy(src_w, src_h, src_format)?;
        let src_copy = self.src_copy.as_ref().unwrap().clone();
        let src_srv = self.src_srv.as_ref().unwrap().clone();

        // SAFETY: 同 device・互換フォーマット・同サイズ間の GPU 内コピー。
        unsafe { self.context.CopyResource(&src_copy, src_tex) };

        // カーソルを src_copy へ焼き込む (縮小/NV12 変換の前 = フル解像度で重ね、以後の
        // パスにそのまま乗る)。DXGI はカーソルを映像に含めないため別取得した形状を合成する。
        self.composite_cursor(src_w, src_h);

        // 目標サイズ (アスペクト維持・偶数)。target_h==0 か src 以下なら元サイズ (= フル)。
        let (out_w, out_h) = scaled_dims(src_w, src_h, self.target_h);
        let downscale = out_w != src_w || out_h != src_h;

        // 縮小時: src を線形サンプルで scaled へ描き、それを Y/UV の入力にする。
        let input_srv = if downscale {
            self.ensure_scaled(out_w, out_h, src_format)?;
            let scaled = self.scaled.as_ref().unwrap();
            let scaled_rtv = scaled.rtv.clone();
            let scaled_srv = scaled.srv.clone();
            let ctx = &self.context;
            // SAFETY: 縮小ブリット。src_srv → scaled (同フォーマット) を線形サンプルで描く。
            unsafe {
                ctx.IASetInputLayout(None);
                ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.VSSetShader(&self.vs_blit, None);
                ctx.PSSetShader(&self.ps_blit, None);
                ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
                ctx.PSSetShaderResources(0, Some(&[Some(src_srv.clone())]));
                ctx.RSSetState(&self.raster);
                let vp = viewport(out_w as f32, out_h as f32);
                ctx.RSSetViewports(Some(&[vp]));
                ctx.OMSetRenderTargets(Some(&[Some(scaled_rtv)]), None);
                ctx.Draw(3, 0);
                // scaled を次に SRV として読むため入力/出力束縛を外す。
                ctx.OMSetRenderTargets(Some(&[None]), None);
                ctx.PSSetShaderResources(0, Some(&[None]));
            }
            scaled_srv
        } else {
            src_srv
        };

        self.ensure_pool(out_w, out_h)?;
        let idx = self.pool_idx;
        self.pool_idx = (self.pool_idx + 1) % self.pool.len();
        let target = &self.pool[idx];

        let ctx = &self.context;
        // SAFETY: すべて live なオブジェクト。描画はプールテクスチャの全画素を書く。
        unsafe {
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuffer.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(input_srv.clone())]));
            ctx.RSSetState(&self.raster);

            // Y パス: 出力解像度。
            ctx.PSSetShader(&self.ps_y, None);
            let vp_y = viewport(out_w as f32, out_h as f32);
            ctx.RSSetViewports(Some(&[vp_y]));
            ctx.OMSetRenderTargets(Some(&[Some(target.rtv_y.clone())]), None);
            ctx.Draw(3, 0);

            // UV パス: 1/2 解像度。
            ctx.PSSetShader(&self.ps_uv, None);
            let vp_uv = viewport((out_w / 2) as f32, (out_h / 2) as f32);
            ctx.RSSetViewports(Some(&[vp_uv]));
            ctx.OMSetRenderTargets(Some(&[Some(target.rtv_uv.clone())]), None);
            ctx.Draw(3, 0);

            // 後続フレームの CopyResource / エンコーダ参照に備えて束縛を外す。
            ctx.OMSetRenderTargets(Some(&[None]), None);
            ctx.PSSetShaderResources(0, Some(&[None]));
        }

        Ok(target.tex.clone())
    }

    /// 映像へのカーソル合成 ON/OFF (クライアントのマウスキャプチャ中だけ ON)。
    pub fn set_cursor_overlay(&mut self, enabled: bool) {
        self.cursor_enabled = enabled;
    }

    /// カーソルの位置と可視性を更新する。`x`/`y` は DXGI PointerPosition (= カーソル
    /// ビットマップの左上、キャプチャ出力基準・px)。マウス未移動フレームでは呼ばれず直前を保つ。
    pub fn set_cursor_pos(&mut self, x: i32, y: i32, visible: bool) {
        self.cursor_x = x;
        self.cursor_y = y;
        self.cursor_visible = visible;
    }

    /// 新しいカーソル形状を受け取り、ストレートアルファ BGRA8 テクスチャを作り直す。
    /// `data` は DXGI の GetFramePointerShape が返した生バッファ。形状変化時のみ呼ばれる。
    pub fn update_cursor_shape(
        &mut self,
        cur_type: u32,
        width: u32,
        height: u32,
        pitch: u32,
        data: &[u8],
    ) -> Result<()> {
        let Some((w, h, bgra)) = build_cursor_rgba(cur_type, width, height, pitch, data) else {
            // 未知種別は合成しない (形状は据え置き)。
            return Ok(());
        };
        if w == 0 || h == 0 {
            return Ok(());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bgra.as_ptr() as *const c_void,
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        // SAFETY: device live、desc 初期化済み、init は w*4 ピッチの w*h*4 バイトを指す。
        let tex = unsafe {
            let mut tex = None;
            self.device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
                .context("CreateTexture2D(cursor)")?;
            tex.ok_or_else(|| anyhow!("cursor texture null"))?
        };
        let mut srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        srv_desc.Anonymous.Texture2D.MostDetailedMip = 0;
        srv_desc.Anonymous.Texture2D.MipLevels = 1;
        // SAFETY: tex live、srv_desc は TEXTURE2D 用に初期化済み。
        let srv = unsafe {
            let mut srv = None;
            self.device
                .CreateShaderResourceView(&tex, Some(&srv_desc), Some(&mut srv))
                .context("CreateShaderResourceView(cursor)")?;
            srv.ok_or_else(|| anyhow!("cursor SRV null"))?
        };

        self.cursor_tex = Some(srv);
        self.cursor_w = w;
        self.cursor_h = h;
        Ok(())
    }

    /// src_copy (フル解像度) へカーソルを alpha ブレンドで重ねる。合成 ON かつ可視かつ
    /// 形状取得済みのときだけ。
    fn composite_cursor(&mut self, src_w: u32, src_h: u32) {
        if !self.cursor_enabled
            || !self.cursor_visible
            || self.cursor_w == 0
            || src_w == 0
            || src_h == 0
        {
            return;
        }
        let Some(cursor_srv) = self.cursor_tex.clone() else {
            return;
        };
        let Some(rtv) = self.src_rtv.clone() else {
            return;
        };

        // DXGI PointerPosition はカーソルビットマップの左上 (ホットスポット補正済み) なので、
        // そのまま左上として置く。クアッドを NDC へ写す。
        let px = self.cursor_x as f32;
        let py = self.cursor_y as f32;
        let to_ndc_x = |x: f32| (x / src_w as f32) * 2.0 - 1.0;
        let to_ndc_y = |y: f32| 1.0 - (y / src_h as f32) * 2.0;
        let params = CursorParams {
            rect: [
                to_ndc_x(px),
                to_ndc_y(py),
                to_ndc_x(px + self.cursor_w as f32),
                to_ndc_y(py + self.cursor_h as f32),
            ],
            is_hdr: u32::from(self.is_hdr),
            scale: self.scale,
            _pad: [0.0; 2],
        };

        let ctx = &self.context;
        // SAFETY: DYNAMIC バッファを WRITE_DISCARD で写経し、live なオブジェクトで小クアッドを描く。
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if ctx
                .Map(
                    &self.cursor_cbuffer,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
                .is_err()
            {
                return;
            }
            std::ptr::copy_nonoverlapping(
                &params as *const CursorParams as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of::<CursorParams>(),
            );
            ctx.Unmap(&self.cursor_cbuffer, 0);

            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs_cursor, None);
            ctx.VSSetConstantBuffers(1, Some(&[Some(self.cursor_cbuffer.clone())]));
            ctx.PSSetShader(&self.ps_cursor, None);
            ctx.PSSetConstantBuffers(1, Some(&[Some(self.cursor_cbuffer.clone())]));
            ctx.PSSetShaderResources(1, Some(&[Some(cursor_srv)]));
            ctx.PSSetSamplers(1, Some(&[Some(self.cursor_sampler.clone())]));
            ctx.RSSetState(&self.raster);
            let vp = viewport(src_w as f32, src_h as f32);
            ctx.RSSetViewports(Some(&[vp]));
            ctx.OMSetBlendState(Some(&self.blend), None, 0xffff_ffff);
            ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            ctx.Draw(6, 0);

            // 後続パス (縮小/Y/UV) が src_copy を SRV として読むため束縛を外し、ブレンドも戻す。
            ctx.OMSetRenderTargets(Some(&[None]), None);
            ctx.OMSetBlendState(None, None, 0xffff_ffff);
            ctx.PSSetShaderResources(1, Some(&[None]));
        }
    }

    /// 縮小ブリット出力テクスチャ (src と同フォーマット・指定サイズ・RT+SR) を (再)用意する。
    fn ensure_scaled(&mut self, w: u32, h: u32, src_format: DXGI_FORMAT) -> Result<()> {
        if self.scaled.is_some() && self.scaled_w == w && self.scaled_h == h {
            return Ok(());
        }
        let (copy_format, view_format) = map_formats(src_format)?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: copy_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        // SAFETY: device live、desc 初期化済み。RT かつ SR として作成する。
        let tex = unsafe {
            let mut tex = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .context("CreateTexture2D(scaled)")?;
            tex.ok_or_else(|| anyhow!("scaled texture null"))?
        };

        let mut srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: view_format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        srv_desc.Anonymous.Texture2D.MostDetailedMip = 0;
        srv_desc.Anonymous.Texture2D.MipLevels = 1;
        // SAFETY: tex live、srv_desc は TEXTURE2D 用に初期化済み。
        let srv = unsafe {
            let mut srv = None;
            self.device
                .CreateShaderResourceView(&tex, Some(&srv_desc), Some(&mut srv))
                .context("CreateShaderResourceView(scaled)")?;
            srv.ok_or_else(|| anyhow!("scaled SRV null"))?
        };

        let mut rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: view_format,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        rtv_desc.Anonymous.Texture2D.MipSlice = 0;
        // SAFETY: tex live、rtv_desc は TEXTURE2D 用に初期化済み。
        let rtv = unsafe {
            let mut rtv = None;
            self.device
                .CreateRenderTargetView(&tex, Some(&rtv_desc), Some(&mut rtv))
                .context("CreateRenderTargetView(scaled)")?;
            rtv.ok_or_else(|| anyhow!("scaled RTV null"))?
        };

        self.scaled = Some(ScaledTarget { tex, srv, rtv });
        self.scaled_w = w;
        self.scaled_h = h;
        tracing::info!(w, h, "downscale target ready");
        Ok(())
    }

    /// シェーダ可読なコピー先テクスチャと SRV を (再)用意する。
    fn ensure_src_copy(&mut self, w: u32, h: u32, src_format: DXGI_FORMAT) -> Result<()> {
        let (copy_format, srv_format) = map_formats(src_format)?;
        let need_new = self.src_copy.is_none()
            || self.src_w != w
            || self.src_h != h
            || self.src_copy_format != copy_format;
        if !need_new {
            return Ok(());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: copy_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // SR (Y/UV 入力) に加え RT も付け、カーソル合成パスで src_copy へ直接焼き込む。
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        // SAFETY: device live、desc 全フィールド初期化済み。
        let tex = unsafe {
            let mut tex = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .context("CreateTexture2D(src copy)")?;
            tex.ok_or_else(|| anyhow!("src copy texture null"))?
        };

        let mut srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: srv_format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        srv_desc.Anonymous.Texture2D.MostDetailedMip = 0;
        srv_desc.Anonymous.Texture2D.MipLevels = 1;
        // SAFETY: tex live、srv_desc は TEXTURE2D 用に初期化済み。
        let srv = unsafe {
            let mut srv = None;
            self.device
                .CreateShaderResourceView(&tex, Some(&srv_desc), Some(&mut srv))
                .context("CreateShaderResourceView")?;
            srv.ok_or_else(|| anyhow!("SRV null"))?
        };

        let mut rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: srv_format,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        rtv_desc.Anonymous.Texture2D.MipSlice = 0;
        // SAFETY: tex live、rtv_desc は TEXTURE2D 用に初期化済み (カーソル合成の描画先)。
        let rtv = unsafe {
            let mut rtv = None;
            self.device
                .CreateRenderTargetView(&tex, Some(&rtv_desc), Some(&mut rtv))
                .context("CreateRenderTargetView(src copy)")?;
            rtv.ok_or_else(|| anyhow!("src copy RTV null"))?
        };

        self.src_copy = Some(tex);
        self.src_srv = Some(srv);
        self.src_rtv = Some(rtv);
        self.src_w = w;
        self.src_h = h;
        self.src_copy_format = copy_format;
        Ok(())
    }

    /// NV12 出力プールを (再)用意する。
    fn ensure_pool(&mut self, w: u32, h: u32) -> Result<()> {
        if !self.pool.is_empty() && self.nv12_w == w && self.nv12_h == h {
            return Ok(());
        }
        self.pool.clear();
        self.pool_idx = 0;
        for _ in 0..POOL_SIZE {
            self.pool.push(self.create_nv12_target(w, h)?);
        }
        self.nv12_w = w;
        self.nv12_h = h;
        tracing::info!(w, h, pool = POOL_SIZE, "NV12 output pool ready");
        Ok(())
    }

    fn create_nv12_target(&self, w: u32, h: u32) -> Result<Nv12Target> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        // SAFETY: device live、desc 初期化済み。NV12 を RT として作成する。
        let tex = unsafe {
            let mut tex = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .context("CreateTexture2D(NV12)")?;
            tex.ok_or_else(|| anyhow!("NV12 texture null"))?
        };

        // plane0 (Y) = R8_UNORM、plane1 (UV) = R8G8_UNORM。D3D11 はビュー形式でプレーンを選ぶ。
        let rtv_y = self.create_plane_rtv(&tex, DXGI_FORMAT_R8_UNORM)?;
        let rtv_uv = self.create_plane_rtv(&tex, DXGI_FORMAT_R8G8_UNORM)?;
        Ok(Nv12Target { tex, rtv_y, rtv_uv })
    }

    fn create_plane_rtv(
        &self,
        tex: &ID3D11Texture2D,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11RenderTargetView> {
        let mut desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        desc.Anonymous.Texture2D.MipSlice = 0;
        // SAFETY: tex live、desc は TEXTURE2D 用に初期化済み。
        unsafe {
            let mut rtv = None;
            self.device
                .CreateRenderTargetView(tex, Some(&desc), Some(&mut rtv))
                .context("CreateRenderTargetView(NV12 plane)")?;
            rtv.ok_or_else(|| anyhow!("RTV null"))
        }
    }
}

// SAFETY: Converter は capture スレッドでのみ使用する。内部 COM は multithread-protected
// device に紐づくため、別スレッドへの move は安全。`MfH264Encoder` の Send 実装と同流儀。
unsafe impl Send for Converter {}

/// 2 の倍数へ切り下げる (NV12 は幅・高さとも偶数が必要)。
fn even(v: u32) -> u32 {
    v & !1
}

/// DXGI ポインタ形状 (color / masked-color / monochrome) を、合成しやすいストレートアルファ
/// BGRA8 (ピッチ = w*4) へ正規化する。戻り値 `(w, h, bgra)`。未知種別は `None`。
///
/// XOR / 反転 (masked-color の AND=0xFF や monochrome の and=1&xor=1) は背景を読まないと
/// 厳密合成できないため不透明で近似する (標準の矢印は不透明な白+黒境界なので正確に出る。
/// I ビーム等の反転カーソルは白で代替し、暗背景で視認できることを優先する)。
fn build_cursor_rgba(
    cur_type: u32,
    width: u32,
    height: u32,
    pitch: u32,
    data: &[u8],
) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h) = (width, height);
    match cur_type {
        CURSOR_TYPE_COLOR => {
            let mut out = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                let row = (y * pitch) as usize;
                for x in 0..w {
                    let si = row + (x * 4) as usize;
                    let di = ((y * w + x) * 4) as usize;
                    if si + 4 <= data.len() {
                        out[di..di + 4].copy_from_slice(&data[si..si + 4]); // B,G,R,A
                    }
                }
            }
            Some((w, h, out))
        }
        CURSOR_TYPE_MASKED_COLOR => {
            // BGRA。A=0 は不透明な色、A=0xFF は画面との XOR (反転)。両方とも不透明で近似する。
            let mut out = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                let row = (y * pitch) as usize;
                for x in 0..w {
                    let si = row + (x * 4) as usize;
                    let di = ((y * w + x) * 4) as usize;
                    if si + 4 <= data.len() {
                        out[di] = data[si];
                        out[di + 1] = data[si + 1];
                        out[di + 2] = data[si + 2];
                        out[di + 3] = 255;
                    }
                }
            }
            Some((w, h, out))
        }
        CURSOR_TYPE_MONOCHROME => {
            // 1bpp、高さは実寸の 2 倍 (上半分 = AND マスク、下半分 = XOR マスク)。
            let ch = h / 2;
            let mut out = vec![0u8; (w * ch * 4) as usize];
            for y in 0..ch {
                let and_row = (y * pitch) as usize;
                let xor_row = ((y + ch) * pitch) as usize;
                for x in 0..w {
                    let byte = (x / 8) as usize;
                    let bit = 7 - (x % 8);
                    let and_b =
                        and_row + byte < data.len() && ((data[and_row + byte] >> bit) & 1) == 1;
                    let xor_b =
                        xor_row + byte < data.len() && ((data[xor_row + byte] >> bit) & 1) == 1;
                    let (b, g, r, a) = match (and_b, xor_b) {
                        (false, false) => (0u8, 0u8, 0u8, 255u8), // 不透明な黒 (境界)
                        (false, true) => (255, 255, 255, 255),    // 不透明な白 (塗り)
                        (true, false) => (0, 0, 0, 0),            // 透明
                        (true, true) => (255, 255, 255, 255),     // 反転 → 白で近似
                    };
                    let di = ((y * w + x) * 4) as usize;
                    out[di] = b;
                    out[di + 1] = g;
                    out[di + 2] = r;
                    out[di + 3] = a;
                }
            }
            Some((w, ch, out))
        }
        _ => None,
    }
}

/// アスペクト比を維持して (src_w, src_h) を高さ `max_height` 以下へ縮小したサイズ (偶数)。
/// `max_height == 0` または src がそれ以下なら元サイズをそのまま返す (= フル解像度)。
fn scaled_dims(src_w: u32, src_h: u32, max_height: u32) -> (u32, u32) {
    if max_height == 0 || src_h <= max_height {
        return (src_w, src_h);
    }
    let out_h = even(max_height).max(2);
    let out_w = even(((src_w as u64 * out_h as u64) / src_h as u64) as u32).max(2);
    (out_w, out_h)
}

fn viewport(w: f32, h: f32) -> D3D11_VIEWPORT {
    D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: w,
        Height: h,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

/// キャプチャ元フォーマット → (コピー先テクスチャ形式, SRV 形式) を決める。
/// SDR は TYPELESS にコピーして UNORM でサンプルし、sRGB 自動デコードを避ける。
fn map_formats(src: DXGI_FORMAT) -> Result<(DXGI_FORMAT, DXGI_FORMAT)> {
    match src {
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => {
            Ok((DXGI_FORMAT_B8G8R8A8_TYPELESS, DXGI_FORMAT_B8G8R8A8_UNORM))
        }
        DXGI_FORMAT_R16G16B16A16_FLOAT => Ok((
            DXGI_FORMAT_R16G16B16A16_TYPELESS,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
        )),
        other => Err(anyhow!(
            "unsupported capture format {:?} (0x{:x})",
            other,
            other.0
        )),
    }
}

/// HLSL をコンパイルする。失敗時はエラーブロブの内容を含めて返す。
///
/// # Safety
/// `SHADER_HLSL` は静的に有効。OUT ブロブは関数内で取り出す。
unsafe fn compile_shader(
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut err: Option<ID3DBlob> = None;
    let src = SHADER_HLSL.as_bytes();
    // SAFETY: src は静的 HLSL のバイト列で ptr/len 整合。OUT の code/err は Option で受ける。
    let result = unsafe {
        D3DCompile(
            src.as_ptr() as *const c_void,
            src.len(),
            s!("convert.hlsl"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut err),
        )
    };
    if let Err(e) = result {
        if let Some(err) = err {
            // SAFETY: err は live なブロブ。ポインタ+サイズは UTF-8 のコンパイルログ。
            let msg = unsafe {
                std::slice::from_raw_parts(err.GetBufferPointer() as *const u8, err.GetBufferSize())
            };
            return Err(anyhow!(
                "D3DCompile failed: {}",
                String::from_utf8_lossy(msg)
            ));
        }
        return Err(anyhow!("D3DCompile failed: {e}"));
    }
    code.ok_or_else(|| anyhow!("D3DCompile returned no bytecode"))
}

/// ブロブのバイト列スライスを得る。
///
/// # Safety
/// `blob` が live な間だけ有効なスライスを返す。呼び出し側で寿命を保証する。
fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: blob は live。GetBufferPointer/Size は有効なバイトコード領域を指す。
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}
