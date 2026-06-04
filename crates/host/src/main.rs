//! Aurora ホスト (操作される側) のエントリポイント。

use anyhow::{Context, Result, anyhow};
use aurora_common::noise::psk_from_password;
use rand::Rng;
use rand::distributions::Alphanumeric;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod audio;
mod autostart;
mod capture;
mod encode;
mod input;
mod net;
mod perf;
mod projfs;
mod secret;

use audio::EncodedAudio;
use capture::{DisplayInfo, Frame, PlatformCapturer, ScreenCapturer};
use encode::{EncodedNal, MAX_BITRATE_BPS, PlatformEncoder, VideoEncoder};
use input::{DisplayGeom, InputInjector, PlatformInjector};
use net::{EncoderCommand, InputCmd, ServerConfig};

/// 既定の送出フレームレート上限 (PLAN.md の 60fps 目標)。
const DEFAULT_FPS_CAP: u32 = 60;
/// 既定の待受ポート (TCP 制御 / UDP 映像で共用)。
const DEFAULT_PORT: u16 = 7000;

#[derive(Debug)]
struct Args {
    dump_bmp: bool,
    dump_h264: Option<usize>,
    /// エンコード遅延・ソースビットレートを 1 秒窓で info ログに出す。
    perf: bool,
    /// 送出フレームレート上限 (fps)。0 で無制限。高リフレッシュレートモニタで
    /// クライアントのデコードを溢れさせ遅延が累積するのを防ぐ。
    fps_cap: u32,
    /// ビットレート上限 (bps)。適応制御の上限 + エンコーダのピーク上限。
    max_bitrate_bps: u32,
    /// 待受ポート (TCP 制御 / UDP 映像で共用)。
    port: u16,
    /// ログオン時自動起動タスクを登録して終了する。
    install: bool,
    /// 自動起動タスクを解除して終了する。
    uninstall: bool,
    /// 接続パスワードを対話入力し DPAPI で暗号化保存して終了する。
    set_password: bool,
    /// 保存済みパスワードを削除して終了する。
    clear_password: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            dump_bmp: false,
            dump_h264: None,
            perf: false,
            fps_cap: DEFAULT_FPS_CAP,
            max_bitrate_bps: MAX_BITRATE_BPS,
            port: DEFAULT_PORT,
            install: false,
            uninstall: false,
            set_password: false,
            clear_password: false,
        }
    }
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump-bmp" => args.dump_bmp = true,
            "--perf" => args.perf = true,
            "--dump-h264" => {
                let n: usize = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("--dump-h264 requires N");
                args.dump_h264 = Some(n);
            }
            "--fps" => {
                args.fps_cap = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("--fps requires N (0 = uncapped)");
            }
            "--max-bitrate" => {
                // 引数は Mbps。内部 bps へ変換する。0 (以下) は無制限 (u32::MAX) を表す。
                let mbps: f64 = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .expect("--max-bitrate requires N (Mbps, 0 = unlimited)");
                args.max_bitrate_bps = if mbps <= 0.0 {
                    u32::MAX
                } else {
                    (mbps * 1_000_000.0) as u32
                };
            }
            "--port" => {
                args.port = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|&p| p != 0)
                    .expect("--port requires N (1..=65535)");
            }
            "--install" => args.install = true,
            "--uninstall" => args.uninstall = true,
            "--set-password" => args.set_password = true,
            "--clear-password" => args.clear_password = true,
            other => {
                eprintln!("unknown arg: {other}");
            }
        }
    }
    args
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "aurora=info".into()))
        .init();

    let args = parse_args();

    if args.install {
        // --install / --uninstall 自体を除いた起動引数を、登録するタスクへ転送する。
        let forward: Vec<String> = std::env::args()
            .skip(1)
            .filter(|a| a != "--install" && a != "--uninstall")
            .collect();
        return autostart::install(&forward);
    }
    if args.uninstall {
        return autostart::uninstall();
    }
    if args.set_password {
        return set_password_interactive();
    }
    if args.clear_password {
        secret::clear_password()?;
        println!("stored password cleared");
        return Ok(());
    }

    if args.dump_bmp {
        return dump_one_bmp();
    }
    if let Some(n) = args.dump_h264 {
        return dump_h264(n);
    }

    run_server(args.perf, args.fps_cap, args.max_bitrate_bps, args.port)
}

