//! DDD ホスト (操作される側) のエントリポイント。

use anyhow::{Context, Result, anyhow};
use ddd_common::noise::psk_from_password;
use rand::Rng;
use rand::distributions::Alphanumeric;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod audio;
mod capture;
mod encode;
mod input;
mod net;
mod projfs;

use audio::EncodedAudio;
use capture::{DisplayInfo, Frame, PlatformCapturer, ScreenCapturer};
use encode::{EncodedNal, PlatformEncoder, VideoEncoder};
use input::{DisplayGeom, InputInjector, PlatformInjector};
use net::{EncoderCommand, InputCmd, ServerConfig};

#[derive(Debug, Default)]
struct Args {
    dump_bmp: bool,
    dump_h264: Option<usize>,
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump-bmp" => args.dump_bmp = true,
            "--dump-h264" => {
                let n: usize = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("--dump-h264 requires N");
                args.dump_h264 = Some(n);
            }
            other => {
                eprintln!("unknown arg: {other}");
            }
        }
    }
    args
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "ddd=info".into()))
        .init();

    let args = parse_args();

    if args.dump_bmp {
        return dump_one_bmp();
    }
    if let Some(n) = args.dump_h264 {
        return dump_h264(n);
    }

    run_server()
}

fn dump_one_bmp() -> Result<()> {
    let mut cap = PlatformCapturer::new_primary()?;
    // 最初は更新がない場合があるので数回試す
    for _ in 0..30 {
        if let Some(frame) = cap.next_frame(Duration::from_millis(200))? {
            write_bmp("capture.bmp", &frame)?;
            tracing::info!(w = frame.width, h = frame.height, "captured -> capture.bmp");
            return Ok(());
        }
    }
    Err(anyhow!("no frame captured in 30 tries"))
}

fn dump_h264(n: usize) -> Result<()> {
    let mut cap = PlatformCapturer::new_primary()?;
    let mut enc = PlatformEncoder::new()?;
    let mut out = std::fs::File::create("out.h264").context("create out.h264")?;
    use std::io::Write;

    let mut produced = 0;
    while produced < n {
        let frame = loop {
            if let Some(f) = cap.next_frame(Duration::from_millis(200))? {
                break f;
            }
        };
        enc.push_frame(&frame)?;
        while let Some(nal) = enc.poll_nal()? {
            out.write_all(&nal.data)?;
            produced += 1;
            if produced % 30 == 0 {
                tracing::info!(produced, "encoded");
            }
            if produced >= n {
                break;
            }
        }
    }
    tracing::info!(produced, "wrote out.h264");
    Ok(())
}

fn run_server() -> Result<()> {
    let password: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(10)
        .map(char::from)
        .collect();
    println!("== ddd-host ready ==");
    println!("password: {password}");
    println!("TCP/UDP: 0.0.0.0:7000");

    let psk = psk_from_password(&password);

    // 接続済みディスプレイを列挙する。失敗時は 1080p 単一ディスプレイで継続。
    let mut displays = match capture::enumerate_displays() {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(error = %e, "display enumeration failed, defaulting to 1920x1080");
            vec![DisplayInfo {
                id: 0,
                name: "primary".to_string(),
                width: 1920,
                height: 1080,
                x: 0,
                y: 0,
            }]
        }
    };
    for d in &displays {
        tracing::info!(id = d.id, name = %d.name, w = d.width, h = d.height, x = d.x, y = d.y, "display");
    }
    // 初期キャプチャ対象 (先頭 = primary)。
    let initial_id = displays[0].id;

    let (frame_tx, frame_rx) = mpsc::channel::<Frame>(2);
    let (nal_tx, nal_rx) = mpsc::channel::<EncodedNal>(16);
    let (enc_cmd_tx, enc_cmd_rx) = mpsc::channel::<EncoderCommand>(8);
    let (input_tx, input_rx) = mpsc::channel::<InputCmd>(256);
    let (audio_tx, audio_rx) = mpsc::channel::<EncodedAudio>(64);
    // 制御スレッド → capture スレッドへ「キャプチャ対象ディスプレイ id」を渡す。
    let (cap_select_tx, cap_select_rx) = std::sync::mpsc::channel::<u32>();
    let (clip_handle, clip_change_rx) = ddd_common::clipboard::spawn();
    let clip_apply_tx = clip_handle.apply_tx.clone();

    // 音声ループバック取り込み。失敗してもホストは映像のみで継続する。
    let audio_enabled = match audio::spawn_capture(audio_tx) {
        Ok(()) => {
            tracing::info!("audio loopback capture started");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "audio capture disabled");
            false
        }
    };

    // capture スレッド: 選択ディスプレイをキャプチャし、(再)構築直後の実解像度を res_tx で通知。
    let (res_tx, res_rx) = std::sync::mpsc::sync_channel::<(u32, u32)>(1);
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(frame_tx, res_tx, cap_select_rx, initial_id) {
                tracing::error!(error = %e, "capture loop ended");
            }
        })?;

    // 初期ディスプレイの実解像度を待ち、列挙値 (mac/linux はフォールバック) を上書きする。
    if let Ok((width, height)) = res_rx.recv_timeout(Duration::from_secs(10)) {
        tracing::info!(width, height, "active display resolution");
        displays[0].width = width;
        displays[0].height = height;
    } else {
        tracing::warn!("active display resolution unknown, using enumerated value");
    }

    let initial_geom = DisplayGeom {
        x: displays[0].x,
        y: displays[0].y,
        width: displays[0].width,
        height: displays[0].height,
    };

    // encode スレッド
    std::thread::Builder::new()
        .name("encode".into())
        .spawn(move || {
            if let Err(e) = encode_loop(frame_rx, nal_tx, enc_cmd_rx) {
                tracing::error!(error = %e, "encode loop ended");
            }
        })?;

    // input 注入スレッド (SendInput は同期APIなので tokio の外で実行)
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            if let Err(e) = input_loop(input_rx, initial_geom) {
                tracing::error!(error = %e, "input loop ended");
            }
        })?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let cfg = ServerConfig {
            tcp_addr: "0.0.0.0:7000".parse().unwrap(),
            udp_addr: "0.0.0.0:7000".parse().unwrap(),
            psk,
            displays,
            audio: audio_enabled,
        };
        net::run_server(
            cfg,
            nal_rx,
            audio_rx,
            enc_cmd_tx,
            input_tx,
            cap_select_tx,
            clip_apply_tx,
            clip_change_rx,
        )
        .await
    })
}

