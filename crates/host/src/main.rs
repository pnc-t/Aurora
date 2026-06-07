//! Aurora ホスト (操作される側) のエントリポイント。
//!
//! リリースビルドの Windows では GUI サブシステムにしてコンソール窓を出さず、システム
//! トレイ常駐で動かす (出先 PC での無人/バックグラウンド運用が主用途)。CLI サブコマンド
//! (`--set-password` 等) を端末から実行したときだけ親コンソールへ接続して入出力する。
//! デバッグビルドはログ確認のためコンソールサブシステムのまま。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::{Context, Result, anyhow};
use aurora_common::noise::psk_from_password;
use rand::Rng;
use rand::distributions::Alphanumeric;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod audio;
mod autostart;
mod capture;
mod conn;
mod encode;
mod input;
mod net;
mod perf;
mod projfs;
mod secret;
mod settings;
mod tray;

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
    /// `--port` が明示指定されたか。明示時は保存値 (トレイ管理) より CLI を優先する。
    port_explicit: bool,
    /// 待受アドレス (`--bind`)。未指定なら Tailscale IP を自動採用し、無ければ 0.0.0.0。
    /// 本プロジェクトは Tailnet 前提のため、既定で tailnet インターフェースのみに公開して
    /// LAN/インターネットへの露出 (総当たり・接続枠 DoS) を避ける。`--bind 0.0.0.0` で
    /// 従来どおり全 NIC 公開 (tailnet 済みマシンでの LAN テスト用) に上書きできる。
    bind: Option<String>,
    /// ログオン時自動起動タスクを登録して終了する。
    install: bool,
    /// 自動起動タスクを解除して終了する。
    uninstall: bool,
    /// 接続パスワードを対話入力し DPAPI で暗号化保存して終了する。
    set_password: bool,
    /// 保存済みパスワードを削除して終了する。
    clear_password: bool,
    /// ログレベル指定。トレイ常駐 (コンソールなし) や自動起動では RUST_LOG を渡せないため、
    /// 引数でファイルログのレベルを上げられるようにする。`debug` 等の素のレベル、または
    /// `aurora=debug,snow=warn` のような完全なディレクティブを受け付ける。
    log_level: Option<String>,
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
            port_explicit: false,
            install: false,
            uninstall: false,
            set_password: false,
            clear_password: false,
            log_level: None,
            bind: None,
        }
    }
}

/// `--flag <value>` の値を取り出す。欠落時はパニックせずフレンドリーに終了する
/// (CLI 引数ミスはユーザーエラーなので panic のバックトレースより明示メッセージが親切)。
fn take_value<I: Iterator<Item = String>>(it: &mut I, flag: &str) -> String {
    it.next().unwrap_or_else(|| {
        eprintln!("error: {flag} requires a value");
        std::process::exit(2);
    })
}

/// `--flag <value>` を取り出して `T` へパースする。欠落・不正値はクリーン終了する。
fn parse_value<T, I>(it: &mut I, flag: &str, hint: &str) -> T
where
    T: std::str::FromStr,
    I: Iterator<Item = String>,
{
    let raw = take_value(it, flag);
    raw.parse().unwrap_or_else(|_| {
        eprintln!("error: {flag} expects {hint} (got '{raw}')");
        std::process::exit(2);
    })
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump-bmp" => args.dump_bmp = true,
            "--perf" => args.perf = true,
            "--dump-h264" => {
                args.dump_h264 = Some(parse_value(&mut it, "--dump-h264", "an integer N"));
            }
            "--fps" => {
                args.fps_cap = parse_value(&mut it, "--fps", "N (0 = uncapped)");
            }
            "--max-bitrate" => {
                // 引数は Mbps。内部 bps へ変換する。0 (以下) は無制限 (u32::MAX) を表す。
                let mbps: f64 = parse_value(&mut it, "--max-bitrate", "N in Mbps (0 = unlimited)");
                args.max_bitrate_bps = if mbps <= 0.0 {
                    u32::MAX
                } else {
                    (mbps * 1_000_000.0) as u32
                };
            }
            "--port" => {
                let port: u16 = parse_value(&mut it, "--port", "N (1..=65535)");
                if port == 0 {
                    eprintln!("error: --port must be 1..=65535");
                    std::process::exit(2);
                }
                args.port = port;
                args.port_explicit = true;
            }
            "--install" => args.install = true,
            "--uninstall" => args.uninstall = true,
            "--set-password" => args.set_password = true,
            "--clear-password" => args.clear_password = true,
            "--log-level" => {
                args.log_level = Some(take_value(&mut it, "--log-level"));
            }
            "--bind" => {
                args.bind = Some(take_value(&mut it, "--bind"));
            }
            other => {
                eprintln!("unknown arg: {other}");
            }
        }
    }
    args
}