fn dump_one_bmp() -> Result<()> {
    let device = capture::create_shared_device()?;
    let mut cap = PlatformCapturer::new_primary(device.clone())?;
    // 最初は更新がない場合があるので数回試す
    for _ in 0..30 {
        if let Some(frame) = cap.next_frame(Duration::from_millis(200))? {
            let bgra = frame_to_bgra(&device, &frame)?;
            write_bmp("capture.bmp", frame.width, frame.height, &bgra)?;
            tracing::info!(w = frame.width, h = frame.height, "captured -> capture.bmp");
            return Ok(());
        }
    }
    Err(anyhow!("no frame captured in 30 tries"))
}

fn dump_h264(n: usize) -> Result<()> {
    let device = capture::create_shared_device()?;
    let mut cap = PlatformCapturer::new_primary(device.clone())?;
    let mut enc = PlatformEncoder::new(device, MAX_BITRATE_BPS)?;
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

/// 接続パスワードを対話入力 (確認のため 2 回) し、DPAPI で暗号化保存する。
/// 保存はこのコマンドを実行したユーザーの鍵で行われるため、ログオン時タスクと
/// **同じユーザー**で実行すること (昇格は不要)。
fn set_password_interactive() -> Result<()> {
    use std::io::Write;

    print!("Enter new Aurora password: ");
    std::io::stdout().flush().ok();
    let pw1 = read_secret_line()?;
    print!("Confirm password: ");
    std::io::stdout().flush().ok();
    let pw2 = read_secret_line()?;

    if pw1 != pw2 {
        return Err(anyhow!("passwords do not match"));
    }
    if pw1.is_empty() {
        return Err(anyhow!("password must not be empty"));
    }
    secret::save_password(&pw1)?;
    println!("password saved to secure store (DPAPI, current user)");
    Ok(())
}

/// 標準入力から 1 行をエコーなしで読み取る (パスワード入力用)。
#[cfg(target_os = "windows")]
fn read_secret_line() -> Result<String> {
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        SetConsoleMode,
    };

    // SAFETY: 標準入力ハンドルを取得し、エコーフラグだけを一時的に外す。コンソール以外
    // (パイプ等) では GetConsoleMode が失敗するので、その場合は素通しで読み取る。
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.context("GetStdHandle")?;
    let mut orig = CONSOLE_MODE(0);
    let have_mode = unsafe { GetConsoleMode(handle, &mut orig) }.is_ok();
    if have_mode {
        // SAFETY: 直前に取得したハンドル/モードに対しエコーのみ無効化する。
        let _ = unsafe { SetConsoleMode(handle, orig & !ENABLE_ECHO_INPUT) };
    }

    let mut line = String::new();
    let res = std::io::stdin().read_line(&mut line);

    if have_mode {
        // SAFETY: 元のモードへ確実に復帰させる。
        let _ = unsafe { SetConsoleMode(handle, orig) };
    }
    println!(); // エコーされない改行を補う
    res.context("read password from stdin")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(not(target_os = "windows"))]
