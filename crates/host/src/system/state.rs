//! host の状態ディレクトリ (`%ProgramData%\Aurora\host`) の解決と保護。
//!
//! SYSTEM サービス化で host は SYSTEM として動き、サインイン前にも接続パスワード・端末許可リスト
//! を読む必要がある。ユーザーの `%LOCALAPPDATA%` は使えないので PC 単位の場所に置く
//! (開発実行でも同じ場所 = 状態を分裂させない。代わりに管理者権限が要る)。
//!
//! Windows 固有の落とし穴:
//! - `%ProgramData%` 直下は**一般ユーザーもフォルダを作れて、既定 ACL で中身を読める**。そのまま
//!   使うと、端末許可リストの改ざん (= 任意端末の承認) やパスワード BLOB の読み出し (マシンスコープ
//!   DPAPI は同じ PC の誰でも復号できる) を許してしまう。使う前に保護 DACL (SYSTEM と
//!   Administrators のみ) を付け直す。
//! - インストール前に一般ユーザーが同名フォルダを先回りで作っておく攻撃 (所有者 = 攻撃者なので
//!   DACL を後から戻せる・ファイルを仕込める) があるため、所有者が SYSTEM / Administrators で
//!   なければ使わない (fail-closed)。
//! - 親の `%ProgramData%\Aurora` には ACL を付けない。VDD ドライバ (インストーラが `vdd\` に配置) は
//!   親からの継承 ACL で設定ファイルを読むため、親を保護すると仮想ディスプレイが壊れる。
//!   親は所有者だけ検査する (一般ユーザー所有の親なら子の `host\` をすり替えられるため)。
//! - ログ (`logs\`) だけはユーザーが「ログフォルダを開く」で読めるよう Users に読み取りを足す。
//!   ログに機密は出さない方針 (CLAUDE.md) なので、読めても実害は接続元 IP 程度。

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};

/// 状態ディレクトリ: SYSTEM と Administrators のみフルコントロール、継承を切る (P)。
const STATE_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// ログ: 上に加えて Users へ読み取り。
const LOGS_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FR;;;BU)";

/// 保護済みの状態ディレクトリを返す。成功するまでは呼ばれるたびに作成・検査・ACL 適用を試み、
/// 成功した結果だけをプロセス内で使い回す (起動直後の一過性の失敗をプロセス終了まで引きずらない)。
/// 管理者権限の無いプロセスでは ACL を適用できないので失敗し続ける (状態は SYSTEM/管理者専用)。
pub fn state_dir() -> Result<PathBuf> {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    cached_or(&DIR, prepare_state_dir)
}

/// 保護済みのログディレクトリ (`<state_dir>\logs`) を返す。キャッシュの方針は [`state_dir`] と同じ。
pub fn log_dir() -> Result<PathBuf> {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    cached_or(&DIR, || {
        let dir = state_dir()?.join("logs");
        prepare(&dir, LOGS_SDDL)?;
        Ok(dir)
    })
}

fn cached_or(
    cell: &OnceLock<PathBuf>,
    prepare: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    if let Some(dir) = cell.get() {
        return Ok(dir.clone());
    }
    let dir = prepare()?;
    Ok(cell.get_or_init(|| dir).clone())
}

/// ログディレクトリのパスだけを返す (作成・ACL 設定はしない)。保護済みフォルダを準備できない
/// 単独トレイが「ログフォルダを開く」で使う (準備は本体の役目)。
pub fn log_dir_path() -> Option<PathBuf> {
    Some(
        aurora_common::paths::machine_data_dir()?
            .join("host")
            .join("logs"),
    )
}

fn prepare_state_dir() -> Result<PathBuf> {
    let root = aurora_common::paths::machine_data_dir()
        .ok_or_else(|| anyhow!("ProgramData is not set"))?;
    ensure_trusted_dir(&root)?;
    let dir = root.join("host");
    prepare(&dir, STATE_SDDL)?;
    Ok(dir)
}