fn main() -> Result<()> {
    let args = parse_args();

    // CLI サブコマンドは端末への入出力が要る。既定のサーバ起動はバックグラウンド
    // (トレイ常駐) で、リリースでは GUI サブシステムのためコンソールが無い。
    let needs_console = args.install
        || args.uninstall
        || args.set_password
        || args.clear_password
        || args.dump_bmp
        || args.dump_h264.is_some();

    // CLI のときだけ親プロセスのコンソールへ接続する (サーバ起動では開かない)。
    #[cfg(windows)]
    if needs_console {
        // set/clear-password は現在パスワードを対話入力させるため、親コンソールが無ければ
        // 新規割り当てする (ダブルクリック起動でも入力欄が出るように)。
        attach_console(args.set_password || args.clear_password);
    }

    // コンソールが無いバックグラウンドサーバ (リリース) はログをファイルへ。それ以外
    // (CLI / デバッグ) は従来どおりコンソールへ出す。
    init_tracing(
        !needs_console && !cfg!(debug_assertions),
        args.log_level.as_deref(),
    );

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
        // set-password と対称に、接続中拒否 + 現在パスワード照合を要求してから削除する
        // (素通しだと接続中の操作者が認証を消して締め出し/乗っ取りできてしまう)。
        require_password_change_authorization()?;
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

    // 待受ポートの解決: `--port` 明示なら CLI を、無ければ保存値 (トレイ管理) を、それも
    // 無ければ既定を使う。
    let port = if args.port_explicit {
        args.port
    } else {
        settings::load_port().unwrap_or(args.port)
    };
    let bind_ip = resolve_bind_ip(args.bind.as_deref());
    run_server(args.perf, args.fps_cap, args.max_bitrate_bps, port, bind_ip)
}

/// 待受アドレスを解決する。
///
/// 優先順位: `--bind` 明示 > Tailscale IP 自動検出 > `0.0.0.0`。本プロジェクトは Tailnet
/// 前提のため、既定では tailnet インターフェースのみに公開して LAN/インターネットへの
/// 露出を避ける。Tailscale 未導入 (CLI 不在 / IP 取得失敗) のときだけ `0.0.0.0` に
/// フォールバックし、LAN/localhost での動作確認フローを壊さない。
fn resolve_bind_ip(explicit: Option<&str>) -> std::net::IpAddr {
    use std::net::{IpAddr, Ipv4Addr};
    if let Some(s) = explicit {
        return s.parse().unwrap_or_else(|_| {
            eprintln!("error: --bind expects an IP address (got '{s}')");
            std::process::exit(2);
        });
    }
    match tailscale_ip() {
        Some(ip) => {
            tracing::info!(%ip, "binding to Tailscale interface (default; override with --bind)");
            ip
        }
        None => {
            tracing::warn!(
                "Tailscale IP not found; binding to 0.0.0.0 (all interfaces). \
                 Install Tailscale or pass --bind to restrict exposure."
            );
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        }
    }
}

