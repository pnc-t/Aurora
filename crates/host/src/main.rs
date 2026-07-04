//! Aurora ホスト (操作される側) のエントリポイント。
//!
//! リリースビルドの Windows では GUI サブシステムにしてコンソール窓を出さず、システム
//! トレイ常駐で動かす (出先 PC での無人/バックグラウンド運用が主用途)。CLI サブコマンド
//! (`--set-password` 等) を端末から実行したときだけ親コンソールへ接続して入出力する。
//! デバッグビルドはログ確認のためコンソールサブシステムのまま。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// 新たに追加する unsafe ブロックには直上に `// SAFETY:` を必須化する (規約の機械強制)。
#![warn(clippy::undocumented_unsafe_blocks)]

use anyhow::{Context, Result, anyhow};
use aurora_common::noise::psk_from_password;
use rand::Rng;
use rand::distributions::Alphanumeric;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

mod audio;
mod capture;
mod display;
mod encode;
mod input;
mod net;
mod perf;
mod projfs;
mod security;
mod system;
mod tray;

use audio::EncodedAudio;
use capture::{DisplayInfo, Frame, PlatformCapturer, ScreenCapturer};
use encode::{EncodedNal, MAX_BITRATE_BPS, PlatformEncoder, VideoEncoder};
use input::{DisplayGeom, InputInjector, PlatformInjector};
use net::{BindSpec, EncoderCommand, InputCmd, ServerConfig};

/// 既定の送出フレームレート上限 (PLAN.md の 60fps 目標)。
const DEFAULT_FPS_CAP: u32 = 60;
/// 既定の待受ポート (TCP 制御 / UDP 映像で共用)。
const DEFAULT_PORT: u16 = 7000;

#[derive(Debug)]
struct Args {
    dump_bmp: bool,
    /// 全 DXGI アダプタ × 出力を列挙して終了する (仮想ディスプレイの出現位置の診断)。
    list_displays: bool,
    /// 仮想ディスプレイ (MttVDD) の解像度モード一覧を補完して reload し終了する (手動補完)。
    vdd_enrich: bool,
    /// 仮想ディスプレイで解像度変更→復元を実測して終了する (復元バグの切り分け診断)。
    vdd_test_restore: bool,
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
    /// 待受アドレス (`--bind`)。未指定なら Tailscale IP を自動採用し、無ければ 127.0.0.1
    /// (loopback) に fail-closed する。本プロジェクトは Tailnet 前提のため、既定で tailnet
    /// インターフェースのみに公開して LAN/インターネットへの露出 (総当たり・接続枠 DoS) を
    /// 避ける。LAN テストや意図的な公開は `--bind <ip>` の明示指定 (例: LAN IP、全 NIC なら
    /// `--bind 0.0.0.0`) でオプトインする。
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
    /// エンコード GPU の名前部分一致指定 (`--encode-gpu`)。"auto" で保存を消し既定
    /// (キャプチャと同一 GPU) へ戻す。トレイ UI が無く CLI が唯一の入口のため明示時は永続化する。
    encode_gpu: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            dump_bmp: false,
            list_displays: false,
            vdd_enrich: false,
            vdd_test_restore: false,
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
            encode_gpu: None,
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
            "--list-displays" => args.list_displays = true,
            "--vdd-enrich" => args.vdd_enrich = true,
            "--vdd-test-restore" => args.vdd_test_restore = true,
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
            "--encode-gpu" => {
                args.encode_gpu = Some(take_value(&mut it, "--encode-gpu"));
            }
            other => {
                eprintln!("unknown arg: {other}");
            }
        }
    }
    args
}

fn main() -> Result<()> {
    // DPI 非対応プロセスでは GetSystemMetrics が拡大率で縮小した仮想値を返し、マウス絶対座標の
    // 正規化 (input/windows.rs) が物理ピクセルのキャプチャ座標と食い違ってカーソルがずれる。
    // 従来は HDR キャプチャ経路のみで設定されていたため、SDR ホストで顕在化した。起動最初期に
    // 全経路で必ず設定する (二重呼び出しは失敗するだけで無害)。
    #[cfg(target_os = "windows")]
    {
        // SAFETY: プロセス全体の DPI 設定を変えるだけで、メモリ安全性に影響しない。
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
    }

    let args = parse_args();

    // CLI サブコマンドは端末への入出力が要る。既定のサーバ起動はバックグラウンド
    // (トレイ常駐) で、リリースでは GUI サブシステムのためコンソールが無い。
    let needs_console = args.install
        || args.uninstall
        || args.set_password
        || args.clear_password
        || args.dump_bmp
        || args.list_displays
        || args.vdd_enrich
        || args.vdd_test_restore
        || args.dump_h264.is_some();

    // CLI のときだけ親プロセスのコンソールへ接続する (サーバ起動では開かない)。
    // set/clear-password は現在パスワードを対話入力させる。これらは専用コンソールを確保し
    // (親 cmd と入力を共有しない)、他のサブコマンドは親コンソールへ出力する。
    #[allow(unused_mut, unused_assignments)]
    let mut owns_console = false;
    #[cfg(windows)]
    if needs_console {
        owns_console = attach_console(args.set_password || args.clear_password);
    }

    // コンソールが無いバックグラウンドサーバ (リリース) はログをファイルへ。それ以外
    // (CLI / デバッグ) は従来どおりコンソールへ出す。_log_guard は非ブロッキング writer の
    // ワーカーを生かすため main 終了まで保持する (drop すると以降のログが失われる)。
    let _log_guard = init_tracing(
        !needs_console && !cfg!(debug_assertions),
        args.log_level.as_deref(),
    );

    // ログ冒頭にバージョン/ビルド種別を残す (どのビルドで起きた事象かを後から特定するため)。
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        debug = cfg!(debug_assertions),
        "aurora-host starting"
    );

    if args.install {
        // --install / --uninstall 自体を除いた起動引数を、登録するタスクへ転送する。
        return system::install(&system::current_forward_args());
    }
    if args.uninstall {
        return system::uninstall();
    }
    if args.set_password {
        return finish_console_command(set_password_interactive(), owns_console);
    }
    if args.clear_password {
        // set-password と対称に、接続中拒否 + 現在パスワード照合を要求してから削除する
        // (素通しだと接続中の操作者が認証を消して締め出し/乗っ取りできてしまう)。
        let result = (|| -> Result<()> {
            require_password_change_authorization()?;
            security::clear_password()?;
            println!("stored password cleared");
            Ok(())
        })();
        return finish_console_command(result, owns_console);
    }

    if args.list_displays {
        return list_displays_cmd();
    }
    if args.vdd_enrich {
        return vdd_enrich_cmd();
    }
    if args.vdd_test_restore {
        return vdd_test_restore_cmd();
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
        system::load_port().unwrap_or(args.port)
    };
    let bind = resolve_bind_spec(args.bind.as_deref());
    run_server(
        args.perf,
        args.fps_cap,
        args.max_bitrate_bps,
        port,
        bind,
        args.encode_gpu,
    )
}

