//! Android Adapter: Android Keystore (AES/GCM) で秘密を端末に紐づけて暗号化する。
//!
//! 落とし穴:
//! (a) Keystore の鍵は端末のロック解除方式変更・アプリ再インストール等で失効しうる。
//!     その場合も呼び出し元 (`load_map`) は「復号できなければ空マップ」で継続する契約なので、
//!     接続フローは止まらず単にパスワード再入力になるだけで済む。
//! (b) JNI 呼び出しは対象スレッドの `JavaVM` への attach が必須。Tauri の非同期タスクは
//!     Java スレッドではないため、呼び出しの都度 `attach_current_thread` する。
//! (c) `setUserAuthenticationRequired` は呼ばない: デスクトップ側 DPAPI と同じく無操作で
//!     復号できる設計とするため (指紋/PIN 認証を毎回要求しない)。
//! (d) `JavaVM` の取得・attach は `crate::android_jni` に集約されている。`JNI_OnLoad` は
//!     プロセスに 1 つしかエクスポートできないため、ここでは定義せず共有モジュールへ委譲する
//!     (`mobile_clipboard` も同じ経路を使う)。

use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JObjectArray, JValue};

use crate::android_jni::{clear_pending_exception, with_env};

const ALIAS: &str = "aurora_secret";
/// 保存フォーマットのバージョン。`[version][iv_len][iv][ciphertext]`。
const FORMAT_VERSION: u8 = 1;

pub(super) fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    encrypt(plain)
}

pub(super) fn unprotect(enc: &[u8]) -> Option<Vec<u8>> {
    decrypt(enc).ok()
}

fn jni_err(env: &mut JNIEnv, context: &str, e: jni::errors::Error) -> String {
    clear_pending_exception(env);
    // 値そのもの (鍵/パスワード) は含めず事象のみ記す。
    format!("android keystore: {context} failed: {e}")
}

/// Keystore に `ALIAS` の鍵があればそれを、無ければ生成してから返す。
fn get_or_create_key<'a>(env: &mut JNIEnv<'a>) -> Result<JObject<'a>, String> {
    let ks_class = env
        .find_class("java/security/KeyStore")
        .map_err(|e| jni_err(env, "KeyStore クラス解決", e))?;
    let provider = env
        .new_string("AndroidKeyStore")
        .map_err(|e| jni_err(env, "文字列生成", e))?;
    let ks = env
        .call_static_method(
            ks_class,
            "getInstance",
            "(Ljava/lang/String;)Ljava/security/KeyStore;",
            &[JValue::Object(&provider)],
        )
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "KeyStore.getInstance", e))?;
    env.call_method(
        &ks,
        "load",
        "(Ljava/security/KeyStore$LoadStoreParameter;)V",
        &[JValue::Object(&JObject::null())],
    )
    .and_then(|v| v.v())
    .map_err(|e| jni_err(env, "KeyStore.load", e))?;

    let alias = env
        .new_string(ALIAS)
        .map_err(|e| jni_err(env, "文字列生成", e))?;
    let exists = env
        .call_method(
            &ks,
            "containsAlias",
            "(Ljava/lang/String;)Z",
            &[JValue::Object(&alias)],
        )
        .and_then(|v| v.z())
        .map_err(|e| jni_err(env, "KeyStore.containsAlias", e))?;

    if !exists {
        generate_key(env)?;
    }

    let key = env
        .call_method(
            &ks,
            "getKey",
            "(Ljava/lang/String;[C)Ljava/security/Key;",
            &[JValue::Object(&alias), JValue::Object(&JObject::null())],
        )
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "KeyStore.getKey", e))?;
    Ok(key)
}