fn capture_loop(
    tx: mpsc::Sender<Frame>,
    res_tx: std::sync::mpsc::SyncSender<(u32, u32)>,
    select_rx: std::sync::mpsc::Receiver<u32>,
    initial_id: u32,
) -> Result<()> {
    let mut cur_id = initial_id;
    let mut cap = PlatformCapturer::new_for_output(cur_id)?;
    let mut reported = false;
    loop {
        // ディスプレイ切替要求を吸う。新規 capturer 構築に失敗したら現状維持。
        while let Ok(id) = select_rx.try_recv() {
            if id != cur_id {
                match PlatformCapturer::new_for_output(id) {
                    Ok(c) => {
                        cap = c;
                        cur_id = id;
                        reported = false;
                        tracing::info!(id, "switched capture display");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, id, "switch capture failed; keeping current");
                    }
                }
            }
        }
        if let Some(f) = cap.next_frame(Duration::from_millis(100))? {
            // (再)構築直後の最初のフレーム寸法を実解像度として通知する。
            if !reported {
                let _ = res_tx.try_send((f.width, f.height));
                reported = true;
            }
            // try_send: バックプレッシャ。詰まったら捨てる。更新なしのときは None
            let _ = tx.try_send(f);
        }
    }
}

fn input_loop(mut rx: mpsc::Receiver<InputCmd>, initial_geom: DisplayGeom) -> Result<()> {
    let mut injector = PlatformInjector::new(initial_geom);
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            InputCmd::Event(ev) => {
                if let Err(e) = injector.inject(&ev) {
                    tracing::warn!(error = %e, ?ev, "input inject failed");
                }
            }
            InputCmd::SetDisplay(geom) => {
                injector.set_display(geom);
                tracing::info!(?geom, "input display switched");
            }
        }
    }
    Ok(())
}

fn encode_loop(
    mut frame_rx: mpsc::Receiver<Frame>,
    nal_tx: mpsc::Sender<EncodedNal>,
    mut cmd_rx: mpsc::Receiver<EncoderCommand>,
) -> Result<()> {
    let mut enc = PlatformEncoder::new()?;
    loop {
        // ノンブロックでコマンドを吸う
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                EncoderCommand::RequestKeyframe => enc.request_keyframe(),
                EncoderCommand::SetBitrate(bps) => {
                    if let Err(e) = enc.set_bitrate(bps) {
                        tracing::warn!(error = %e, bps, "set_bitrate failed");
                    }
                }
            }
        }
        match frame_rx.blocking_recv() {
            Some(frame) => {
                if let Err(e) = enc.push_frame(&frame) {
                    tracing::warn!(error = %e, "push_frame failed");
                    continue;
                }
                while let Some(nal) = enc.poll_nal()? {
                    if nal_tx.blocking_send(nal).is_err() {
                        return Ok(());
                    }
                }
            }
            None => return Ok(()),
        }
    }
}

/// 24bit BMP (BGR, ボトムアップ) を保存。
fn write_bmp(path: &str, frame: &Frame) -> Result<()> {
    use std::io::Write;
    let w = frame.width as i32;
    let h = frame.height as i32;
    let row_bytes = ((w * 3 + 3) / 4) * 4;
    let pixel_bytes = (row_bytes * h) as u32;
    let file_size: u32 = 14 + 40 + pixel_bytes;

    let mut f = std::fs::File::create(path)?;
    // BITMAPFILEHEADER
    f.write_all(b"BM")?;
    f.write_all(&file_size.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    // BITMAPINFOHEADER
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&w.to_le_bytes())?;
    f.write_all(&h.to_le_bytes())?; // 正値=ボトムアップ
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&24u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&pixel_bytes.to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    // ピクセル (ボトムアップ)
    let stride = frame.stride as usize;
    let pad = vec![0u8; (row_bytes as usize) - (w as usize * 3)];
    for y in (0..h as usize).rev() {
        let row = &frame.bgra[y * stride..y * stride + w as usize * 4];
        let mut bgr = Vec::with_capacity(w as usize * 3);
        for px in row.chunks_exact(4) {
            bgr.push(px[0]);
            bgr.push(px[1]);
            bgr.push(px[2]);
        }
        f.write_all(&bgr)?;
        f.write_all(&pad)?;
    }
    Ok(())
}
