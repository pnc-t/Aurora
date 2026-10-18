//! バイナリ内蔵インストーラ。Linux (systemd) 専用。
//!
//! raspi へ 1 ファイル持ち込んで `sudo ./aurora-wol install` するだけで常駐化する
//! (これまでは Windows 開発機から ssh 越しに PowerShell スクリプトで systemd 化していた)。

use std::fs;
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use aurora_common::noise::MIN_PASSWORD_LEN;

use crate::{DEFAULT_PORT, parse_tailscale_ip, tailscale_ip_cmd};

const BIN_PATH: &str = "/usr/local/bin/aurora-wol";
const CONF_DIR: &str = "/etc/aurora-wol";
const PASSWORD_PATH: &str = "/etc/aurora-wol/password";
const UNIT_PATH: &str = "/etc/systemd/system/aurora-wol.service";
const SERVICE: &str = "aurora-wol";
/// unit を動かす専用システムユーザー。root でネットワーク待受しないための隔離用。
const SERVICE_USER: &str = "aurora-wol";
/// systemd `StateDirectory=` が作る永続ディレクトリ (ログの HOME としても使う)。
const STATE_DIR: &str = "/var/lib/aurora-wol";

/// 自動生成パスワードの文字集合 (紛らわしい文字を除外)。既存デプロイスクリプトと同じ。
const PASSWORD_CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
const GENERATED_PASSWORD_LEN: usize = 24;

/// install/uninstall サブコマンドの入口。手書きの手動 argv パース (既存 `parse_args` と同じ流儀)。
pub(crate) fn run(subcmd: &str, args: impl Iterator<Item = String>) -> Result<()> {
    match subcmd {
        "install" => run_install(args),
        "uninstall" => run_uninstall(args),
        other => bail!("unknown subcommand: {other}"),
    }
}

fn run_install(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut port = DEFAULT_PORT;
    let mut password: Option<String> = None;
    let mut bind: Option<IpAddr> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                let v = args.next().context("--port requires a value")?;
                port = v.parse().context("--port expects an integer 1..=65535")?;
            }
            "--password" => {
                password = Some(args.next().context("--password requires a value")?);
            }
            "--bind" => {
                let v = args.next().context("--bind requires a value")?;
                bind = Some(v.parse().context("--bind expects an IP address")?);
            }
            other => bail!("unknown arg: {other}"),
        }
    }
    install(port, password, bind)
}

fn run_uninstall(args: impl Iterator<Item = String>) -> Result<()> {
    let mut purge = false;
    for a in args {
        match a.as_str() {
            "--purge" => purge = true,
            other => bail!("unknown arg: {other}"),
        }
    }
    uninstall(purge)
}

/// 新規設定/自動生成した場合のみ値を保持する (呼び出し元の完了表示で出し分けるため)。
enum PasswordOutcome {
    Kept,
    New(String),
}

fn install(port: u16, password_arg: Option<String>, bind: Option<IpAddr>) -> Result<()> {
    require_root()?;
    deploy_binary()?;
    ensure_service_user()?;
    let password = resolve_password(password_arg)?;
    // Kept (既存ファイル保持) の場合でも、旧版 (root 所有 0600) から新ユーザーへ所有を
    // 移行する必要があるため必ず呼ぶ。
    secure_password_file()?;

    let unit = render_unit(port, bind);
    fs::write(UNIT_PATH, unit).with_context(|| format!("failed to write {UNIT_PATH}"))?;

    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "--now", SERVICE])?;
    // enable --now は既に稼働中だと旧バイナリのまま走り続けるため、明示的に restart して
    // 今回配置したバイナリを確実に使わせる。
    run_systemctl(&["restart", SERVICE])?;

    let active = systemctl_stdout(&["is-active", SERVICE])?;
    if active != "active" {
        bail!(
            "aurora-wol サービスが起動していません (状態: {active})。`journalctl -u aurora-wol -e` でログを確認してください。"
        );
    }

    print_install_summary(port, password);
    Ok(())
}

