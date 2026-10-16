//! 対話セッションのログオンユーザー (= デスクトップの explorer の持ち主) として振る舞うための補助。
//!
//! SYSTEM サービス配下の本体は SYSTEM のまま動くため、そのままでは
//! - ファイル受信が SYSTEM のプロファイル (`C:\Windows\System32\config\systemprofile`) に落ち、
//! - ファイル送出 (クライアントからのダウンロード要求) が SYSTEM 権限で何でも読め、
//! - ProjFS の投影ルートが利用者から見えない場所に作られる。
//!
//! そこでファイルの開閉は**ユーザーのトークンを偽装したスレッド上で**行い、アクセス判定と
//! 所有者をユーザーに揃える (開いたハンドルはそのまま非偽装の I/O に使える)。サインイン前
//! (ユーザー不在) はこれらの機能を使わせない。
//!
//! Windows 固有の落とし穴:
//! - 偽装はスレッド単位。tokio の非同期 I/O は別スレッドで走るため、偽装は `spawn_blocking`
//!   の中で「開く」までに限定し、偽装を残したままスレッドを返さない。
//! - UAC の分割トークンでは `TokenLinkedToken` は SeTcb 権限が無いと識別レベルしか返らない
//!   ため、シェル (explorer) のトークンを複製して使う (サービス配下でも同じ経路で済む)。

use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY,
    TokenImpersonation,
};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, SetThreadToken,
};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

/// Drop で CloseHandle する偽装トークン。
pub struct UserToken(HANDLE);

impl Drop for UserToken {
    fn drop(&mut self) {
        // SAFETY: 取得に成功した有効なハンドルだけを保持し、1 度だけ閉じる。
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// プロセスハンドル (Drop で CloseHandle)。
struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: OpenProcess で得た有効なハンドルを 1 度だけ閉じる。
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// 非昇格シェル (デスクトップの explorer) のトークンを偽装用に複製する。シェルが居ない
/// (サインイン前・explorer 停止中) / 開けないときは理由を `Err` で返す (ログでの切り分け用)。
pub fn shell_impersonation_token() -> Result<UserToken, &'static str> {
    // SAFETY: 引数なしでシェルのデスクトップウィンドウを得るだけ (無ければ null)。
    let hwnd = unsafe { GetShellWindow() };
    if hwnd.0.is_null() {
        return Err("no shell window");
    }
    let mut pid = 0u32;
    // SAFETY: hwnd は直前に得たウィンドウ。pid は出力先のスタック変数。
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return Err("no shell process");
    }
    // SAFETY: explorer を問い合わせ権限だけで開く。
    let process = ProcessHandle(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|_| "OpenProcess(shell) failed")?,
    );
    let mut primary = HANDLE::default();
    // SAFETY: process は有効なプロセスハンドル。primary は出力先。
    unsafe { OpenProcessToken(process.0, TOKEN_DUPLICATE | TOKEN_QUERY, &mut primary) }
        .map_err(|_| "OpenProcessToken(shell) failed")?;
    let primary = UserToken(primary);
    let mut imp = HANDLE::default();
    // SAFETY: primary は有効なトークン。スレッド偽装に使える偽装レベルのトークンを複製する。
    unsafe {
        DuplicateTokenEx(
            primary.0,
            TOKEN_IMPERSONATE | TOKEN_QUERY,
            None,
            SecurityImpersonation,
            TokenImpersonation,
            &mut imp,
        )
    }
    .map_err(|_| "DuplicateTokenEx failed")?;
    Ok(UserToken(imp))
}

/// 現スレッドで `token` を偽装して `f` を実行し、必ず偽装を解除して戻る。
pub fn impersonating<T>(token: &UserToken, f: impl FnOnce() -> T) -> Result<T, &'static str> {
    // SAFETY: 複製した偽装トークンを現在のスレッドに設定する。f の後で必ず解除する
    // (このスレッドは tokio / ProjFS と共有されうるため、偽装を残さない)。
    unsafe { SetThreadToken(None, token.0) }.map_err(|_| "SetThreadToken failed")?;
    let out = f();
    // SAFETY: スレッドの偽装を解除するだけ。
    let _ = unsafe { SetThreadToken(None, HANDLE::default()) };
    Ok(out)
}

/// トークンの持ち主のプロファイルディレクトリ (`C:\Users\<名前>`)。
fn profile_dir(token: &UserToken) -> Option<PathBuf> {
    known_folder(token, &windows::Win32::UI::Shell::FOLDERID_Profile)
}

/// トークンの持ち主の `%LOCALAPPDATA%`。
fn local_appdata(token: &UserToken) -> Option<PathBuf> {
    known_folder(token, &windows::Win32::UI::Shell::FOLDERID_LocalAppData)
}

fn known_folder(token: &UserToken, id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    // SAFETY: token は有効なユーザートークン。成功時に返る文字列は CoTaskMemAlloc 領域なので、
    // 読んだ直後に CoTaskMemFree で解放する。
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, token.0).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const std::ffi::c_void));
        s.map(PathBuf::from)
    }
}

/// ファイル入出力をどのユーザーとして行うか。SYSTEM サービス配下ではログオンユーザーを偽装し、
/// それ以外 (開発実行・旧 ONLOGON 運用) は従来どおり自プロセスのまま (プロファイルは環境変数)。
///
/// `f` にはそのユーザーのプロファイルディレクトリを渡す。サービス配下でユーザーが居ないときは
/// `PermissionDenied` を返す (サインイン画面からはファイルを出し入れさせない)。
pub fn as_session_user<T>(
    f: impl FnOnce(&std::path::Path) -> std::io::Result<T>,
) -> std::io::Result<T> {
    if !crate::system::service::is_service_child() {
        let base = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        return f(&base);
    }
    let denied = |why: &str| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("no signed-in user to act as ({why})"),
        )
    };
    let token = shell_impersonation_token().map_err(denied)?;
    let profile = profile_dir(&token).ok_or_else(|| denied("profile not found"))?;
    impersonating(&token, || f(&profile)).map_err(denied)?
}

/// ProjFS の投影ルートを置くユーザーの `%LOCALAPPDATA%`。SYSTEM サービス配下ではログオン
/// ユーザーのもの (居なければ `None`)、それ以外は自プロセスの環境変数どおり。
pub fn session_user_local_appdata() -> Option<PathBuf> {
    if !crate::system::service::is_service_child() {
        return std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    }
    local_appdata(&shell_impersonation_token().ok()?)
}