/// `ALIAS` の AES-256/GCM 鍵を Android Keystore 内に新規生成する。
fn generate_key(env: &mut JNIEnv) -> Result<(), String> {
    let kg_class = env
        .find_class("javax/crypto/KeyGenerator")
        .map_err(|e| jni_err(env, "KeyGenerator クラス解決", e))?;
    let algo = env
        .new_string("AES")
        .map_err(|e| jni_err(env, "文字列生成", e))?;
    let provider = env
        .new_string("AndroidKeyStore")
        .map_err(|e| jni_err(env, "文字列生成", e))?;
    let kg = env
        .call_static_method(
            kg_class,
            "getInstance",
            "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/KeyGenerator;",
            &[JValue::Object(&algo), JValue::Object(&provider)],
        )
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "KeyGenerator.getInstance", e))?;

    let builder_class = env
        .find_class("android/security/keystore/KeyGenParameterSpec$Builder")
        .map_err(|e| jni_err(env, "KeyGenParameterSpec.Builder クラス解決", e))?;
    let alias = env
        .new_string(ALIAS)
        .map_err(|e| jni_err(env, "文字列生成", e))?;
    // PURPOSE_ENCRYPT (1) | PURPOSE_DECRYPT (2) = 3
    let builder = env
        .new_object(
            builder_class,
            "(Ljava/lang/String;I)V",
            &[JValue::Object(&alias), JValue::Int(3)],
        )
        .map_err(|e| jni_err(env, "KeyGenParameterSpec.Builder new", e))?;

    let block_modes = string_array(env, &["GCM"])?;
    let builder = env
        .call_method(
            &builder,
            "setBlockModes",
            "([Ljava/lang/String;)Landroid/security/keystore/KeyGenParameterSpec$Builder;",
            &[JValue::Object(&block_modes)],
        )
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "Builder.setBlockModes", e))?;

    let paddings = string_array(env, &["NoPadding"])?;
    let builder = env
        .call_method(
            &builder,
            "setEncryptionPaddings",
            "([Ljava/lang/String;)Landroid/security/keystore/KeyGenParameterSpec$Builder;",
            &[JValue::Object(&paddings)],
        )
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "Builder.setEncryptionPaddings", e))?;

    let builder = env
        .call_method(
            &builder,
            "setKeySize",
            "(I)Landroid/security/keystore/KeyGenParameterSpec$Builder;",
            &[JValue::Int(256)],
        )
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "Builder.setKeySize", e))?;

    // 生体/PIN 認証は要求しない (無操作復号にするため、意図的に setUserAuthenticationRequired
    // を呼ばない)。
    let spec = env
        .call_method(
            &builder,
            "build",
            "()Landroid/security/keystore/KeyGenParameterSpec;",
            &[],
        )
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "Builder.build", e))?;

    env.call_method(
        &kg,
        "init",
        "(Ljava/security/spec/AlgorithmParameterSpec;)V",
        &[JValue::Object(&spec)],
    )
    .and_then(|v| v.v())
    .map_err(|e| jni_err(env, "KeyGenerator.init", e))?;

    env.call_method(&kg, "generateKey", "()Ljavax/crypto/SecretKey;", &[])
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "KeyGenerator.generateKey", e))?;
    Ok(())
}

fn string_array<'a>(env: &mut JNIEnv<'a>, values: &[&str]) -> Result<JObjectArray<'a>, String> {
    let string_class = env
        .find_class("java/lang/String")
        .map_err(|e| jni_err(env, "String クラス解決", e))?;
    let array = env
        .new_object_array(values.len() as i32, string_class, JObject::null())
        .map_err(|e| jni_err(env, "String[] 生成", e))?;
    for (i, v) in values.iter().enumerate() {
        let s = env
            .new_string(*v)
            .map_err(|e| jni_err(env, "文字列生成", e))?;
        env.set_object_array_element(&array, i as i32, &s)
            .map_err(|e| jni_err(env, "String[] 要素設定", e))?;
    }
    Ok(array)
}

fn cipher_instance<'a>(env: &mut JNIEnv<'a>) -> Result<JObject<'a>, String> {
    let cipher_class = env
        .find_class("javax/crypto/Cipher")
        .map_err(|e| jni_err(env, "Cipher クラス解決", e))?;
    let transform = env
        .new_string("AES/GCM/NoPadding")
        .map_err(|e| jni_err(env, "文字列生成", e))?;
    env.call_static_method(
        cipher_class,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Cipher;",
        &[JValue::Object(&transform)],
    )
    .and_then(|v| v.l())
    .map_err(|e| jni_err(env, "Cipher.getInstance", e))
}