/// 待受アドレスの解決方法 ([`BindSpec`]) を決める。
///
/// `--bind` 明示なら `Fixed`、未指定なら `Auto`。本プロジェクトは Tailnet 前提のため、既定
/// (`Auto`) では tailnet インターフェースのみに公開して LAN/インターネットへの露出を避ける。
///
/// **Tailscale IP の実解決は bind 時に行う** (`net::resolve_bind_ip`)。ホストは ONLOGON タスクで
/// `tailscaled` より先に起動しうるため、起動時点では IP が未確定なことがある。ここで一度きりに
/// 解決すると loopback へ落ちて以後固着するので、解決を bind ループへ遅延させ、そこで待つ。
/// Tailscale 未導入 (取得失敗) が続けば最終的に `127.0.0.1` に fail-close する (localhost の
/// 動作確認フローは保つ一方、LAN/インターネットへは到達不能)。LAN テストや意図的な公開は
/// `--bind <ip>` の明示オプトイン (`--bind 0.0.0.0` で全 NIC 公開も選べる)。
fn resolve_bind_spec(explicit: Option<&str>) -> BindSpec {
    match explicit {
        Some(s) => {
            let ip = s.parse().unwrap_or_else(|_| {
                eprintln!("error: --bind expects an IP address (got '{s}')");
                std::process::exit(2);
            });
            BindSpec::Fixed(ip)
        }
        None => BindSpec::Auto,
    }
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

/// `tracing` を初期化する。`to_file` が真なら `%LOCALAPPDATA%\Aurora\logs\host.<日付>.log`
/// へ日次ローテ (7 世代) + 非ブロッキングで書き、デバッグビルドや CLI 実行では加えて
/// コンソール (stdout) にも出す。ファイル出力先が用意できない場合はコンソールへフォール
/// バックする。フィルタの優先順位は `--log-level` 引数 > `RUST_LOG` 環境変数 > 既定
/// (`aurora=info`)。戻り値のガードは main 終了まで保持すること。
fn init_tracing(to_file: bool, level: Option<&str>) -> aurora_common::logging::LogGuard {
    aurora_common::logging::init(aurora_common::logging::LogOptions {
        file_stem: "host",
        filter: build_log_filter(level),
        dir: if to_file {
            aurora_common::logging::default_log_dir()
        } else {
            None
        },
        // ファイル出力する常駐サーバでは stdout は不要 (コンソール無し)。CLI / デバッグは
        // ファイル出力しない (= to_file=false) のでフォールバックで console に出る。
        console: false,
    })
}

/// CLI サブコマンドのためにコンソールを用意する。端末から起動された場合は親プロセスの
/// コンソールへ接続して入出力する。
///
/// `interactive` (= `--set-password`/`--clear-password`) のときは**専用コンソールを確保**する。
/// リリースは GUI サブシステムのため cmd から起動されても cmd はプロセスを待たずコンソール
/// 入力を握り続ける。親コンソールへ attach すると入力が cmd に奪われ `stdin` が即 EOF になり
/// 対話入力が読めない (入力文字が親シェルへ漏れる)。`AllocConsole` で自前のコンソールを持てば
/// 入力所有権を握れる。`interactive` でなければ (出力のみの `--install` 等) 親コンソールへ出す。
///
/// デバッグビルドは既にコンソールサブシステムで `stdin`/`stdout` が機能するため何もしない。
///
/// 戻り値は**自前の専用コンソールを割り当てたか** (`true` ならプロセス終了時に窓が即閉じるため、
/// 結果を見せるには終了前のポーズが要る — [`finish_console_command`] 参照)。
#[cfg(windows)]
fn attach_console(interactive: bool) -> bool {
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole, FreeConsole,
    };

    // コンソールサブシステムのデバッグビルドは標準入出力がそのまま使えるので触らない。
    if cfg!(debug_assertions) {
        return false;
    }

    // SAFETY: コンソールの確保/接続と標準ハンドルの結線のみ。失敗は無視する。
    unsafe {
        if interactive {
            // 親 cmd と入力を共有しないよう、既存接続を切ってから専用コンソールを割り当てる。
            let _ = FreeConsole();
            if AllocConsole().is_ok() {
                reopen_std_to_console();
                return true;
            }
            false
        } else {
            if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
                reopen_std_to_console();
            }
            false
        }
    }
}

/// 自前の専用コンソールを確保した対話コマンド (`--set-password` / `--clear-password`) の終了処理。
///
/// `owns_console` が真だと、プロセス終了時に確保したコンソール窓が即閉じて結果 (保存成功・弱パス
/// 警告・エラー) が読めない。失敗時はエラーを表示し、いずれも Enter 待ちでポーズしてから閉じる。
/// 既に表示済みのため `Ok` を返し、`anyhow` による二重表示を避ける。親コンソール接続時や
/// デバッグ (コンソールサブシステム) では `owns_console` が偽で、そのまま結果を返す。
fn finish_console_command(result: Result<()>, owns_console: bool) -> Result<()> {
    if !owns_console {
        return result;
    }
    use std::io::Write;
    if let Err(e) = &result {
        eprintln!("Error: {e}");
    }
    print!("\nEnter キーで閉じます...");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    Ok(())
}

/// 接続/割り当てたコンソールの読み書きハンドルを開き直し、標準入出力ハンドルに再設定する。
///
/// GUI サブシステム (リリース) では `AttachConsole` が `STD_INPUT_HANDLE` 等を繋ぎ直さない
/// ため、`std::io::stdin().read_line()` が即 EOF を返し対話入力 (`--set-password` /
/// `--clear-password`) が一切読めない (入力文字が親シェルへ漏れる)。`CONIN$`/`CONOUT$` を
/// 開いて `SetStdHandle` で差し替えることで標準入出力をコンソールへ正しく結線する。
#[cfg(windows)]
fn reopen_std_to_console() {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows::core::w;

    let access = (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0;
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE;

    // SAFETY: コンソールのデバイス名を読み書きで開き、標準ハンドルへ設定するだけ。失敗は無視。
    unsafe {
        if let Ok(input) = CreateFileW(
            w!("CONIN$"),
            access,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        ) {
            let _ = SetStdHandle(STD_INPUT_HANDLE, input);
        }
        if let Ok(output) = CreateFileW(
            w!("CONOUT$"),
            access,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        ) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, output);
            let _ = SetStdHandle(STD_ERROR_HANDLE, output);
        }
    }
}

/// 持続的なキャプチャ固着からの最終手段: 同じ実行ファイルを同じ引数で起動し直し、現プロセスを
/// 終了する。HDR ディスプレイのロック→アンロック遷移後などに DXGI Desktop Duplication が
/// ACCESS_LOST を返し続け、共有デバイスの作り直しもスレッドの入力デスクトップ貼り直しも効かず、
/// プロセスを生かしたままでは二度と復帰しないケースがある (実機確認済)。固着は DXGI/DWM への
/// プロセス単位の接続が壊れることに起因し、プロセス内のどの再生成 API でもリセットできない。
/// 新プロセスはこの接続を張り直せるためキャプチャが復帰する。client は自動再接続する。
///
/// 起動に成功したら現プロセスを `exit(0)` で終える (戻らない)。spawn 自体に失敗した場合のみ戻り、
/// 呼び出し側はキャプチャのリトライを続ける (動いている可能性を残すため自殺はしない)。
fn reexec_self(current_port: u16) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "re-exec: current_exe failed; continuing retries");
            return;
        }
    };
    // 自身の起動引数 (--bind / --log-level 等) を引き継ぐ。ただし --port は元の CLI 値を
    // そのまま再生すると、起動後にトレイで変更したポートへ巻き戻ってしまう (明示 --port は
    // 保存値より優先のため)。元の --port を除去し、現在の実効ポートに置き換えて渡す。
    let mut args: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--port" {
            let _ = it.next(); // 値も読み飛ばす
            continue;
        }
        args.push(a);
    }
    args.push("--port".into());
    args.push(current_port.to_string());
    tracing::warn!(
        ?exe,
        ?args,
        "capture stuck unrecoverably; re-exec host to recover"
    );
    match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(mut child) => {
            // spawn 成功 = 回復成功ではない。子が引数不正や初期化失敗で即死しているのに
            // exit(0) すると誰も残らない (完全停止)。少しだけ待って生存確認し、即死して
            // いたら旧プロセスを生かしてリトライを続ける。子は bind の AddrInUse を 5 秒
            // リトライして待つため、この待ちが子の bind を失敗させることはない。
            std::thread::sleep(std::time::Duration::from_millis(750));
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::error!(%status, "re-exec: child died immediately; keeping old process");
                }
                // 生存中 (None) / 確認失敗: 子に委ね、旧プロセスは速やかに終了してソケットを
                // 解放する (新プロセス側は bind をリトライして待つ)。
                _ => std::process::exit(0),
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "re-exec: spawn failed; continuing retries");
        }
    }
}

