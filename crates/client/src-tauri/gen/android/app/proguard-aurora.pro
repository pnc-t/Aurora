# release ビルドは isMinifyEnabled = true (build.gradle.kts) で ProGuard/R8 が難読化する。
# Rust 側 (mobile_clipboard) は com/aurora/remote/MainActivity と setClipboardText を
# クラス名・メソッド名の文字列で JNI 解決しているため、難読化されると名前が変わり無言で
# 解決に失敗する (クリップボード書き込みが黙って壊れる)。既存の proguard-tauri.pro は
# TauriActivity しか keep していないため、このファイルで MainActivity 側を別途 keep する。
#
# gradle 側の proguardFiles(*fileTree(".") { include("**/*.pro") } ...) が同ディレクトリの
# .pro を自動で拾うため、gen/android が再生成されてもこの設定は (このファイル自体が
# .gitignore の ! で追跡されている限り) 消えない。
-keep class com.aurora.remote.MainActivity { *; }
