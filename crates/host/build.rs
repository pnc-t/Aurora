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
//!
//! あわせて Windows では exe へ aurora アイコン (リソース ID = 1) を埋め込む。これが
//! エクスプローラ/タスクバーの exe アイコンになり、トレイ常駐 (`tray.rs`) も同じものを
//! `LoadIconW(hinstance, MAKEINTRESOURCE(1))` で読む。

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

    // exe へのアイコン埋め込み (Windows のみ)。失敗しても起動はするため warning に留める。
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if target_os == "windows" {
        let mut res = winresource::WindowsResource::new();
        // リソース ID を 1 に固定し、tray から MAKEINTRESOURCE(1) で確実に参照できるようにする。
        res.set_icon_with_id("assets/icon.ico", "1");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed icon: {e}");
        }
    }
}