#[cfg(not(target_os = "windows"))]
fn ensure_trusted_dir(dir: &std::path::Path) -> Result<()> {
    use anyhow::Context;
    std::fs::create_dir_all(dir).context("create state dir")
}

#[cfg(not(target_os = "windows"))]
fn prepare(dir: &std::path::Path, _sddl: &str) -> Result<()> {
    ensure_trusted_dir(dir)
}

/// ディレクトリを (無ければ) 作り、所有者が SYSTEM / Administrators であることを確かめる。
#[cfg(target_os = "windows")]
fn ensure_trusted_dir(dir: &std::path::Path) -> Result<()> {
    use anyhow::{Context, bail};

    std::fs::create_dir_all(dir)
        .with_context(|| format!("create {} (run as administrator)", dir.display()))?;
    // 先回りで作られたジャンクション/シンボリックリンクは、所有者検査がリンク先 (SYSTEM 所有の
    // 別ディレクトリ等) を見て通ってしまい、ACL 適用がリンク先を書き換える。リンクは使わない。
    if is_reparse_point(dir)? {
        bail!(
            "{} is a junction or symbolic link (possibly planted by a non-admin user);              refusing to use it. Delete it as administrator and restart",
            dir.display()
        );
    }
    if !owner_is_trusted(dir)? {
        bail!(
            "{} is owned by an untrusted account (possibly pre-created by a non-admin user); \
             refusing to use it. Delete the folder as administrator and restart",
            dir.display()
        );
    }
    Ok(())
}

/// 所有者を確かめたうえで保護 DACL を付ける。
#[cfg(target_os = "windows")]
fn prepare(dir: &std::path::Path, sddl: &str) -> Result<()> {
    use anyhow::Context;

    ensure_trusted_dir(dir)?;
    apply_dacl(dir, sddl)
        .with_context(|| format!("protect {} (run as administrator)", dir.display()))
}

/// リパースポイント (ジャンクション・シンボリックリンク等) か。リンク自体の属性を見る (辿らない)。
#[cfg(target_os = "windows")]
fn is_reparse_point(path: &std::path::Path) -> Result<bool> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let meta =
        std::fs::symlink_metadata(path).map_err(|e| anyhow!("inspect {}: {e}", path.display()))?;
    Ok(meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// 所有者が SYSTEM か Administrators か。
#[cfg(target_os = "windows")]
fn owner_is_trusted(dir: &std::path::Path) -> Result<bool> {
    use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows::core::PCWSTR;

    let path = wide(dir);
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: path は NUL 終端。owner は sd の内部を指し、sd は成功時 LocalAlloc 領域で下で解放する。
    let err = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(path.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(anyhow!(
            "GetNamedSecurityInfoW({}) failed: {err:?}",
            dir.display()
        ));
    }
    // SAFETY: owner は sd が生きている間有効な SID。
    let trusted = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool()
    };
    // SAFETY: GetNamedSecurityInfoW が LocalAlloc した記述子を 1 度だけ解放する。
    let _ = unsafe { LocalFree(HLOCAL(sd.0)) };
    Ok(trusted)
}