fn uninstall(purge: bool) -> Result<()> {
    require_root()?;

    // 未インストールの再実行 (二重 uninstall) を許容するため、disable --now の失敗は無視する。
    let _ = Command::new("systemctl")
        .args(["disable", "--now", SERVICE])
        .output();

    if Path::new(UNIT_PATH).exists() {
        fs::remove_file(UNIT_PATH).with_context(|| format!("failed to remove {UNIT_PATH}"))?;
        run_systemctl(&["daemon-reload"])?;
    }
    if Path::new(BIN_PATH).exists() {
        fs::remove_file(BIN_PATH).with_context(|| format!("failed to remove {BIN_PATH}"))?;
    }

    if purge {
        if Path::new(CONF_DIR).exists() {
            fs::remove_dir_all(CONF_DIR).with_context(|| format!("failed to remove {CONF_DIR}"))?;
        }
        if Path::new(STATE_DIR).exists() {
            fs::remove_dir_all(STATE_DIR)
                .with_context(|| format!("failed to remove {STATE_DIR}"))?;
        }
        // ユーザーが残っても実害はなく、他プロセスが使用中で userdel が失敗しても
        // uninstall 自体は完了させたいのでベストエフォートにする。
        let _ = Command::new("userdel").arg(SERVICE_USER).output();
    } else {
        println!("パスワードファイルは残しています: {PASSWORD_PATH} (完全に消すには --purge)");
    }

    println!("アンインストール完了。");
    // unit は SERVICE_USER (HOME=/var/lib/aurora-wol) で動くため、稼働中のログはそこに残る。
    println!("ログは {STATE_DIR}/.local/share/Aurora/logs に残っています。");
    Ok(())
}

/// unit を動かす専用システムユーザーを作成する (既にあれば何もしない)。
fn ensure_service_user() -> Result<()> {
    let exists = Command::new("id")
        .args(["-u", SERVICE_USER])
        .output()
        .context("failed to run `id -u aurora-wol`")?
        .status
        .success();
    if exists {
        return Ok(());
    }

    let out = Command::new("useradd")
        .args([
            "--system",
            "--user-group",
            "--no-create-home",
            "--home-dir",
            STATE_DIR,
            "--shell",
            "/usr/sbin/nologin",
            SERVICE_USER,
        ])
        .output()
        .context("failed to run useradd")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("useradd failed: {stderr}");
    }
    println!("サービスユーザーを作成しました: {SERVICE_USER}");
    Ok(())
}

/// パスワードファイルの所有・権限を SERVICE_USER 用に正規化する。
///
/// サービスは非 root (SERVICE_USER) で走るため所有をそちらへ移す。0400 (所有者読取のみ) に
/// するのは、万一サービスが侵害されてもパスワードを書き換えられないようにするため
/// (更新は root で行う install コマンド経由のみとする)。
fn secure_password_file() -> Result<()> {
    let out = Command::new("chown")
        .arg(format!("{SERVICE_USER}:{SERVICE_USER}"))
        .arg(PASSWORD_PATH)
        .output()
        .with_context(|| format!("failed to chown {PASSWORD_PATH}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("chown {PASSWORD_PATH} failed: {stderr}");
    }

    let mut perms = fs::metadata(PASSWORD_PATH)
        .with_context(|| format!("failed to stat {PASSWORD_PATH}"))?
        .permissions();
    perms.set_mode(0o400);
    fs::set_permissions(PASSWORD_PATH, perms)
        .with_context(|| format!("failed to chmod {PASSWORD_PATH}"))?;
    Ok(())
}

fn require_root() -> Result<()> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("failed to run `id -u`")?;
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uid != "0" {
        bail!("root 権限が必要です (sudo で実行してください)");
    }
    Ok(())
}

