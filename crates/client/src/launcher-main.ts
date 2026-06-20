// ランチャーウィンドウ (未接続) のエントリ。
// 接続操作では Rust の open_capture_window を呼び、別の capture ウィンドウを開く。
// ランチャー自身は閉じず、複数ホストへは「ウィンドウを複数開く」形で同時接続できる。

import "./styles/base.css";
import { mountLauncher } from "./ui/launcher";
import { openCaptureWindow } from "./core/ipc";

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