/// iGPU フォールバック適用後の子プロセスである印 (多重 re-exec 防止)。
#[cfg(windows)]
const IGPU_FALLBACK_ENV: &str = "AURORA_IGPU_FALLBACK";

/// ハイブリッド GPU (muxless Optimus) のノートでは内蔵パネルのデスクトップ合成を iGPU が担うため、
/// dGPU プロセスからは `DuplicateOutput` が全アダプタで UNSUPPORTED になり画面が出ない。Windows の
/// per-app GPU プリファレンス (`UserGpuPreferences` レジストリ) を **iGPU(省電力)** にして re-exec すると、
/// DXGI の出力列挙が iGPU 側へ反転し複製できる (実機確認済)。レジストリは永続化するので次回起動からは
/// 直接 iGPU で立ち上がり本フォールバックは発火しない。dGPU がデスクトップを所有する通常機 (デスクトップ
/// 等) はプローブが成功しここは何もしないため dGPU/NVENC のまま。env で 1 回限りに制限し、それでも
/// 直らなければ通常のリトライ経路に委ねる (無限 re-exec 防止)。
#[cfg(windows)]
fn maybe_reexec_for_igpu_capture(target_name: &str) {
    // 既にフォールバック適用後の子なら、再度の re-exec はしない。
    if std::env::var_os(IGPU_FALLBACK_ENV).is_some() {
        return;
    }
    // dGPU で複製可能 (通常機) / 対象出力なし → フォールバック不要。
    if !capture::capture_all_adapters_unsupported(target_name) {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "igpu fallback: current_exe failed; continuing on default GPU");
            return;
        }
    };
    let exe_str = exe.to_string_lossy().to_string();
    if let Err(e) = set_powersaving_gpu_preference(&exe_str) {
        tracing::error!(error = %e, "igpu fallback: failed to set GPU preference; continuing on default GPU");
        return;
    }
    tracing::warn!(
        exe = %exe_str,
        "hybrid GPU: desktop not duplicable on current GPU; set iGPU (power-saving) preference and re-exec"
    );
    // 起動引数はそのまま引き継ぐ (startup 時点なので tray のポート変更は未発生=書換え不要)。
    let args: Vec<String> = std::env::args().skip(1).collect();
    match std::process::Command::new(&exe)
        .args(&args)
        .env(IGPU_FALLBACK_ENV, "1")
        .spawn()
    {
        Ok(mut child) => {
            // spawn 成功 = 回復成功ではない。子が即死したら旧プロセスを残す (reexec_self と同じ流儀)。
            std::thread::sleep(std::time::Duration::from_millis(750));
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::error!(%status, "igpu fallback: child died immediately; keeping old process");
                }
                _ => std::process::exit(0),
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "igpu fallback: spawn failed; continuing on default GPU");
        }
    }
}

/// 自プロセス実行ファイルの GPU プリファレンスを iGPU(省電力) に設定する。
/// `HKCU\Software\Microsoft\DirectX\UserGpuPreferences` に `<exeパス> = "GpuPreference=1;"` を書く
/// (1=省電力/iGPU, 2=高パフォーマンス/dGPU)。Windows「設定→システム→ディスプレイ→グラフィック」と同実体。
#[cfg(windows)]
fn set_powersaving_gpu_preference(exe_path: &str) -> Result<()> {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegSetValueExW,
    };
    use windows::core::PCWSTR;

    let subkey: Vec<u16> = "Software\\Microsoft\\DirectX\\UserGpuPreferences"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = exe_path.encode_utf16().chain(std::iter::once(0)).collect();
    let value_data: Vec<u16> = "GpuPreference=1;"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: Reg* は標準 Win32。subkey/value_name は NUL 終端の有効な UTF-16、hkey は OUT。
    // value_data を NUL 終端込みのバイト列として REG_SZ に渡す。hkey は成否に関わらず close する。
    unsafe {
        let mut hkey = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .ok()
        .context("RegCreateKeyExW(UserGpuPreferences)")?;

        let bytes =
            std::slice::from_raw_parts(value_data.as_ptr() as *const u8, value_data.len() * 2);
        let set = RegSetValueExW(hkey, PCWSTR(value_name.as_ptr()), 0, REG_SZ, Some(bytes));
        let _ = RegCloseKey(hkey);
        set.ok().context("RegSetValueExW(GpuPreference)")?;
    }
    Ok(())
}

/// 全 DXGI アダプタ × 出力を列挙して表示する (`--list-displays`)。仮想ディスプレイ (VDD/IDD) が
/// どのアダプタ・output index に出るか、desktop に attach されるかを実機確認する診断コマンド。
/// VDD インストール前後で実行して差分を見る用途。
fn list_displays_cmd() -> Result<()> {
    use std::fmt::Write as _;
    // GUI サブシステムのリリースでは親コンソールへ attach するためファイルへのリダイレクトが
    // 効かない (CONOUT$ へ張り替わる)。診断を確実に読めるよう、stdout に加えて一時ファイルにも
    // 書き出す (`%TEMP%\aurora-list-displays.txt`)。
    let mut out = String::new();
    let _ = writeln!(out, "== DXGI adapters/outputs ==");
    #[cfg(target_os = "windows")]
    {
        for line in capture::debug_list_outputs()? {
            let _ = writeln!(out, "{line}");
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        for d in capture::enumerate_displays()? {
            let _ = writeln!(
                out,
                "output[{}] {}  {}x{} @ ({},{})",
                d.id, d.name, d.width, d.height, d.x, d.y
            );
        }
    }
    match display::detect() {
        Some(vdd) => {
            let _ = writeln!(out, "\nvirtual display detected: {} (MttVDD)", vdd.gdi_name);
        }
        None => {
            let _ = writeln!(out, "\nvirtual display: not detected");
        }
    }
    print!("{out}");
    let path = std::env::temp_dir().join("aurora-list-displays.txt");
    if std::fs::write(&path, &out).is_ok() {
        println!("(also written to {})", path.display());
    }
    Ok(())
}

/// 仮想ディスプレイ (MttVDD) の解像度モード一覧を補完して reload する (`--vdd-enrich`)。
/// run_server 起動時の自動補完と同じ処理を、サーバ本体を起動せず手動で実行・確認するための
/// 診断/メンテナンスコマンド。実行後の対応モードも表示する。
#[cfg_attr(not(target_os = "windows"), allow(clippy::unnecessary_wraps))]
fn vdd_enrich_cmd() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        if display::detect().is_none() {
            println!("no virtual display (MttVDD) detected; install/enable the VDD first");
            return Ok(());
        }
        match display::ensure_common_resolutions()? {
            true => {
                println!("added missing resolutions to vdd_settings.xml.");
                println!(
                    "NOTE: applying new modes needs a driver device-restart (admin). Click \
                     \"Restart Driver\" in the Virtual Driver Control app (run as admin), or run \
                     `pnputil /restart-device \"ROOT\\DISPLAY\\0000\"` from an elevated prompt. \
                     (RELOAD_DRIVER via pipe does NOT regenerate modes and drops the display.)"
                );
            }
            false => println!("resolution list already complete; nothing to add."),
        }
        println!("\n== current outputs/modes ==");
        for line in capture::debug_list_outputs()? {
            println!("{line}");
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        println!("virtual display enrichment is Windows-only");
    }
    Ok(())
}