/// `tailscale ip -4` を実行して tailnet IPv4 を取得する。CLI 不在・非ゼロ終了・パース失敗は
/// `None`。CGNAT 範囲 (100.64/10) のインターフェース誤検出を避けるため、権威ある CLI に問う。
fn tailscale_ip() -> Option<std::net::IpAddr> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    // 複数アドレスが返りうる (1 行 1 つ)。先頭の有効な IPv4 を採る。
    text.lines()
        .map(str::trim)
        .find_map(|l| l.parse::<std::net::Ipv4Addr>().ok())
        .map(std::net::IpAddr::V4)
}

/// ログフィルタを構築する。`--log-level` 引数が最優先。素のレベル (`debug`) は `aurora=`
/// を補い、`=` を含むなら完全なディレクティブとして扱う。無効・未指定なら `RUST_LOG`、
/// それも無ければ既定の `aurora=info`。
fn build_log_filter(level: Option<&str>) -> EnvFilter {
    if let Some(l) = level {
        let directive = if l.contains('=') {
            l.to_string()
        } else {
            format!("aurora={l}")
        };
        if let Ok(f) = EnvFilter::try_new(&directive) {
            return f;
        }
        eprintln!("invalid --log-level '{l}', falling back to RUST_LOG/default");
    }
    EnvFilter::try_from_default_env().unwrap_or_else(|_| "aurora=info".into())
}

/// `tracing` を初期化する。`to_file` が真なら `%LOCALAPPDATA%\Aurora\host.log` へ書き、
/// 開けない場合や偽のときはコンソール (stdout) へ出す。フィルタの優先順位は
/// `--log-level` 引数 > `RUST_LOG` 環境変数 > 既定 (`aurora=info`)。
#[cfg_attr(not(windows), allow(unused_variables))]
fn init_tracing(to_file: bool, level: Option<&str>) {
    let filter = build_log_filter(level);
    #[cfg(windows)]
    if to_file {
        if let Some(file) = open_log_file() {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
            return;
        }
    }
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// ログ出力先ファイルを追記モードで開く。一定サイズを超えていたら作り直す (簡易
/// ローテーション)。ディレクトリや環境変数が無ければ `None`。
#[cfg(windows)]
fn open_log_file() -> Option<std::fs::File> {
    /// これを超えたら次回起動時に作り直す (約 5MiB)。
    const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

    let base = std::env::var_os("LOCALAPPDATA")?;
    let mut dir = std::path::PathBuf::from(base);
    dir.push("Aurora");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("host.log");

    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true);
    if too_big {
        opts.truncate(true);
    } else {
        opts.append(true);
    }
    opts.open(&path).ok()
}

/// CLI サブコマンドのためにコンソールを用意する。端末から起動された場合は親プロセスの
/// コンソールへ接続して入出力する。`alloc_if_none` が真で親コンソールが無い場合 (例:
/// ダブルクリック起動) は新規コンソールを割り当てる (`--set-password` の対話入力用)。
/// デバッグビルドは既にコンソールサブシステムなので `AttachConsole` は失敗するが無害。
#[cfg(windows)]
fn attach_console(alloc_if_none: bool) {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole};
    // SAFETY: コンソールへの接続/割り当てのみ。失敗は無視する (接続済み・親無し等)。
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() && alloc_if_none {
            let _ = AllocConsole();
        }
    }
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
///
/// セキュリティ: リモートクライアントは入力注入で端末を開きこのコマンドを実行できる。
/// (1) 稼働中サーバが接続中なら拒否する。(2) 既存パスワードがある場合は現在の
/// パスワードを要求して照合する。これによりリモートからの締め出し/乗っ取りを防ぐ。
/// パスワードの変更/削除を許可してよいか確認する共通ガード。接続中は拒否し、保存済み
/// パスワードがあれば現在のパスワードを対話入力させて照合する。トレイ UI のパスワード変更と
/// 対称の防御を CLI (`--set-password` / `--clear-password`) にも与える。接続中の判定は名前付き
/// イベント経由でプロセス間共有される (`conn`)。リモート操作者は入力注入でホストの cmd を
/// 開き CLI を叩けるため、トレイだけ硬化しても CLI が素通しでは締め出し/乗っ取りを防げない。
fn require_password_change_authorization() -> Result<()> {
    use std::io::Write;

    // 接続中は変更を拒否する (リモート操作者による締め出し/乗っ取り防止)。
    if conn::is_active() {
        return Err(anyhow!(
            "クライアント接続中はパスワードを変更できません。切断してから実行してください"
        ));
    }

    // 既存パスワードがあるなら、まず現在のパスワードを要求して本人確認する。
    if let Ok(Some(current)) = secret::load_password() {
        if !current.is_empty() {
            print!("Enter current Aurora password: ");
            std::io::stdout().flush().ok();
            let entered = read_secret_line()?;
            if entered != current {
                return Err(anyhow!("current password does not match"));
            }
        }
    }
    Ok(())
}