/// 自身のバイナリを `/usr/local/bin/aurora-wol` へ配置する。既にそのパスから実行されて
/// いる場合 (インストール済みの状態からの再実行) は配置をスキップする。
fn deploy_binary() -> Result<()> {
    let current = std::env::current_exe().context("failed to resolve current exe path")?;
    let target = Path::new(BIN_PATH);
    let already_installed =
        target.exists() && fs::canonicalize(&current).ok() == fs::canonicalize(target).ok();
    if already_installed {
        println!("既にインストール済みのバイナリから実行されています。配置をスキップします。");
        return Ok(());
    }

    // 実行中のバイナリへの直接書込は ETXTBSY で失敗するため、同一 FS 上に書いて rename で
    // 原子的に差し替える。
    let tmp = Path::new("/usr/local/bin/.aurora-wol.tmp");
    fs::copy(&current, tmp)
        .with_context(|| format!("failed to copy binary to {}", tmp.display()))?;
    let mut perms = fs::metadata(tmp)
        .with_context(|| format!("failed to stat {}", tmp.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(tmp, perms)
        .with_context(|| format!("failed to chmod {}", tmp.display()))?;
    fs::rename(tmp, target).with_context(|| format!("failed to move binary to {BIN_PATH}"))?;
    println!("バイナリを配置しました: {BIN_PATH}");
    Ok(())
}

/// パスワードを解決する。優先順: `--password` 明示指定 (更新意図とみなし上書き) >
/// 既存ファイル保持 > 対話プロンプト (空なら自動生成)。
fn resolve_password(explicit: Option<String>) -> Result<PasswordOutcome> {
    if let Some(pw) = explicit {
        validate_password(&pw)?;
        write_password(&pw)?;
        return Ok(PasswordOutcome::New(pw));
    }

    if Path::new(PASSWORD_PATH).exists() {
        println!("既存パスワードを保持します: {PASSWORD_PATH}");
        return Ok(PasswordOutcome::Kept);
    }

    print!("aurora-wol の接続パスワードを入力してください (空 Enter で自動生成): ");
    io::stdout().flush().ok();
    let mut line = String::new();
    // EOF (非対話環境) は空入力と同じ扱い (自動生成) にする。
    let entered = match io::stdin().read_line(&mut line) {
        Ok(0) => String::new(),
        Ok(_) => line.trim().to_string(),
        Err(_) => String::new(),
    };

    let password = if entered.is_empty() {
        let generated = generate_password()?;
        println!();
        println!("自動生成したパスワード (client の WoL タブ登録に必要。控えてください):");
        println!("  {generated}");
        println!();
        generated
    } else {
        validate_password(&entered)?;
        entered
    };
    write_password(&password)?;
    Ok(PasswordOutcome::New(password))
}

/// host のパスワード設定時と同じハード拒否。
fn validate_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        bail!("password must be at least {MIN_PASSWORD_LEN} characters");
    }
    Ok(())
}

fn write_password(password: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::create_dir_all(CONF_DIR).with_context(|| format!("failed to create {CONF_DIR}"))?;
    // 「書いてから chmod」だと umask 既定 (0644) で作成された瞬間に他ユーザーから読める
    // 窓が生じるため、作成時点で mode を 0600 に絞って open する。
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(PASSWORD_PATH)
        .with_context(|| format!("failed to create {PASSWORD_PATH}"))?;
    file.write_all(password.as_bytes())
        .with_context(|| format!("failed to write {PASSWORD_PATH}"))?;
    Ok(())
}

/// `/dev/urandom` から [`GENERATED_PASSWORD_LEN`] 文字のパスワードを生成する。rand 依存を
/// 増やさないため直読みし、modulo バイアスはリジェクションサンプリングで回避する。
fn generate_password() -> Result<String> {
    let charset_len = PASSWORD_CHARS.len() as u32;
    let reject_threshold = 256 - (256 % charset_len);

    let mut urandom = fs::File::open("/dev/urandom").context("failed to open /dev/urandom")?;
    let mut password = String::with_capacity(GENERATED_PASSWORD_LEN);
    let mut byte = [0u8; 1];
    while password.chars().count() < GENERATED_PASSWORD_LEN {
        urandom
            .read_exact(&mut byte)
            .context("failed to read /dev/urandom")?;
        let b = u32::from(byte[0]);
        if b < reject_threshold {
            password.push(PASSWORD_CHARS[(b % charset_len) as usize] as char);
        }
    }
    Ok(password)
}