/// 仮想ディスプレイで解像度変更→復元を実測する (`--vdd-test-restore`)。「切断後に解像度/fps が
/// 戻らない」原因を、安全な仮想ディスプレイ上で ChangeDisplaySettings の戻り値込みで切り分ける。
#[cfg_attr(not(target_os = "windows"), allow(clippy::unnecessary_wraps))]
fn vdd_test_restore_cmd() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let Some(vdd) = display::detect() else {
            println!("no virtual display (MttVDD) detected; enable the VDD first");
            return Ok(());
        };
        println!("testing display mode apply/restore on {}", vdd.gdi_name);
        for line in display::debug_test_cycle(&vdd.gdi_name) {
            println!("{line}");
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        println!("display mode test is Windows-only");
    }
    Ok(())
}

fn dump_one_bmp() -> Result<()> {
    let primary = capture::enumerate_displays()
        .ok()
        .and_then(|d| d.into_iter().next().map(|d| d.name))
        .unwrap_or_else(|| "\\\\.\\DISPLAY1".to_string());
    let device = capture::create_shared_device(&primary)?;
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
    let primary = capture::enumerate_displays()
        .ok()
        .and_then(|d| d.into_iter().next().map(|d| d.name))
        .unwrap_or_else(|| "\\\\.\\DISPLAY1".to_string());
    let device = capture::create_shared_device(&primary)?;
    let mut cap = PlatformCapturer::new_primary(device.clone())?;
    let mut enc = encode::create(device, MAX_BITRATE_BPS, None)?;
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
    if net::is_active() {
        return Err(anyhow!(
            "クライアント接続中はパスワードを変更できません。切断してから実行してください"
        ));
    }

    // 既存パスワードがあるなら、まず現在のパスワードを要求して本人確認する。
    if let Ok(Some(current)) = security::load_password() {
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

    // 弱いパスワード選択を避けてもらうため、強いパスフレーズ候補を先に提示する。
    println!(
        "Tip: 長いパスフレーズを推奨します (例: \"{}\")",
        suggest_passphrase()
    );
    print!("Enter new Aurora password: ");
    std::io::stdout().flush().ok();
    let pw1 = read_secret_line()?;
    print!("Confirm password: ");
    std::io::stdout().flush().ok();
    let pw2 = read_secret_line()?;

    if pw1 != pw2 {
        return Err(anyhow!("passwords do not match"));
    }
    if pw1.chars().count() < aurora_common::noise::MIN_PASSWORD_LEN {
        return Err(anyhow!(
            "password must be at least {} characters",
            aurora_common::noise::MIN_PASSWORD_LEN
        ));
    }
    // 最低長は満たすが弱い兆候があればソフト警告 (保存は続行する)。
    if let Some(reason) = aurora_common::noise::weak_password_reason(&pw1) {
        eprintln!("warning: {reason}。より強いパスワードを推奨します (このまま保存します)。");
    }
    security::save_password(&pw1)?;
    println!("password saved to secure store (DPAPI, current user)");
    Ok(())
}

/// 推奨パスフレーズ候補を生成する (語彙から 4 語をランダムにハイフン連結)。パスワード設定時の
/// ヒントとして提示し、利用者が弱いパスワードを選びにくくする。提案専用 (強制はしない)。
pub(crate) fn suggest_passphrase() -> String {
    use rand::seq::SliceRandom;
    /// 打ちやすい一般的な短い英単語。提案用の小さな語彙 (網羅性より打鍵しやすさ重視)。
    const WORDS: &[&str] = &[
        "amber", "anchor", "apple", "arrow", "autumn", "bamboo", "beacon", "birch", "bison",
        "bloom", "branch", "breeze", "bridge", "bright", "canyon", "cedar", "cherry", "cloud",
        "clover", "cobalt", "comet", "copper", "coral", "cotton", "crater", "crisp", "crystal",
        "dawn", "delta", "ember", "falcon", "feather", "fern", "flint", "forest", "garnet",
        "glacier", "granite", "harbor", "hazel", "heron", "indigo", "island", "ivory", "jade",
        "jasmine", "juniper", "kettle", "lagoon", "lantern", "ledge", "lemon", "lilac", "lotus",
        "lumen", "maple", "marble", "meadow", "mellow", "meteor", "mint", "moss", "nectar",
        "nimbus", "oak", "ocean", "olive", "onyx", "opal", "orchid", "otter", "pebble", "pepper",
        "pine", "plum", "pollen", "poppy", "prairie", "quartz", "quill", "raven", "ridge", "river",
        "rowan", "saffron", "sage", "salmon", "sapphire", "sierra", "silver", "spruce", "summit",
        "sunset", "tamarind", "thistle", "timber", "topaz", "tulip", "umber", "valley", "velvet",
        "violet", "walnut", "willow", "winter", "zephyr",
    ];
    let mut rng = rand::thread_rng();
    (0..4)
        .map(|_| *WORDS.choose(&mut rng).unwrap_or(&"aurora"))
        .collect::<Vec<_>>()
        .join("-")
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
    // SAFETY: handle は直上で取得した有効値。GetConsoleMode は OUT の orig に現在モードを書く。
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
    bind: BindSpec,
    encode_gpu_arg: Option<String>,
) -> Result<()> {
    // DPAPI で保存済みのパスワードがあればそれを使う (再起動しても固定・無人起動で読める)。
    // 無ければランダム生成して表示する (手動/開発・初回起動向け)。機密はログに出さない
    // 方針のため、保存済みを使う場合は値を表示しない。
    println!("== aurora-host ready ==");
    // 前回の異常終了で This PC に残った投影ドライブ (subst) があれば起動時に掃除する。
    projfs::cleanup_drives();
    // 前回のテイクオーバー (仮想ディスプレイ化で物理をデタッチ) がクラッシュ等で残っていたら、
    // ディスクに保存したトポロジから復元する (物理画面が暗いまま固着しないための安全網)。
    display::recover_takeover_if_any();
    // テイクオーバー中の panic で物理が暗転したまま死ぬのを防ぐ (穴①)。既定フックの前に、
    // 残骸トポロジがあれば復元する。recover_takeover_if_any は冪等で、テイクオーバー中は
    // begin_takeover がディスクに書いたトポロジが残っているので物理が再アタッチされる。
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // release(GUI) は stderr が無く既定フックの出力は捨てられるため、まずログファイルへ
            // 残す (フィールドのクラッシュを後から調べられるように)。
            tracing::error!(panic = %info, "host panicked");
            display::recover_takeover_if_any();
            default_hook(info);
        }));
    }
    // VDD がデスクトップにアタッチされていたら切り離す。
    // 未接続時にカーソルや Window が仮想ディスプレイへ迷い込む問題を防ぐ。
    // テイクオーバーが接続時に再アタッチするため、接続の妨げにはならない。
    if let Some(vdd) = display::detect() {
        display::detach_vdd_if_attached(&vdd.gdi_name);
    }
    // 保存パスワードの有無を覚えておく。無ければトレイ起動直後に設定ダイアログを促す。
    let mut have_stored = false;
    let password = match security::load_password() {
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
    println!("TCP/UDP: {bind} (port {port})");

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
    // 認証失敗の累計カウンタ。net (接続受理) が加算し、tray がメニューに表示することで、
    // オーナーが「総当たりに晒されているか」に気付けるようにする (#2 可視化)。
    let auth_failures = Arc::new(std::sync::atomic::AtomicU32::new(0));
    // デバイスピン留め (クライアント静的鍵の許可リスト) の実行時状態。既定無効。永続化済みの
    // 有効フラグ・許可リストを読み込む。net (照合) と tray (操作) で共有する。
    let devices = Arc::new(security::DeviceGuard::load());
    // net→tray の実行時ステータス/通知バス。net (接続受理) が接続/切断/登録を記録し、tray が
    // ポーリングしてツールチップ更新とバルーン通知へ反映する (接続状態の可視化)。
    let tray_bus = tray::TrayBus::new();
    // システムトレイ常駐 UI を起動 (Windows のみ実体)。保存パスワードが無い初回は
    // 設定ダイアログを自動表示する。
    tray::spawn(
        psk.clone(),
        has_password,
        port_cell.clone(),
        reload.clone(),
        auth_failures.clone(),
        devices.clone(),
        tray_bus.clone(),
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
    // 注: 仮想ディスプレイの解像度モード一覧の補完 (vdd_settings.xml への追記) は、反映に
    // ドライバのデバイス再起動 (= 管理者権限) が必要で、ユーザー権限のホスト実行時にはできない
    // (RELOAD_DRIVER パイプはモードを再生成せず仮想ディスプレイを落とすだけと実機確認)。よって
    // 解像度の充実はインストール時 (管理者) に行う方針とし、実行時は VDD が提供する既存モードから
    // ChangeDisplaySettingsEx で最近傍を選ぶ。手動補完は `--vdd-enrich` (XML 書込みのみ)。
    for d in &displays {
        tracing::info!(id = d.id, name = %d.name, w = d.width, h = d.height, x = d.x, y = d.y, "display");
    }
    // 初期キャプチャ対象 (先頭 = primary)。
    let initial_name = displays[0].name.clone();

    // ハイブリッド GPU (muxless Optimus) のノートで dGPU プロセスからは内蔵パネルを複製できない
    // (DuplicateOutput が全アダプタで UNSUPPORTED) 環境は、自プロセスの GPU プリファレンスを
    // iGPU(省電力) にして re-exec する。フォールバック発火時はここから戻らず子へ置き換わる。
    // dGPU がデスクトップを所有する通常機 (デスクトップ等) では何もしない (dGPU/NVENC のまま)。
    #[cfg(windows)]
    maybe_reexec_for_igpu_capture(&initial_name);

    // capture・convert・encode で共有する D3D11 デバイスを 1 つだけ生成する
    // (Windows のみ。他 OS は `()`)。ディスプレイ切替や MFT 共有のため持続させる。
    let device = capture::create_shared_device(&initial_name)?;

    // --encode-gpu の解決: CLI 明示 > 保存値 > 既定 (キャプチャ GPU)。"auto" は保存を消して既定へ。
    // 保存失敗は映像機能に影響しないため warn で続行する。
    let encode_gpu: Option<String> = match encode_gpu_arg.as_deref() {
        Some(v) if v.eq_ignore_ascii_case("auto") => {
            if let Err(e) = system::clear_encode_gpu() {
                tracing::warn!(error = %e, "failed to clear encode GPU setting");
            }
            None
        }
        Some(v) => {
            if let Err(e) = system::save_encode_gpu(v) {
                tracing::warn!(error = %e, "failed to save encode GPU setting");
            }
            Some(v.to_string())
        }
        None => system::load_encode_gpu(),
    };

    // VDD の描画 GPU をキャプチャ GPU に一致させる (クロス GPU keyed mutex 固着の恒久対策)。
    // 別 GPU が VDD を描画すると Windows がクロスアダプタ共有サーフェス (keyed mutex) で橋渡しし、
    // キャプチャ側 Desktop Duplication が ACCESS_LOST 恒常化で固まる (Optimus ノートで実機確認)。
    // friendlyname 変更の反映には**デバイス再起動が要る** (RELOAD_DRIVER pipe は loadSettings を
    // 再実行しないため無効)。startup 時点なら接続なし=安全。デバイス再起動は昇格が要るので
    // (autostart /RL HIGHEST なら成功)、非昇格時は再起動を試みず次回 reboot 反映に委ねる。
    #[cfg(target_os = "windows")]
    {
        let gpu = device.adapter_name.clone();
        if !gpu.is_empty() {
            match display::ensure_vdd_render_gpu(&gpu) {
                Ok(true) => {
                    // デバイス再起動は昇格が要る (非昇格では WMI/pnputil とも 0x80041001 等で失敗)。
                    // 非昇格の手動起動 (インストーラ完了時の起動など) では無駄に試して怖い WARN を
                    // 出さず、リブート反映に委ねる。autostart (/RL HIGHEST) は昇格するので即反映される。
                    if display::process_is_elevated() {
                        tracing::info!(gpu = %gpu, "VDD render GPU aligned to capture GPU; restarting VDD device to apply");
                        match display::restart_vdd_device() {
                            Ok(()) => tracing::info!("VDD device restarted"),
                            Err(e) => {
                                tracing::warn!(error = %e, "VDD device restart failed; change applies on next reboot or manual device restart")
                            }
                        }
                    } else {
                        tracing::info!(gpu = %gpu, "VDD render GPU aligned to capture GPU; device restart needs elevation (run via autostart), change applies on next reboot");
                    }
                }
                Ok(false) => {
                    tracing::debug!(gpu = %gpu, "VDD render GPU already matches capture GPU (or VDD not installed)")
                }
                Err(e) => tracing::warn!(error = %e, "could not align VDD render GPU"),
            }
        }
    }

    // フレームと一緒に capture 時刻 (Instant) を運び、encode 側で capture→encode 待ちを測る
    // (--perf 時のみ集計)。Instant は Copy で安価。
    let (frame_tx, frame_rx) = mpsc::channel::<(Instant, Frame)>(2);
    let (nal_tx, nal_rx) = mpsc::channel::<EncodedNal>(16);
    let (enc_cmd_tx, enc_cmd_rx) = mpsc::channel::<EncoderCommand>(8);
    let (input_tx, input_rx) = mpsc::channel::<InputCmd>(256);
    let (audio_tx, audio_rx) = mpsc::channel::<EncodedAudio>(64);
    // 制御スレッド → capture スレッドへ「キャプチャ対象ディスプレイ id」を渡す。
    let (cap_select_tx, cap_select_rx) = std::sync::mpsc::channel::<String>();
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
    // 実行時の解像度変更 (クライアントの SetResolution) を capture スレッドへ届けるチャネル
    // (送出する映像の最大高さ。0 = キャプチャ解像度のまま)。
    let (scale_tx, scale_rx) = std::sync::mpsc::channel::<u32>();
    // 映像へのカーソル合成 ON/OFF (クライアントのマウスキャプチャ中だけ) を capture スレッドへ。
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel::<bool>();
    let cap_device = device.clone();
    // re-exec 時に現在の実効ポート (トレイ変更反映済み) を引き継ぐため共有して渡す。
    let cap_port = port_cell.clone();
    // 接続/キーフレーム要求時に capture スレッドへ「現在の画面を 1 枚送れ」と促す共有フラグ。
    let force_frame = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cap_force_frame = force_frame.clone();
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(
                cap_device,
                frame_tx,
                res_tx,
                cap_select_rx,
                initial_name,
                fps_cap,
                fps_rx,
                scale_rx,
                cursor_rx,
                cap_port,
                cap_force_frame,
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
            if let Err(e) = encode_loop(
                device,
                frame_rx,
                nal_tx,
                enc_cmd_rx,
                perf,
                max_bitrate_bps,
                encode_gpu,
            ) {
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
            bind,
            port: port_cell,
            reload,
            psk,
            displays,
            audio: audio_enabled,
            max_bitrate_bps,
            auth_failures,
            devices,
            force_frame,
            tray_bus,
        };
        net::run_server(
            cfg,
            nal_rx,
            audio_rx,
            enc_cmd_tx,
            input_tx,
            cap_select_tx,
            fps_tx,
            scale_tx,
            cursor_tx,
            clip_apply_tx,
            clip_change_rx,
        )
        .await
    })
}

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    device: capture::SharedDevice,
    tx: mpsc::Sender<(Instant, Frame)>,
    res_tx: std::sync::mpsc::SyncSender<(u32, u32)>,
    select_rx: std::sync::mpsc::Receiver<String>,
    initial_name: String,
    fps_cap: u32,
    fps_rx: std::sync::mpsc::Receiver<u32>,
    scale_rx: std::sync::mpsc::Receiver<u32>,
    cursor_rx: std::sync::mpsc::Receiver<bool>,
    port: Arc<std::sync::atomic::AtomicU16>,
    force_frame: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    // capture スレッドで COM/MF を初期化する (Windows のみ実体あり)。
    #[cfg(target_os = "windows")]
    capture::device::init_thread_com()?;

    // 送出ペーシング: 高リフレッシュレートモニタだと next_frame が毎リフレッシュ
    // (例 144Hz) でフレームを返し、全部 encode/送信するとクライアントのデコードが
    // 追いつかずバックログが累積して遅延が増える。fps 上限で間引いて encode 側へ流す (0 = 無制限)。
    //
    // **グリッド基準ペーシング**: 送出可否を「前回送出 (実時刻) からの経過」ではなく、min_interval
    // 刻みの固定グリッド `next_send` で判定する。実時刻基準だと DXGI 到着ジッタがそのまま送出間隔に
    // 乗り、かつ間引きの量子化 (interval 直前のフレームを落とし次を送る) で送出間隔が interval〜
    // interval+リフレッシュ周期 で揺れる。これがクライアントの到着ジッタ (= ジッタバッファ肥大の主因)
    // になる。グリッド基準なら誤差が累積せず、送出間隔が均等化する (2026-06-27 調査)。
    let fps_to_interval = |fps: u32| {
        if fps == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(1.0 / fps as f64)
        }
    };
    let mut min_interval = fps_to_interval(fps_cap);
    // 次に送出してよいグリッド時刻。初回は即送出。
    let mut next_send = Instant::now();
    // 直近にキャプチャしたフレーム (NV12 テクスチャの COM 参照のみ保持・画素コピー無し)。
    // 画面静止中に新規接続/キーフレーム要求 (`force_frame`) が来たとき、これを再投入して
    // 現在の画面を即送る。DXGI Desktop Duplication は変化フレームしか返さないため、これが
    // 無いとホスト画面が動くまでクライアントが黒いままになる (実機: 2 台構成で再現)。
    let mut last_frame: Option<Frame> = None;

    // 持続的なキャプチャ固着がこの時間続いたら、プロセスを再起動して回復する (re-exec)。
    // 一過性の ACCESS_LOST (UAC・解像度変更・短時間ロック) は安価な再構築で数秒以内に戻るため、
    // それらを巻き込まないよう猶予を取る。実機で復帰不能と判明した HDR ロック遷移はこれを超える。
    const REEXEC_AFTER: Duration = Duration::from_secs(8);
    // テイクオーバー中の永続固着の救済猶予。テイクオーバー中は re-exec を抑止する (CCD 直後の
    // 一過性 ACCESS_LOST で点滅ループになるため) が、永続固着すると物理を巻き込んだ暗転が
    // 「きれいな切断 or ホスト再起動」まで治らない。この猶予を超えたら capture スレッド自身が
    // end_takeover で物理を復帰させる。一過性 settle (数秒) を巻き込まないよう REEXEC_AFTER より長く取る。
    const TAKEOVER_RESTORE_AFTER: Duration = Duration::from_secs(15);

    let mut cur_name = initial_name;
    // 初回の capturer 構築も失敗でスレッドを終わらせない。`?` で抜けると net 側の待受は
    // 続くのに映像が永久に出ない「ゾンビホスト」になる (特に re-exec 直後は DXGI が
    // 一時的に失敗しうる)。成功するまで 500ms 間隔でリトライする。
    let mut cap = loop {
        match PlatformCapturer::new_for_name(device.clone(), &cur_name) {
            Ok(c) => break c,
            Err(e) => {
                // `{:#}` で anyhow のコンテキスト連鎖 (下層の HRESULT 含む) を 1 行に出す。
                // 既定の `%e` は最上位コンテキスト ("DuplicateOutput") しか出さず根因が消える。
                tracing::warn!(error = format!("{e:#}"), name = %cur_name, "initial capturer create failed; retrying");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };
    let mut reported = false;
    // 一度でもフレームを取得できたか (= このプロセスでキャプチャが成立する環境か)。re-exec は
    // 「動いていたのに固着した」場合に限定し、起動時から取得できない環境での再起動ループを防ぐ。
    let mut captured_ok = false;
    // 連続失敗が始まった時刻。Ok で None に戻す。
    let mut first_fail: Option<Instant> = None;
    // 現在の送出最大高さ (0 = キャプチャ解像度)。capturer 再構築をまたいで保持し再適用する。
    let mut cur_target_h: u32 = 0;
    // カーソル合成の現在状態 (capturer 再構築をまたいで保持し再適用する)。
    let mut cur_cursor_overlay = false;
    loop {
        // 実行時の fps 上限変更 (クライアント SetVideoParams) を反映する。
        while let Ok(fps) = fps_rx.try_recv() {
            min_interval = fps_to_interval(fps);
            tracing::info!(fps, "fps cap updated");
        }
        // 実行時の解像度変更 (クライアント SetResolution) を反映する。
        while let Ok(h) = scale_rx.try_recv() {
            cur_target_h = h;
            tracing::info!(max_height = h, "target resolution updated");
        }
        // カーソル合成の ON/OFF (クライアントのマウスキャプチャ要求) を反映する。
        while let Ok(on) = cursor_rx.try_recv() {
            cur_cursor_overlay = on;
            tracing::info!(enabled = on, "cursor overlay updated");
        }
        // 毎反復で適用する (ディスプレイ切替や ACCESS_LOST で capturer を作り直しても維持)。
        cap.set_target_height(cur_target_h);
        cap.set_cursor_overlay(cur_cursor_overlay);
        // ディスプレイ切替要求を吸う。新規 capturer 構築に失敗したら現状維持。
        while let Ok(name) = select_rx.try_recv() {
            if name != cur_name {
                match PlatformCapturer::new_for_name(device.clone(), &name) {
                    Ok(c) => {
                        cap = c;
                        cur_name = name;
                        reported = false;
                        tracing::info!(name = %cur_name, "switched capture display");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, name = %name, "switch capture failed; keeping current");
                    }
                }
            }
        }
        // next_frame のエラーでスレッドを終わらせない。DXGI Desktop Duplication は
        // 画面ロック・UAC セキュアデスクトップ・解像度変更・フルスクリーン切替で
        // DXGI_ERROR_ACCESS_LOST を返すのが正常動作で、多くは capturer を作り直せば復帰する。
        // ここで `?` で抜けるとホスト再起動まで映像が永久に止まる (無人運用で致命的)。
        // 再構築に失敗する間 (ロック画面中など) は 500ms 間隔でリトライし続ける。
        //
        // ただし HDR ディスプレイのロック→アンロック遷移後などは、capturer を作り直しても
        // (新しい共有デバイスでも、スレッドの入力デスクトップを貼り直しても) AcquireNextFrame が
        // ACCESS_LOST を返し続け、プロセスを生かしたままでは永久に復帰しないケースがある
        // (実機確認済: 固着は DXGI/DWM へのプロセス単位の接続が壊れることに起因)。一度は取得
        // できていたのに REEXEC_AFTER を超えて固着が続いたら、プロセスを再起動して回復する。
        let acquired = match cap.next_frame(Duration::from_millis(100)) {
            Ok(f) => {
                first_fail = None;
                f
            }
            Err(e) => {
                // ロック画面・UAC セキュアデスクトップ中の ACCESS_LOST は、デスクトップが
                // 戻れば復帰する正常動作。固着時間に数えると 8 秒を超える通常のロックや
                // UAC 放置のたびに re-exec (= セッション切断) してしまうため、入力デスク
                // トップが Default のときだけ数える。HDR 固着はアンロック後 (Default 復帰後)
                // も ACCESS_LOST が続くので、この条件でも確実に検出できる。
                if !capture::input_desktop_is_default() {
                    first_fail = None;
                    tracing::debug!(error = %e, name = %cur_name, "capture failed on lock/secure desktop; not counted as stuck");
                } else {
                    let stuck_for = first_fail.get_or_insert_with(Instant::now).elapsed();
                    tracing::warn!(error = %e, name = %cur_name, stuck_ms = stuck_for.as_millis() as u64, "capture failed; rebuilding capturer");
                    // テイクオーバー外で対象ディスプレイ自体が消えている (例: 仮想セッション終了後の
                    // end_takeover で DISPLAY11 がデタッチ済み) なら、消えた名前を掴んだまま固着し
                    // ホストを丸ごと re-exec する無駄が起きる。利用可能な物理ディスプレイへ
                    // フォールバックして固着を解消する (テイクオーバー中は物理が意図的に消えている
                    // ので対象にしない)。
                    if !crate::display::is_takeover_active() {
                        if let Ok(list) = capture::enumerate_displays() {
                            if !list.is_empty() && !list.iter().any(|d| d.name == cur_name) {
                                if let Some(first) = list.first() {
                                    tracing::info!(old = %cur_name, new = %first.name, "capture target vanished; falling back to available display (avoids re-exec)");
                                    cur_name = first.name.clone();
                                    first_fail = None;
                                }
                            }
                        }
                    }
                    // テイクオーバー中の永続固着の救済 (暗転が治らない問題の本命対策)。re-exec は
                    // 下で抑止される / クライアント切断まで net 側の end_takeover も走らないため、
                    // このまま固着すると物理を含め全画面が永久に暗転する。settle を超えて固着し続けたら
                    // capture スレッド自身が end_takeover で物理を復帰させる。end_takeover が
                    // is_takeover_active() を false にするので、直下の通常 re-exec 経路がこのイテレーションで
                    // 有効になり、クリーンなプロセス作り直しまで一気に回復する (トポロジファイルは
                    // end_takeover が削除するので再起動後の recover は再テイクオーバーしない=点滅ループ防止)。
                    if captured_ok
                        && stuck_for >= TAKEOVER_RESTORE_AFTER
                        && crate::display::is_takeover_active()
                    {
                        tracing::error!(
                            stuck_ms = stuck_for.as_millis() as u64,
                            "takeover capture stuck; restoring physical displays (end_takeover) to clear blackout"
                        );
                        if let Err(e) = crate::display::end_takeover() {
                            tracing::warn!(error = %e, "watchdog end_takeover failed");
                        }
                    }
                    // テイクオーバー中は re-exec を抑止する。CCD のトポロジ変更直後は仮想の複製が
                    // 一時的に ACCESS_LOST になるが、再起動するとトポロジ復元→再接続→再テイクオーバーの
                    // 点滅ループになる。再構築を続けて落ち着くのを待つ (固着が一過性なら復帰する)。
                    if captured_ok
                        && stuck_for >= REEXEC_AFTER
                        && !crate::display::is_takeover_active()
                    {
                        // プロセス内のどの再生成でも復帰しないと判断。再起動で回復する (戻らない)。
                        // 固着原因は Windows/DXGI 固有だが、再起動による回復自体は OS 非依存で、
                        // captured_ok ガードにより再起動ループにもならないため共通コードに置く。
                        reexec_self(port.load(std::sync::atomic::Ordering::Relaxed));
                    }
                }
                // テイクオーバー中は、解像度変更後に Windows が仮想ディスプレイの GDI 名を振り直し、
                // cur_name が stale 化して find_output_by_name が永久に失敗→映像固着することがある
                // (リサイズで固まる症状の最有力仮説)。MTT1337 で現在の GDI 名を引き直し、変わって
                // いれば乗り換える。cur_name と再構築を所有する capture 層で直すのが確実。
                // テイクオーバー中の巻き戻し (物理再アタッチ) はループ先頭の能動チェックで再アサート
                // するのでここでは扱わない。ここでは解像度変更後に Windows が仮想ディスプレイの GDI 名を
                // 振り直し cur_name が stale 化する問題のみ救済する (MTT1337 で引き直し、変われば乗換)。
                if crate::display::is_takeover_active() {
                    if let Some(v) = crate::display::detect() {
                        if v.gdi_name != cur_name {
                            tracing::info!(old = %cur_name, new = %v.gdi_name, "takeover stall: virtual GDI name changed; retargeting capture");
                            cur_name = v.gdi_name;
                            first_fail = None;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
                match PlatformCapturer::new_for_name(device.clone(), &cur_name) {
                    Ok(c) => {
                        cap = c;
                        reported = false;
                        tracing::info!(name = %cur_name, "capturer rebuilt");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, name = %cur_name, "capturer rebuild failed; retrying");
                    }
                }
                continue;
            }
        };
        // 新規接続/キーフレーム要求の通知。新フレームがあればそれを (鮮度優先で) 強制送出し、
        // 画面静止で新フレームが無ければ直近フレームを再投入する。どちらも encode 側が
        // キーフレームを生成し (net が同時に RequestKeyframe を送る)、黒画面を解消する。
        let forced = force_frame.swap(false, std::sync::atomic::Ordering::AcqRel);
        if forced {
            tracing::debug!(
                static_screen = acquired.is_none(),
                has_last_frame = last_frame.is_some(),
                "force_frame: 強制送出 (静止時は直近フレーム再投入)"
            );
        }
        if let Some(f) = acquired {
            captured_ok = true;
            // (再)構築直後の最初のフレーム寸法を実解像度として通知する。
            if !reported {
                let _ = res_tx.try_send((f.width, f.height));
                reported = true;
            }
            // 直近フレームを更新 (COM 参照のみ・安価)。ペーシングで捨てられた場合も最新を保持。
            last_frame = Some(f.clone());
            // fps 上限ペーシング (グリッド基準): グリッド時刻 next_send に達したフレームを送る。
            // 0 (無制限) のとき・強制送出時は常に送る。
            let now = Instant::now();
            let due = min_interval.is_zero() || now >= next_send;
            if forced || due {
                if due && !min_interval.is_zero() {
                    // グリッドを 1 間隔進める。停止/ギャップで 1 間隔超 遅れていたら now へ再同期し、
                    // 復帰時に溜まったぶんを一気に送る (バースト) のを防ぐ。
                    next_send += min_interval;
                    if next_send <= now {
                        next_send = now + min_interval;
                    }
                }
                // try_send: バックプレッシャ。詰まったら捨てる。更新なしのときは None
                // `now` を capture 時刻として同送 (encode 側で cap→encode 待ちを測る)。
                let _ = tx.try_send((now, f));
            }
        } else if forced {
            // 画面静止 (DXGI タイムアウト) 中の強制送出: 直近フレームを再投入する。
            if let Some(lf) = last_frame.as_ref() {
                let _ = tx.try_send((Instant::now(), lf.clone()));
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
    mut frame_rx: mpsc::Receiver<(Instant, Frame)>,
    nal_tx: mpsc::Sender<EncodedNal>,
    mut cmd_rx: mpsc::Receiver<EncoderCommand>,
    perf: bool,
    max_bitrate_bps: u32,
    encode_gpu: Option<String>,
) -> Result<()> {
    let mut enc = encode::create(device, max_bitrate_bps, encode_gpu.as_deref())?;
    let mut meter = perf.then(perf::EncodeMeter::new);
    // 送信バックプレッシャを計測から除くため、NAL を一旦ここへ集めてから送る。
    let mut nals: Vec<EncodedNal> = Vec::new();
    // 受信済みコマンドを吸い、keyframe 要求が含まれていたら true を返す。RequestKeyframe は
    // 次の push_frame に効くフラグなので、push の直前に必ず適用する (先頭ドレインだけだと
    // 強制送出フレームが P フレームになり、参照を持たない新規クライアントが黒画面のまま残る)。
    let drain_cmds =
        |enc: &mut PlatformEncoder, cmd_rx: &mut mpsc::Receiver<EncoderCommand>| -> bool {
            let mut keyframe_requested = false;
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    EncoderCommand::RequestKeyframe => {
                        enc.request_keyframe();
                        keyframe_requested = true;
                    }
                    EncoderCommand::SetBitrate(bps) => {
                        if let Err(e) = enc.set_bitrate(bps) {
                            tracing::warn!(error = %e, bps, "set_bitrate failed");
                        }
                    }
                }
            }
            keyframe_requested
        };

    // 直近 push にキーフレーム要求が乗ったか。入力が尽きたあとのフラッシュ判断に使う。
    // 外側ループをまたいで保持する: blocking_recv で待つ前 (先頭ドレイン) に届いた要求も
    // 取りこぼさず、後続フレームを待たずにフラッシュへ繋ぐため (要求は別チャネルで来るので
    // 強制送出フレームと到着順がずれうる)。フラッシュ実施時にクリアする。
    let mut pending_keyframe = false;
    loop {
        pending_keyframe |= drain_cmds(&mut enc, &mut cmd_rx);
        // 最初の 1 枚はブロッキングで待つ。
        let mut frame = match frame_rx.blocking_recv() {
            Some(f) => f,
            None => return Ok(()),
        };
        // この外側反復で keyframe を実際に送出できたか。push 経路で出れば flush は不要。
        let mut keyframe_sent = false;
        loop {
            pending_keyframe |= drain_cmds(&mut enc, &mut cmd_rx);
            let t0 = std::time::Instant::now();
            // capture→encode 待ち: フレームが capture チャネルで待たされた時間 (push 直前に測る)。
            let cap_wait_us = frame.0.elapsed().as_micros().min(u32::MAX as u128) as u32;
            if let Err(e) = enc.push_frame(&frame.1) {
                tracing::warn!(error = %e, "push_frame failed");
            } else {
                nals.clear();
                while let Some(nal) = enc.poll_nal()? {
                    nals.push(nal);
                }
                if let Some(m) = meter.as_mut() {
                    let encode_us = t0.elapsed().as_micros() as u32;
                    let nal_bytes: usize = nals.iter().map(|n| n.data.len()).sum();
                    m.record(encode_us, nal_bytes, cap_wait_us);
                    m.maybe_flush();
                }
                if nals.iter().any(|n| n.keyframe) {
                    keyframe_sent = true;
                }
                for nal in nals.drain(..) {
                    if nal_tx.blocking_send(nal).is_err() {
                        return Ok(());
                    }
                }
            }
            // 入力キューに続きがあれば再ブロックせず連続処理し、パイプライン並列性を保つ。
            match frame_rx.try_recv() {
                Ok(next) => frame = next,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        // 入力アイドル (try_recv Empty で inner loop を抜けた) 時の低遅延フラッシュ。非同期 HW MFT は
        // frame N の HaveOutput を次の submit まで握るため、ドラッグが断続的で入力が疎になると直前
        // フレームの出力がエンコーダ内に滞留し、配信が遅れ・バースト化する (クライアントの到着ジッタ増
        // → ジッタバッファ肥大の一因。2026-06-27 実測で host fps/encode の乱高下として観測)。後続入力を
        // 待たず flush_pending で押し出し滞留分を即送出する。keyframe 要求時は下の専用フラッシュ (埋め草
        // re-push) が担うので委ねる。高 fps では inner loop が try_recv で回り続けここに来ないため
        // オーバーヘッドは無い (滞留が無ければ flush_pending は即返る)。
        if !pending_keyframe {
            if let Err(e) = enc.flush_pending() {
                tracing::warn!(error = %e, "idle flush_pending failed");
            }
            while let Some(nal) = enc.poll_nal()? {
                if nal_tx.blocking_send(nal).is_err() {
                    return Ok(());
                }
            }
        }
        // 入力が尽きたが keyframe 要求が乗っていて、まだ keyframe を出せていない。非同期 HW MFT は
        // frame N の出力に後続入力を要し (パイプライン段数は個体差あり)、静止画面では後続フレームが
        // 無いため IDR がエンコーダ内に滞留し、ホスト画面が動く (操作する) まで黒画面が残る。直近
        // フレームを非キーフレームで「埋め草入力」として繰り返し投入し、滞留した IDR が実際に出て
        // くるまで押し出す (段数不明なので上限付きで自己調整・出たら即やめる)。同期 SW エンコーダの
        // 1 フレーム遅延もこの再投入で解消され、0 遅延エンコーダは push 経路で既に出ているので入らない。
        const MAX_FLUSH_PUSHES: u32 = 8;
        if pending_keyframe {
            pending_keyframe = false;
            let mut pushes = 0u32;
            while !keyframe_sent && pushes < MAX_FLUSH_PUSHES {
                pushes += 1;
                if let Err(e) = enc.push_frame(&frame.1) {
                    tracing::warn!(error = %e, "keyframe flush re-push failed");
                    break;
                }
                if let Err(e) = enc.flush_pending() {
                    tracing::warn!(error = %e, "encoder flush_pending failed");
                }
                while let Some(nal) = enc.poll_nal()? {
                    keyframe_sent |= nal.keyframe;
                    if nal_tx.blocking_send(nal).is_err() {
                        return Ok(());
                    }
                }
            }
            tracing::debug!(
                pushes,
                keyframe_sent,
                "keyframe flush: 埋め草入力で滞留 IDR を押し出し"
            );
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
