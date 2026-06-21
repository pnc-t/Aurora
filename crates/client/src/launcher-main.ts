// ランチャーウィンドウ (未接続) のエントリ。
// 接続操作では Rust の open_capture_window を呼び、別の capture ウィンドウを開く。
// ランチャー自身は閉じず、複数ホストへは「ウィンドウを複数開く」形で同時接続できる。

import "./styles/base.css";
import { mountLauncher } from "./ui/launcher";
import { openCaptureWindow } from "./core/ipc";
import { installWebviewGuard } from "./core/webview-guard";
import { installGlobalErrorLogging } from "./core/log";

// 未捕捉例外/Promise reject を Rust ログ (client.log) へ転送する。
installGlobalErrorLogging();

// ランチャーでも WebView 既定挙動 (右クリックメニュー / リロード / devtools / ズーム) を抑止する。
// capture 用のローカルホットキーは無いのでコールバックは渡さない。
installWebviewGuard();

const overlay = document.getElementById("connect-overlay") as HTMLElement;

const launcher = mountLauncher({
  root: overlay,
  // onConnect の解決 = capture ウィンドウ生成成功。実際の接続進捗/エラーは capture
  // ウィンドウ側に表示される。ここで reject すると launcher のダイアログがエラーを出す。
  onConnect: async (entry, password, save) => {
    await openCaptureWindow(entry.host, entry.port, save, password);
  },
});

launcher.show();