/// systemd unit ファイルの中身を生成する。`bind` 指定時のみ ExecStart に `--bind` を足す。
fn render_unit(port: u16, bind: Option<IpAddr>) -> String {
    let bind_arg = match bind {
        Some(ip) => format!(" --bind {ip}"),
        None => String::new(),
    };
    // 専用ユーザー + NoNewPrivileges/ProtectSystem 等で、untrusted なネットワーク入力を
    // 非 root で処理する (万一のバグでも被害を限定する)。
    // LoadCredential + %d specifier を使わないのは、bullseye 世代の systemd (v247) が
    // %d 未対応のため。代わりにパスワードファイルを SERVICE_USER 所有 0400 にして直接読ませる
    // (secure_password_file を参照)。
    // Environment=HOME を StateDirectory 配下の絶対パスで固定するのは、共通
    // aurora_common::logging (HOME 配下 ~/.local/share/Aurora/logs に書く) を無改修で
    // 使うため。ログは /var/lib/aurora-wol/.local/share/Aurora/logs に残る。
    format!(
        "[Unit]\n\
         Description=Aurora WoL relay\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=/usr/local/bin/aurora-wol --port {port} --password-file /etc/aurora-wol/password{bind_arg}\n\
         User=aurora-wol\n\
         Group=aurora-wol\n\
         NoNewPrivileges=true\n\
         ProtectSystem=full\n\
         ProtectHome=true\n\
         PrivateTmp=true\n\
         StateDirectory=aurora-wol\n\
         Environment=HOME=/var/lib/aurora-wol\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to run systemctl {args:?}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("systemctl {args:?} failed: {stderr}");
    }
    Ok(())
}

/// 非ゼロ終了でも出力を読みたい呼び出し (`is-active` 等) 用。ステータスは呼び出し側が見る。
fn systemctl_stdout(args: &[&str]) -> Result<String> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to run systemctl {args:?}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn resolve_tailscale_ip_sync() -> Option<IpAddr> {
    let out = tailscale_ip_cmd().output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    parse_tailscale_ip(&text)
}

fn print_install_summary(port: u16, password: PasswordOutcome) {
    let host = match resolve_tailscale_ip_sync() {
        Some(ip) => format!("{ip}:{port}"),
        None => format!("<この機器の Tailscale IP>:{port} (tailscale ip -4 で確認)"),
    };
    let password_display = match password {
        PasswordOutcome::Kept => "(既存のものを使用)".to_string(),
        PasswordOutcome::New(pw) => pw,
    };
    println!("インストール完了。client の WoL タブに以下を登録してください:");
    println!("  ホスト: {host}");
    println!("  パスワード: {password_display}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_unit_without_bind() {
        let unit = render_unit(7100, None);
        assert!(unit.contains(
            "ExecStart=/usr/local/bin/aurora-wol --port 7100 --password-file /etc/aurora-wol/password\n"
        ));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("User=aurora-wol"));
        assert!(unit.contains("NoNewPrivileges=true"));
        assert!(unit.contains("StateDirectory=aurora-wol"));
        assert!(!unit.contains("User=root"));
    }

    #[test]
    fn render_unit_with_bind() {
        let unit = render_unit(7100, Some("100.64.1.2".parse().unwrap()));
        assert!(unit.contains(
            "ExecStart=/usr/local/bin/aurora-wol --port 7100 --password-file /etc/aurora-wol/password --bind 100.64.1.2\n"
        ));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("User=aurora-wol"));
        assert!(unit.contains("NoNewPrivileges=true"));
        assert!(unit.contains("StateDirectory=aurora-wol"));
        assert!(!unit.contains("User=root"));
    }

    #[test]
    fn generated_password_has_expected_shape() {
        let pw = generate_password().unwrap();
        assert_eq!(pw.chars().count(), GENERATED_PASSWORD_LEN);
        assert!(pw.chars().all(|c| PASSWORD_CHARS.contains(&(c as u8))));
    }
}
