import { Channel } from "@tauri-apps/api/core";
import { connectToHost, disconnect, selectDisplay, type ClientEvent } from "./ipc";
import { handleVideoChunk, initDecoder, resetDecoder } from "./decoder";
import { attachInput, detachInput } from "./input";
import { mountConnectionUi, setHostInfo, setStatus } from "./connection-ui";
import { handleFileResult, mountFileTransfer } from "./file-transfer";

const toolbar = document.getElementById("toolbar") as HTMLElement;
const canvas = document.getElementById("screen") as HTMLCanvasElement;

initDecoder(canvas);

mountConnectionUi({
  root: toolbar,
  onSubmit: async (host, password) => {
    resetDecoder();
    const eventChannel = new Channel<ClientEvent>();
    const videoChannel = new Channel<ArrayBuffer>();
    eventChannel.onmessage = handleClientEvent;
    videoChannel.onmessage = (data) => {
      // Tauri は ArrayBuffer / number[] のどちらかで来る場合がある
      const buf = data instanceof ArrayBuffer ? data : new Uint8Array(data as unknown as number[]).buffer;
      handleVideoChunk(buf);
    };
    await connectToHost(host, password, eventChannel, videoChannel);
  },
  onDisconnect: () => {
    void disconnect();
    detachInput();
    setStatus("disconnected");
  },
  onSelectDisplay: (d) => {
    // 解像度/SPS が変わるためデコーダを作り直し、入力スケールを新ディスプレイへ更新。
    void selectDisplay(d.id);
    resetDecoder();
    attachInput(canvas, { width: d.width, height: d.height });
    canvas.focus();
    setStatus(`switching to ${d.name} ${d.width}x${d.height}…`);
  },
});

// 接続フォーム上にファイル送信 UI を追加する。
const connForm = document.getElementById("conn-form");
if (connForm) mountFileTransfer(connForm);

function handleClientEvent(ev: ClientEvent): void {
  switch (ev.kind) {
    case "connected":
      setStatus("handshake complete");
      break;
    case "host_info": {
      setHostInfo(ev.displays);
      const d = ev.displays[0];
      if (d) {
        attachInput(canvas, { width: d.width, height: d.height });
        canvas.focus();
      }
      break;
    }
    case "stats":
      setStatus(`fps=${ev.fps} br=${ev.bitrate_kbps}kbps rtt=${(ev.rtt_us / 1000).toFixed(1)}ms loss=${ev.loss_permille}‰`);
      break;
    case "pong":
      // RTT 表示は stats 側でやる想定 (MVP は無視)
      break;
    case "file_result":
      handleFileResult(ev.id, ev.ok, ev.message);
      break;
    case "disconnected":
      detachInput();
      setStatus(`disconnected: ${ev.reason}`);
      break;
  }
}
