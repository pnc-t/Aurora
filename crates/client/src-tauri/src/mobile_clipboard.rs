//! Android: ホスト→端末のクリップボード書き込み。
//!
//! `navigator.clipboard.writeText` はユーザー操作 (transient activation) を伴わない呼び出しを
//! WebView が拒否する (`NotAllowedError: Write permission denied`)。ホストからの受信イベント
//! 経由の書き込みはユーザー操作を伴わないため、Web 実装では原理的に成功しない (実機
//! client.log で確定、2026-07-29)。次にここを触る人は Web 実装へ戻さないこと。代わりに
//! `MainActivity.setClipboardText` を JNI で直接呼び、Android の `ClipboardManager` へ書く。
//!
//! 端末→ホスト方向 (読み取り) は Android 10+ でフォアグラウンド制約がありスコープ外。
//! 書き込み専用。

use jni::JNIEnv;
use jni::objects::JValue;

use crate::android_jni::{clear_pending_exception, main_activity_class, with_env};

/// `MainActivity.setClipboardText(String): Boolean` を呼び、端末のクリップボードへ書き込む。
/// Android 側が false を返した場合や JNI 例外が起きた場合は Err (呼び出し元は 1 度だけログする)。
/// クラス参照は `android_jni::main_activity_class` のキャッシュ (`JNI_OnLoad` で解決済みの
/// `GlobalRef`) を使う。ここで改めて `find_class` しない (このスレッドではアプリのクラスローダが
/// 効かず `ClassNotFoundException` になるため。`android_jni.rs` 冒頭コメント参照)。
pub(crate) fn write(text: &str) -> Result<(), String> {
    let class = main_activity_class()?;
    with_env(|env| {
        let jtext = env
            .new_string(text)
            .map_err(|e| jni_err(env, "文字列生成", e))?;
        let ok = env
            .call_static_method(
                class,
                "setClipboardText",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&jtext)],
            )
            .and_then(|v| v.z())
            .map_err(|e| jni_err(env, "MainActivity.setClipboardText", e))?;
        if ok {
            Ok(())
        } else {
            Err("android clipboard: setClipboardText returned false".into())
        }
    })
}

fn jni_err(env: &mut JNIEnv, context: &str, e: jni::errors::Error) -> String {
    clear_pending_exception(env);
    // クリップボード本文は機密ではないが、値そのものはログに含めず事象のみ記す。
    format!("android clipboard: {context} failed: {e}")
}