fn encrypt(plain: &[u8]) -> Result<Vec<u8>, String> {
    with_env(|env| encrypt_with(env, plain))
}

fn encrypt_with(env: &mut JNIEnv, plain: &[u8]) -> Result<Vec<u8>, String> {
    let key = get_or_create_key(env)?;
    let cipher = cipher_instance(env)?;
    // Cipher.ENCRYPT_MODE = 1
    env.call_method(
        &cipher,
        "init",
        "(ILjava/security/Key;)V",
        &[JValue::Int(1), JValue::Object(&key)],
    )
    .and_then(|v| v.v())
    .map_err(|e| jni_err(env, "Cipher.init(encrypt)", e))?;

    let iv_obj = env
        .call_method(&cipher, "getIV", "()[B", &[])
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "Cipher.getIV", e))?;
    let iv = env
        .convert_byte_array(JByteArray::from(iv_obj))
        .map_err(|e| jni_err(env, "IV 取得", e))?;
    if iv.len() > u8::MAX as usize {
        return Err("android keystore: IV 長が不正です".into());
    }

    let input = env
        .byte_array_from_slice(plain)
        .map_err(|e| jni_err(env, "平文バッファ生成", e))?;
    let ct_obj = env
        .call_method(&cipher, "doFinal", "([B)[B", &[JValue::Object(&input)])
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "Cipher.doFinal(encrypt)", e))?;
    let ct = env
        .convert_byte_array(JByteArray::from(ct_obj))
        .map_err(|e| jni_err(env, "暗号文取得", e))?;

    let mut out = Vec::with_capacity(2 + iv.len() + ct.len());
    out.push(FORMAT_VERSION);
    out.push(iv.len() as u8);
    out.extend_from_slice(&iv);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn decrypt(enc: &[u8]) -> Result<Vec<u8>, String> {
    if enc.len() < 2 || enc[0] != FORMAT_VERSION {
        return Err("android keystore: 保存フォーマットが不正です".into());
    }
    let iv_len = enc[1] as usize;
    if enc.len() < 2 + iv_len {
        return Err("android keystore: 保存フォーマットが不正です".into());
    }
    let iv = &enc[2..2 + iv_len];
    let ct = &enc[2 + iv_len..];
    with_env(|env| decrypt_with(env, iv, ct))
}

fn decrypt_with(env: &mut JNIEnv, iv: &[u8], ct: &[u8]) -> Result<Vec<u8>, String> {
    let key = get_or_create_key(env)?;
    let cipher = cipher_instance(env)?;

    let spec_class = env
        .find_class("javax/crypto/spec/GCMParameterSpec")
        .map_err(|e| jni_err(env, "GCMParameterSpec クラス解決", e))?;
    let iv_arr = env
        .byte_array_from_slice(iv)
        .map_err(|e| jni_err(env, "IV バッファ生成", e))?;
    let spec = env
        .new_object(
            spec_class,
            "(I[B)V",
            &[JValue::Int(128), JValue::Object(&iv_arr)],
        )
        .map_err(|e| jni_err(env, "GCMParameterSpec new", e))?;

    // Cipher.DECRYPT_MODE = 2
    env.call_method(
        &cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
        &[JValue::Int(2), JValue::Object(&key), JValue::Object(&spec)],
    )
    .and_then(|v| v.v())
    .map_err(|e| jni_err(env, "Cipher.init(decrypt)", e))?;

    let input = env
        .byte_array_from_slice(ct)
        .map_err(|e| jni_err(env, "暗号文バッファ生成", e))?;
    let pt_obj = env
        .call_method(&cipher, "doFinal", "([B)[B", &[JValue::Object(&input)])
        .and_then(|v| v.l())
        .map_err(|e| jni_err(env, "Cipher.doFinal(decrypt)", e))?;
    env.convert_byte_array(JByteArray::from(pt_obj))
        .map_err(|e| jni_err(env, "平文取得", e))
}