fn read_secret_line() -> Result<String> {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read password from stdin")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn run_server(perf: bool, fps_cap: u32, max_bitrate_bps: u32, port: u16) -> Result<()> {
    // DPAPI で保存済みのパスワードがあればそれを使う (再起動しても固定・無人起動で読める)。
    // 無ければランダム生成して表示する (手動/開発・初回起動向け)。機密はログに出さない
    // 方針のため、保存済みを使う場合は値を表示しない。
    println!("== aurora-host ready ==");
    let password = match secret::load_password() {
        Ok(Some(p)) if !p.is_empty() => {
            println!("password: (from secure store)");
            p
        }
        Ok(_) => {
            let generated: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(10)
                .map(char::from)
                .collect();
            println!("password: {generated}");
            println!("(no stored password; run `aurora-host --set-password` to set a fixed one)");
            generated
        }
        Err(e) => {
            // 保存はされているが復号できない (別ユーザー等)。ランダム生成で継続する。
            tracing::warn!(error = %e, "could not load stored password; using a random one");
            let generated: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(10)
                .map(char::from)
                .collect();
            println!("password: {generated}");
            generated
        }
    };
    println!("TCP/UDP: 0.0.0.0:{port}");

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

    // capture・convert・encode で共有する D3D11 デバイスを 1 つだけ生成する
    // (Windows のみ。他 OS は `()`)。ディスプレイ切替や MFT 共有のため持続させる。
    let device = capture::create_shared_device()?;

    let (frame_tx, frame_rx) = mpsc::channel::<Frame>(2);
    let (nal_tx, nal_rx) = mpsc::channel::<EncodedNal>(16);
    let (enc_cmd_tx, enc_cmd_rx) = mpsc::channel::<EncoderCommand>(8);
    let (input_tx, input_rx) = mpsc::channel::<InputCmd>(256);
    let (audio_tx, audio_rx) = mpsc::channel::<EncodedAudio>(64);
    // 制御スレッド → capture スレッドへ「キャプチャ対象ディスプレイ id」を渡す。
    let (cap_select_tx, cap_select_rx) = std::sync::mpsc::channel::<u32>();
    let (clip_handle, clip_change_rx) = aurora_common::clipboard::spawn();
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
    if fps_cap == 0 {
        tracing::info!("frame rate cap: uncapped");
    } else {
        tracing::info!(fps = fps_cap, "frame rate cap");
    }
    if max_bitrate_bps == u32::MAX {
        tracing::info!("bitrate cap: unlimited");
    } else {
        tracing::info!(
            max_mbps = max_bitrate_bps as f64 / 1_000_000.0,
            "bitrate cap (PeakConstrainedVBR peak)"
        );
    }
    // 実行時 fps 変更 (クライアントの SetVideoParams) を capture スレッドへ届けるチャネル。
    let (fps_tx, fps_rx) = std::sync::mpsc::channel::<u32>();
    let cap_device = device.clone();
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(
                cap_device,
                frame_tx,
                res_tx,
                cap_select_rx,
                initial_id,
                fps_cap,
                fps_rx,
            ) {
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
            if let Err(e) = encode_loop(device, frame_rx, nal_tx, enc_cmd_rx, perf, max_bitrate_bps)
            {
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
            tcp_addr: SocketAddr::from(([0, 0, 0, 0], port)),
            udp_addr: SocketAddr::from(([0, 0, 0, 0], port)),
            psk,
            displays,
            audio: audio_enabled,
            max_bitrate_bps,
        };
        net::run_server(
            cfg,
            nal_rx,
            audio_rx,
            enc_cmd_tx,
            input_tx,
            cap_select_tx,
            fps_tx,
            clip_apply_tx,
            clip_change_rx,
        )
        .await
    })
}

