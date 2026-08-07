//! Android JNI 共有基盤: `JNI_OnLoad` はプロセスに 1 つしかエクスポートできないため、
//! Keystore 越しの秘密保存 (`secret::android`) とクリップボード書き込み
//! (`mobile_clipboard`) の双方が使う `JavaVM` 取得/attach をここへ集約する。
//!
//! `ndk_context` クレートは使わない: `android_context()` は未初期化だと `expect` で panic
//! する実装であり、Tauri v2 のスタック (tao/wry) はこれを初期化しない (`ndk-glue` 系のみが
//! 初期化するが Tauri は使っていない)。代わりにこの自前の `JNI_OnLoad` で `JavaVM` を受け取って
//! キャッシュする。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};

use jni::JNIEnv;
use jni::objects::GlobalRef;

/// `JNI_OnLoad` でキャッシュする `JavaVM`。Android では `System.loadLibrary` がこの
/// cdylib を読み込んだ時点で JVM から一度だけ呼ばれるため、以降はここから再取得する。
static JAVA_VM: AtomicPtr<jni::sys::JavaVM> = AtomicPtr::new(std::ptr::null_mut());

/// `MainActivity` クラス名。`JNI_OnLoad` でのキャッシュと `mobile_clipboard` の両方から
/// 参照するのでここに 1 箇所だけ置く。
const MAIN_ACTIVITY_CLASS_NAME: &str = "com/aurora/remote/MainActivity";

/// `MainActivity` クラスの `GlobalRef` キャッシュ。JNI の `FindClass` は「呼び出し元スレッドの
/// Java 呼び出しスタック」からクラスローダを決めるため、`with_env` で attach しただけの
/// ネイティブスレッド (tokio ワーカー等) から呼ぶとシステム (bootstrap) クラスローダが使われ、
/// アプリ独自クラスは `ClassNotFoundException` になる (実機 client.log で確認、2026-07-29)。
/// `JNI_OnLoad` は `System.loadLibrary` を呼んだ Java スレッド上で実行されるため、そこでだけ
/// 正しいアプリのクラスローダで `FindClass` でき、その結果を `GlobalRef` としてここに保持する
/// (ローカル参照は呼び出しを跨ぐと無効になるため必ず global ref にする)。
/// 今必要な `MainActivity` 1 クラスのみをキャッシュ対象とし、汎用クラスローダ機構は作らない。
static MAIN_ACTIVITY_CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// Android の JVM がこの cdylib をロードした際に一度だけ呼ぶエントリポイント。
/// `ndk_context` (Tauri のスタックでは初期化されない) の代わりに、ここでしか安全に
/// `JavaVM` を受け取れないため専用のエクスポート関数として実装する。プロセスに 1 つしか
/// エクスポートできないシンボルなので、呼び出し元 (secret::android / mobile_clipboard) は
/// 直接これを定義せず、必ずこのモジュール経由で `JavaVM` を取得すること。
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(
    vm: *mut jni::sys::JavaVM,
    _reserved: *mut std::ffi::c_void,
) -> jni::sys::jint {
    JAVA_VM.store(vm, Ordering::Release);
    cache_main_activity_class(vm);
    jni::sys::JNI_VERSION_1_6
}

/// `MainActivity` の `GlobalRef` を、正しいクラスローダが効く `JNI_OnLoad` の呼び出しスタック
/// 上で解決してキャッシュする (このモジュール冒頭の `MAIN_ACTIVITY_CLASS` 参照)。取得できなくても
/// panic せず諦める (`mobile_clipboard::write` 側が未キャッシュとしてエラーを返す)。
fn cache_main_activity_class(vm_ptr: *mut jni::sys::JavaVM) {
    // SAFETY: vm_ptr は直前に JVM から渡された JavaVM* で、この関数は JNI_OnLoad から
    // その場で一度だけ呼ばれるため、プロセス内で有効なポインタであることが保証されている。
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(vm_ptr) }) else {
        return;
    };
    // JNI_OnLoad は cdylib をロードした Java スレッド上で呼ばれ既に attach 済みなので、
    // (AttachGuard を static へ持ち越せない) attach_current_thread ではなく get_env を使う。
    let Ok(mut env) = vm.get_env() else {
        return;
    };
    let Ok(class) = env.find_class(MAIN_ACTIVITY_CLASS_NAME) else {
        clear_pending_exception(&mut env);
        return;
    };
    if let Ok(global) = env.new_global_ref(class) {
        // OnceLock なので JNI_OnLoad の 2 回目以降 (通常ありえない) は無視される。
        let _ = MAIN_ACTIVITY_CLASS.set(global);
    }
}

/// キャッシュ済みの `MainActivity` `GlobalRef` を返す。`JNI_OnLoad` が走っていない等で未取得
/// なら Err (呼び出し元がログして諦める。ここでは panic しない)。
pub(crate) fn main_activity_class() -> Result<&'static GlobalRef, String> {
    MAIN_ACTIVITY_CLASS
        .get()
        .ok_or_else(|| "MainActivity クラス参照が未キャッシュです (JNI_OnLoad 未完了)".into())
}

/// JNI 呼び出しが例外を投げた場合、pending exception を残すと以降の JNI 呼び出しが
/// 全て壊れるため、エラー化する前に必ずクリアする。
pub(crate) fn clear_pending_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

/// `JavaVM` を取得してカレントスレッドに attach し、`f` に `JNIEnv` を渡して実行する。
/// `AttachGuard` は `JavaVM` を借用するため、両者のライフタイムをこの関数内に閉じ込める。
pub(crate) fn with_env<T>(f: impl FnOnce(&mut JNIEnv) -> Result<T, String>) -> Result<T, String> {
    let vm_ptr = JAVA_VM.load(Ordering::Acquire);
    if vm_ptr.is_null() {
        return Err("JavaVM が未初期化です (JNI_OnLoad 未呼び出し)".into());
    }
    // SAFETY: vm_ptr は JNI_OnLoad で JVM から渡された JavaVM* をそのまま保持したものなので、
    // 現在のプロセスで有効なポインタであることが保証されている。
    let vm = unsafe { jni::JavaVM::from_raw(vm_ptr) }
        .map_err(|e| format!("JavaVM の取得に失敗: {e}"))?;
    let mut guard = vm
        .attach_current_thread()
        .map_err(|e| format!("スレッド attach に失敗: {e}"))?;
    f(&mut guard)
}