fn set_password_interactive() -> Result<()> {
    use std::io::Write;

    require_password_change_authorization()?;

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

fn run_server(
    perf: bool,
    fps_cap: u32,
    max_bitrate_bps: u32,
    port: u16,
    bind_ip: std::net::IpAddr,
) -> Result<()> {
    // DPAPI で保存済みのパスワードがあればそれを使う (再起動しても固定・無人起動で読める)。
    // 無ければランダム生成して表示する (手動/開発・初回起動向け)。機密はログに出さない
    // 方針のため、保存済みを使う場合は値を表示しない。
    println!("== aurora-host ready ==");
    // 保存パスワードの有無を覚えておく。無ければトレイ起動直後に設定ダイアログを促す。
    let mut have_stored = false;
    let password = match secret::load_password() {
        Ok(Some(p)) if !p.is_empty() => {
            println!("password: (from secure store)");
            have_stored = true;
            p
        }
        Ok(_) => {
            let generated: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(10)
                .map(char::from)
                .collect();
            println!("password: {generated}");
            println!(
                "(no stored password; set one from the tray icon or `aurora-host --set-password`)"
            );
            // GUI リリースでは stdout が無く上の println は破棄される。この乱数値は表示されず
            // 接続にも使えないため、ファイルログに「トレイで設定するまでの暫定値」である旨を残す
            // (実運用ではトレイの初回ダイアログ prompt_now でユーザーがパスワードを設定する)。
            tracing::warn!(
                "no stored connection password; set one from the tray icon. A random password is \
                 used as a placeholder until then (not shown in background/GUI mode)."
            );
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
    println!("TCP/UDP: {bind_ip}:{port}");

    // PSK は tray (UI スレッド) と net (接続受理) で共有する。tray でパスワードを変更すると
    // ここを書き換え、net が次の接続受理時に読むことで再起動なしに反映される。
    let psk = Arc::new(Mutex::new(psk_from_password(&password)));
    // 実パスワードが設定済みか (= 現在パスワードの照合を要求するか)。初回 (保存なし) は
    // ランダム値なので「未設定」扱いで照合を求めない。tray で変更すると true になる。
    let has_password = Arc::new(std::sync::atomic::AtomicBool::new(have_stored));
    // 待受ポートも tray と net で共有する。tray で変更すると port を書き換えて reload を
    // 通知し、net (accept ループ) が TCP/UDP を貼り直すことで再起動なしに反映される。
    let port_cell = Arc::new(std::sync::atomic::AtomicU16::new(port));
    let reload = Arc::new(tokio::sync::Notify::new());
    // システムトレイ常駐 UI を起動 (Windows のみ実体)。保存パスワードが無い初回は
    // 設定ダイアログを自動表示する。
    tray::spawn(
        psk.clone(),
        has_password,
        port_cell.clone(),
        reload.clone(),
        !have_stored,
    );

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
            bind_ip,
            port: port_cell,
            reload,
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
