// フロント (TypeScript) 用の薄い logger。
//
// 出力は Rust 側へ転送し `client.<日付>.log` に一本化する。WebView の DevTools コンソールは
// webview-guard が F12/右クリックを塞いでいて Tauri 上では基本開けず、release では DevTools
// 自体が無いため、console には出さない (ログを見る正規ルートはファイル)。未捕捉例外も含めて
// ファイルに残して後から調査できるようにする。
//
// 使い方: `console.warn(...)` を `logger.warn(...)` に置き換える。引数は console と同じく
// 可変個 (文字列・オブジェクト・Error 等)。高頻度 (映像フレーム単位) のログは送らないこと。

import { invoke } from "@tauri-apps/api/core";

type Level = "error" | "warn" | "info" | "debug";

/** 1 つの引数を表示用の文字列にする。Error はメッセージ + stack、オブジェクトは JSON。 */
function stringifyArg(arg: unknown): string {
  if (typeof arg === "string") return arg;
  if (arg instanceof Error) return arg.stack ?? `${arg.name}: ${arg.message}`;
  try {
    return JSON.stringify(arg);
  } catch {
    return String(arg);
  }
}

function format(args: unknown[]): string {
  return args.map(stringifyArg).join(" ");
}

/** Rust へ転送する。転送自体の失敗でアプリを巻き込まない (console には既に出している)。 */
function forward(level: Level, message: string): void {
  void invoke("frontend_log", { level, message }).catch(() => {
    /* 転送失敗は無視 (Tauri 文脈外の単体 Vite 起動など) */
  });
}

function emit(level: Level, args: unknown[]): void {
  // ログは Rust の client.log へ一本化する。WebView の DevTools コンソールは
  // webview-guard が F12/右クリックを塞いでいて Tauri 上では基本開けないため、
  // console へは出さない (見たいときは client.log を tail する)。
  forward(level, format(args));
}

export const logger = {
  error: (...args: unknown[]): void => emit("error", args),
  warn: (...args: unknown[]): void => emit("warn", args),
  info: (...args: unknown[]): void => emit("info", args),
  debug: (...args: unknown[]): void => emit("debug", args),
};

let installed = false;

/**
 * 未捕捉例外と未処理 Promise reject を捕まえて Rust ログへ転送する。
 * アプリ起動時 (main.ts) に一度だけ呼ぶ。
 */
export function installGlobalErrorLogging(): void {
  if (installed) return;
  installed = true;

  window.addEventListener("error", (ev) => {
    const where = ev.filename ? ` (${ev.filename}:${ev.lineno}:${ev.colno})` : "";
    forward("error", `uncaught: ${stringifyArg(ev.error ?? ev.message)}${where}`);
  });

  window.addEventListener("unhandledrejection", (ev) => {
    forward("error", `unhandledrejection: ${stringifyArg(ev.reason)}`);
  });
}