/// SDDL の DACL を保護 (継承遮断) 付きでディレクトリへ設定する。子へは継承 ACE が伝播する。
#[cfg(target_os = "windows")]
fn apply_dacl(dir: &std::path::Path, sddl: &str) -> Result<()> {
    use windows::Win32::Foundation::{BOOL, ERROR_SUCCESS, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };
    use windows::core::{HSTRING, PCWSTR};

    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: SDDL は定数文字列。成功時 sd は LocalAlloc 領域で、下で必ず解放する。
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .map_err(|e| anyhow!("parse SDDL: {e}"))?;
    let mut present = BOOL(0);
    let mut defaulted = BOOL(0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: sd は直前に作った有効な記述子。dacl は sd 内部を指す (sd 解放まで有効)。
    let got = unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) };
    let path = wide(dir);
    let result = got.map_err(|e| anyhow!("read DACL: {e}")).and_then(|()| {
        // SAFETY: path は NUL 終端、dacl は sd 内部の有効な ACL。所有者/グループは変更しない。
        let err = unsafe {
            SetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                PSID::default(),
                PSID::default(),
                Some(dacl),
                None,
            )
        };
        if err == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(anyhow!("SetNamedSecurityInfoW failed: {err:?}"))
        }
    });
    // SAFETY: ConvertString... が LocalAlloc した記述子を 1 度だけ解放する。
    let _ = unsafe { LocalFree(HLOCAL(sd.0)) };
    result
}

#[cfg(target_os = "windows")]
fn wide(p: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 旧版の状態 (`%LOCALAPPDATA%\Aurora`、ユーザースコープ DPAPI) を状態ディレクトリへ移す
/// (`--migrate-user-state`)。旧パスワード BLOB はそのユーザーでしか復号できないため、インストーラが
/// **利用者のアカウントで (昇格して)** 呼ぶ。移行先に既にあるものは上書きしない (再実行しても安全)。
/// 旧ファイルは消さない (アンインストーラが従来どおり掃除する)。
pub fn migrate_user_state() -> Result<()> {
    use anyhow::Context;

    let Some(src) = aurora_common::paths::data_dir() else {
        println!("no user profile directory; nothing to migrate");
        return Ok(());
    };
    let dst = state_dir()?;
    let mut migrated = Vec::new();

    let old_secret = src.join(crate::security::secret::FILE_NAME);
    if !dst.join(crate::security::secret::FILE_NAME).exists()
        && let Some(blob) = read_migration_source(&old_secret)?
    {
        let plain = aurora_common::dpapi::unprotect(&blob)
            .context("decrypt old password store (run as the user who set it)")?;
        let password = String::from_utf8(plain).context("old password is not valid UTF-8")?;
        crate::security::save_password(&password)?;
        migrated.push(crate::security::secret::FILE_NAME);
    }
    // 平文設定は読み書きし直す (コピーだと元ファイルの ACL を持ち込みうる。新規作成なら
    // 保護済みディレクトリの継承 ACL になる)。
    for name in ["port", "encode-gpu", "pinning", "trusted_devices.txt"] {
        let to = dst.join(name);
        if !to.exists()
            && let Some(data) = read_migration_source(&src.join(name))?
        {
            std::fs::write(&to, data).with_context(|| format!("write {name}"))?;
            migrated.push(name);
        }
    }
    if migrated.is_empty() {
        println!("nothing to migrate");
    } else {
        println!("migrated to {}: {}", dst.display(), migrated.join(", "));
    }
    Ok(())
}

/// 移行元のファイルを読む。無ければ `None`。移行元は利用者が自由に書ける場所で、移行は昇格した
/// インストーラから走るため、リンク (別の場所のファイルを読ませる) や通常ファイル以外、上限を
/// 超える大きさのものは取り込まない (いずれも旧版が書いた形ではない)。
fn read_migration_source(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    /// 旧版の状態ファイルはいずれも数 KB 以下 (許可リストでも 1 端末 1 行)。
    const MAX_MIGRATION_FILE: u64 = 1024 * 1024;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("inspect {}: {e}", path.display())),
    };
    #[cfg(target_os = "windows")]
    let linked = is_reparse_point(path)?;
    #[cfg(not(target_os = "windows"))]
    let linked = meta.file_type().is_symlink();
    if linked || !meta.is_file() || meta.len() > MAX_MIGRATION_FILE {
        println!(
            "skipping {} (not a plain file written by an older version)",
            path.display()
        );
        return Ok(None);
    }
    std::fs::read(path)
        .map(Some)
        .map_err(|e| anyhow!("read {}: {e}", path.display()))
}
