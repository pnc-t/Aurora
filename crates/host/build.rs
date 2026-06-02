//! ホストのリンク設定。
//!
//! ProjFS (`projectedfslib.dll`) は Windows のオプション機能 "Projected File System" が
//! 有効なときだけ存在する。通常リンク (静的インポート) にすると DLL 欠如時に exe が
//! `main` 到達前に `0xC0000135 (STATUS_DLL_NOT_FOUND)` でロード失敗し、画面共有や入力
//! といったコア機能まで一切起動できなくなる。
//!
//! そこで MSVC の遅延ロード (`/DELAYLOAD`) を使い、`projectedfslib.dll` を「実際に
//! ProjFS を使う瞬間に初めて解決する」形にする。DLL が無い環境でもホストは起動でき、
//! ドライブリダイレクトだけが無効化される (`projfs::start` がプローブで Err を返す)。

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    // MSVC リンカでのみ /DELAYLOAD が使える。
    if target_os == "windows" && target_env == "msvc" {
        // 遅延ロード対象。最初の Prj* 呼び出しまで DLL を解決しない。
        println!("cargo:rustc-link-arg=/DELAYLOAD:projectedfslib.dll");
        // __delayLoadHelper2 を供給する遅延ロードヘルパ。
        println!("cargo:rustc-link-arg=delayimp.lib");
    }
}