fn capture_loop(
    device: capture::SharedDevice,
    tx: mpsc::Sender<Frame>,
    res_tx: std::sync::mpsc::SyncSender<(u32, u32)>,
    select_rx: std::sync::mpsc::Receiver<u32>,
    initial_id: u32,
    fps_cap: u32,
    fps_rx: std::sync::mpsc::Receiver<u32>,
) -> Result<()> {
    // capture スレッドで COM/MF を初期化する (Windows のみ実体あり)。
    #[cfg(target_os = "windows")]
    capture::device::init_thread_com()?;

    // 送出ペーシング: 高リフレッシュレートモニタだと next_frame が毎リフレッシュ
    // (例 144Hz) でフレームを返し、全部 encode/送信するとクライアントのデコードが
    // 追いつかずバックログが累積して遅延が増える。前回送出から min_interval 経過した
    // フレームだけを encode 側へ流す (0 = 無制限)。
    let fps_to_interval = |fps: u32| {
        if fps == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(1.0 / fps as f64)
        }
    };
    let mut min_interval = fps_to_interval(fps_cap);
    // 初回フレームは即送出できるよう過去側に倒しておく。
    let mut last_sent = Instant::now()
        .checked_sub(min_interval)
        .unwrap_or_else(Instant::now);

    let mut cur_id = initial_id;
    let mut cap = PlatformCapturer::new_for_output(device.clone(), cur_id)?;
    let mut reported = false;
    loop {
        // 実行時の fps 上限変更 (クライアント SetVideoParams) を反映する。
        while let Ok(fps) = fps_rx.try_recv() {
            min_interval = fps_to_interval(fps);
            tracing::info!(fps, "fps cap updated");
        }
        // ディスプレイ切替要求を吸う。新規 capturer 構築に失敗したら現状維持。
        while let Ok(id) = select_rx.try_recv() {
            if id != cur_id {
                match PlatformCapturer::new_for_output(device.clone(), id) {
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
            // fps 上限ペーシング: 間隔未満のフレームは捨てる (最新フレームを送るため
            // ここで間引く)。0 (無制限) のときは常に送る。
            let now = Instant::now();
            if min_interval.is_zero() || now.duration_since(last_sent) >= min_interval {
                last_sent = now;
                // try_send: バックプレッシャ。詰まったら捨てる。更新なしのときは None
                let _ = tx.try_send(f);
            }
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
    device: capture::SharedDevice,
    mut frame_rx: mpsc::Receiver<Frame>,
    nal_tx: mpsc::Sender<EncodedNal>,
    mut cmd_rx: mpsc::Receiver<EncoderCommand>,
    perf: bool,
    max_bitrate_bps: u32,
) -> Result<()> {
    let mut enc = PlatformEncoder::new(device, max_bitrate_bps)?;
    let mut meter = perf.then(perf::EncodeMeter::new);
    // 送信バックプレッシャを計測から除くため、NAL を一旦ここへ集めてから送る。
    let mut nals: Vec<EncodedNal> = Vec::new();
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
                let t0 = std::time::Instant::now();
                if let Err(e) = enc.push_frame(&frame) {
                    tracing::warn!(error = %e, "push_frame failed");
                    continue;
                }
                nals.clear();
                while let Some(nal) = enc.poll_nal()? {
                    nals.push(nal);
                }
                if let Some(m) = meter.as_mut() {
                    let encode_us = t0.elapsed().as_micros() as u32;
                    let nal_bytes: usize = nals.iter().map(|n| n.data.len()).sum();
                    m.record(encode_us, nal_bytes);
                    m.maybe_flush();
                }
                for nal in nals.drain(..) {
                    if nal_tx.blocking_send(nal).is_err() {
                        return Ok(());
                    }
                }
            }
            None => return Ok(()),
        }
    }
}

/// デバッグ用: `Frame` を行詰め BGRA8 に変換する。Windows は NV12 テクスチャを読み戻し、
/// 他 OS は保持済みの BGRA をそのまま返す。
#[cfg(target_os = "windows")]
fn frame_to_bgra(device: &capture::SharedDevice, frame: &Frame) -> Result<Vec<u8>> {
    capture::debug_frame_to_bgra(device, frame)
}
#[cfg(not(target_os = "windows"))]
fn frame_to_bgra(_device: &capture::SharedDevice, frame: &Frame) -> Result<Vec<u8>> {
    Ok(frame.bgra.clone())
}

/// 24bit BMP (BGR, ボトムアップ) を保存。`bgra` は `width*4` 行詰めの BGRA8。
fn write_bmp(path: &str, width: u32, height: u32, bgra: &[u8]) -> Result<()> {
    use std::io::Write;
    let w = width as i32;
    let h = height as i32;
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
    let stride = w as usize * 4;
    let pad = vec![0u8; (row_bytes as usize) - (w as usize * 3)];
    for y in (0..h as usize).rev() {
        let row = &bgra[y * stride..y * stride + w as usize * 4];
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
