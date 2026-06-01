//! DDD ホスト (操作される側) のエントリポイント。

use anyhow::{Context, Result, anyhow};
use ddd_common::noise::psk_from_password;
use ddd_common::proto::Display;
use rand::Rng;
use rand::distributions::Alphanumeric;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod capture;
mod encode;
mod net;

use capture::{Frame, PlatformCapturer, ScreenCapturer};
use encode::{EncodedNal, PlatformEncoder, VideoEncoder};
use net::{EncoderCommand, ServerConfig};

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

    let (frame_tx, frame_rx) = mpsc::channel::<Frame>(2);
    let (nal_tx, nal_rx) = mpsc::channel::<EncodedNal>(16);
    let (enc_cmd_tx, enc_cmd_rx) = mpsc::channel::<EncoderCommand>(8);

    // capture スレッド
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(frame_tx) {
                tracing::error!(error = %e, "capture loop ended");
            }
        })?;

    // encode スレッド
    std::thread::Builder::new()
        .name("encode".into())
        .spawn(move || {
            if let Err(e) = encode_loop(frame_rx, nal_tx, enc_cmd_rx) {
                tracing::error!(error = %e, "encode loop ended");
            }
        })?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let cfg = ServerConfig {
            tcp_addr: "0.0.0.0:7000".parse().unwrap(),
            udp_addr: "0.0.0.0:7000".parse().unwrap(),
            psk,
            display: Display {
                id: 0,
                width: 1920,
                height: 1080,
                name: "primary".to_string(),
            },
        };
        net::run_server(cfg, nal_rx, enc_cmd_tx).await
    })
}

fn capture_loop(tx: mpsc::Sender<Frame>) -> Result<()> {
    let mut cap = PlatformCapturer::new_primary()?;
    loop {
        if let Some(f) = cap.next_frame(Duration::from_millis(100))? {
            // try_send: バックプレッシャ。詰まったら捨てる。更新なしのときは None
            let _ = tx.try_send(f);
        }
    }
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
