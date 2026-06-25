// 1 セッション (= 1 ホストへの接続 / 1 タブ) 分の状態をまとめたクラス。
// マルチタブの capture ウィンドウは複数の Session を Map で保持し、アクティブな 1 つだけを
// 共有 UI (canvas / toolbar / status / stats) へ結びつける。非アクティブな Session は
// 映像を一時停止 (host PauseVideo) して帯域を止め、入力も外す。
//
// 共有 UI へのアクセスは `SessionHost` 経由。Session 自身は「自分がアクティブか」を host に
// 問い合わせ、アクティブなときだけ共有 UI を更新する。タブ切替時は orchestrator が
// 旧 Session を `deactivate()`、新 Session を `activate()` する。

import { Channel } from "@tauri-apps/api/core";
import {
  attachSession,
  connectToHost,
  disconnect,
  formatDisplayName,
  requestKeyframe,
  saveRecentHost,
  selectDisplay,
  sendInput,
  setActiveSession,
  setAudioMuted,
  setClientViewport,
  setResolution,
  setVideoParams,
  setVideoPaused,
  type ClientEvent,
  type ClientSettings,
  type DisplayDto,
  type HostEntry,
} from "./ipc";
import { Decoder } from "./decoder";
import { Input } from "./input";
import { logger } from "./log";
import type { ToolbarHandle } from "../ui/toolbar";
import type { ToastKind } from "../ui/toast";

export type StatusKind = "connecting" | "error" | "hidden";
export type StatsEvent = Extract<ClientEvent, { kind: "stats" }>;

/** タブの接続状態 (タブのドット色に使う)。 */
export type TabStatus = "connecting" | "connected" | "error";

/**
 * タブを別ウィンドウへ移譲する際の表示状態スナップショット (Phase 3 ドラッグ分割)。
 * networking (Rust セッション) は継続するため、ここには接続済み前提の「表示に必要な状態」
 * だけを入れる。移譲先はこれで `Session` を復元し、新 Channel をアタッチして表示を再開する。
 */
export type SessionSnapshot = {
  entry: HostEntry;
  save: boolean;
  displays: DisplayDto[];
  currentDisplayId: number | null;
  connLabel: string;
  audioAvailable: boolean;
  videoFps: number;
  videoMbps: number;
  captured: boolean;
};

/** Session が共有 UI / orchestrator へアクセスするための窓口。 */
export interface SessionHost {
  readonly canvas: HTMLCanvasElement;
  readonly toolbar: ToolbarHandle;
  getSettings(): ClientSettings;
  isActive(s: Session): boolean;
  /** 中央ステータスオーバーレイを再描画する (アクティブ時のみ実描画)。 */
  renderStatus(s: Session): void;
  /** トースト (アクティブ時のみ)。 */
  toast(s: Session, text: string, kind?: ToastKind): void;
  /** 統計 HUD 更新 (アクティブ時のみ)。 */
  updateStats(s: Session, ev: StatsEvent): void;
  /** 統計 HUD クリア (アクティブ時のみ)。 */
  clearStats(s: Session): void;
  /** ダウンロード結果を file パネルへ (アクティブ時のみ)。 */
  fileResult(s: Session, id: number, ok: boolean, message: string): void;
  /** タブの見た目 (ラベル/ドット) を更新するよう orchestrator へ通知。 */
  refreshTab(s: Session): void;
  /** このセッションが恒久終了した (ユーザー切断/接続不正)。タブ除去・最後なら窓を閉じる。 */
  removed(s: Session): void;
  /** 窓単位のリサイズ可否を設定する。 */
  setResizable(on: boolean): void;
}

const RECONNECT_DELAYS_MS = [1000, 2000, 4000, 8000, 15000];
const MAX_RECONNECT_ATTEMPTS = 10;
const RECONNECT_STABLE_MS = 6000;

export class Session {
  readonly id: string;
  readonly entry: HostEntry;

  private readonly decoder: Decoder;
  private readonly input: Input;

  private connected = false;
  private active = false;

  get isConnected(): boolean {
    return this.connected;
  }

  // 接続前/切断後の中央オーバーレイ状態 (アクティブ時に orchestrator が描画する)。
  status: StatusKind = "connecting";
  statusText = "";
  statusShowClose = false;

  // タブ表示用 (orchestrator が読む)。
  get tabLabel(): string {
    return this.entry.label || this.entry.host;
  }
  tabStatus: TabStatus = "connecting";

  // host_info から得たディスプレイ一覧と現在の対象。
  private currentDisplays: DisplayDto[] = [];
  private currentDisplay: DisplayDto | null = null;

  // toolbar 再同期用に保持する接続情報。
  private connLabel = "";
  private audioAvailable = false;
  private videoFps = 60;
  private videoMbps = 15;
  private latestStats: StatsEvent | null = null;

  // セッション世代。openSession ごとに進め、遅れて届く旧チャネルのイベントを無視する。
  private sessionGen = 0;
  private connectInFlight = false;
  // reader/writer 双方が Disconnected を送るための実行済みフラグ。
  private disconnectHandled = true;

  // 自動再接続。
  private reconnecting = false;
  private reconnectAttempt = 0;
  private reconnectTimer: number | null = null;
  private stabilizeTimer: number | null = null;

  // ホスト解像度の自動追従 (RDP 風)。
  private viewportTimer: number | null = null;
  private lastSentVw = 0;
  private lastSentVh = 0;

  constructor(
    id: string,
    entry: HostEntry,
    private readonly save: boolean,
    private readonly host: SessionHost,
  ) {
    this.id = id;
    this.entry = entry;
    const settings = host.getSettings();
    this.videoFps = settings.fps;
    this.videoMbps = settings.max_mbps;
    this.decoder = new Decoder(host.canvas, {
      requestKeyframe: () => void requestKeyframe(this.id),
    });
    this.input = new Input({
      sendInput: (ev) => void sendInput(this.id, ev).catch((e) => logger.warn("sendInput failed", e)),
    });
  }

  // ---- ライフサイクル ----

  /** 最初の接続を開く (起動時に orchestrator が呼ぶ)。 */
  async start(): Promise<void> {
    logger.info(`session start: ${this.entry.host}:${this.entry.port}`);
    this.setStatus(`connecting to ${this.entry.host}…`, "connecting");
    await this.openSession(this.save);
  }

  /** タブをアクティブにする: 入力 attach・映像再開・共有 UI 再同期。 */
  activate(): void {
    this.active = true;
    setActiveSession(this.id);
    if (this.connected) {
      void setVideoPaused(this.id, false);
      // 一時停止中に参照フレームが古くなっているため、作り直してキーフレームから復帰する。
      this.decoder.reset();
      void requestKeyframe(this.id);
      this.attachInputToCurrent();
    }
    this.syncSharedUi();
  }

  /** タブを非アクティブにする: 入力 detach・映像一時停止。 */
  deactivate(): void {
    this.active = false;
    this.input.detach();
    if (this.connected) void setVideoPaused(this.id, true);
  }

  /** ユーザー操作の切断 (ツールバー切断ボタン)。恒久終了し、orchestrator がタブを除去する。 */
  closeByUser(): void {
    logger.info(`disconnect by user: ${this.entry.host}`);
    this.teardown();
    void disconnect(this.id);
    this.host.removed(this);
  }

  /** タブ除去/窓終了時の資源解放。 */
  destroy(): void {
    this.cancelReconnect();
    if (this.viewportTimer !== null) {
      clearTimeout(this.viewportTimer);
      this.viewportTimer = null;
    }
    this.input.detach();
    this.decoder.close();
    void disconnect(this.id);
  }

  // ---- タブのウィンドウ間移動 (Phase 3 ドラッグ分割) ----

  /** 移譲用の表示状態スナップショットを作る。接続中のみ意味を持つ。 */
  snapshot(): SessionSnapshot {
    return {
      entry: this.entry,
      save: this.save,
      displays: this.currentDisplays,
      currentDisplayId: this.currentDisplay?.id ?? null,
      connLabel: this.connLabel,
      audioAvailable: this.audioAvailable,
      videoFps: this.videoFps,
      videoMbps: this.videoMbps,
      captured: this.input.isCaptured(),
    };
  }

  /**
   * このタブを別ウィンドウへ移譲した: networking (Rust セッション) は止めず、この窓の
   * ローカル資源 (decoder/input/タイマー) だけ解放する。`disconnect` は呼ばない。
   * 世代を進め、デタッチ後に旧 Channel へ遅れて届くイベント/映像を無視する。
   */
  detachForHandoff(): void {
    this.disconnectHandled = true; // この窓ではもう切断イベントを扱わない
    this.cancelReconnect();
    if (this.viewportTimer !== null) {
      clearTimeout(this.viewportTimer);
      this.viewportTimer = null;
    }
    this.input.detach();
    this.decoder.close();
    this.sessionGen++;
  }

  /**
   * 別ウィンドウから移譲されたセッションを引き受ける。再接続はせず、新しい Channel を
   * 既存の Rust セッションへアタッチして表示状態を復元する。映像は呼び出し後の
   * `activate()` (= setActive) が requestKeyframe で復帰させる。
   */
  async adoptFrom(snap: SessionSnapshot): Promise<void> {
    this.currentDisplays = snap.displays;
    this.currentDisplay =
      snap.currentDisplayId !== null
        ? (snap.displays.find((d) => d.id === snap.currentDisplayId) ?? snap.displays[0] ?? null)
        : (snap.displays[0] ?? null);
    this.connLabel = snap.connLabel;
    this.audioAvailable = snap.audioAvailable;
    this.videoFps = snap.videoFps;
    this.videoMbps = snap.videoMbps;
    this.connected = true;
    this.tabStatus = "connected";
    this.setStatus("", "hidden");
    this.disconnectHandled = false;

    const gen = ++this.sessionGen;
    const { event, video } = this.newChannels(gen);
    await attachSession(this.id, event, video);
  }

  // ---- 共有 UI へのフック (orchestrator が呼ぶ) ----

  isCaptured(): boolean {
    return this.input.isCaptured();
  }
  setCaptureMode(on: boolean): void {
    this.input.setCaptureMode(on);
  }

  switchDisplay(d: DisplayDto): void {
    void selectDisplay(this.id, d.id);
    this.currentDisplay = d;
    this.decoder.reset();
    this.input.attach(this.host.canvas, { width: d.width, height: d.height }, {
      onCaptureChange: (c) => this.onCaptureChange(c),
    });
    this.host.canvas.focus();
    const dn = formatDisplayName(d.name);
    this.connLabel = `${this.entry.host} · ${dn} ${d.width}x${d.height}`;
    if (this.active) this.host.toolbar.setConnLabel(this.connLabel);
    this.host.toast(this, `switching to ${dn} ${d.width}x${d.height}`);
    this.sendViewportNow(true);
  }

  setVideoParams(fps: number, mbps: number): void {
    this.videoFps = fps;
    this.videoMbps = mbps;
    void setVideoParams(this.id, fps, mbps * 1_000_000);
  }
  setResolution(maxHeight: number): void {
    void setResolution(this.id, maxHeight);
    this.host.toast(this, maxHeight === 0 ? "解像度: フル" : `解像度: ${maxHeight}p`);
  }
  setAudioMuted(muted: boolean): void {
    void setAudioMuted(this.id, muted);
    this.host.toast(this, muted ? "音声ミュート" : "音声ミュート解除");
  }

  /** 窓リサイズ (アクティブ Session のみ orchestrator が呼ぶ)。 */
  onWindowResize(): void {
    if (this.connected) this.scheduleSendViewport();
  }

  /** アクティブ化時に共有 UI を自分の状態へ揃える。 */
  syncSharedUi(): void {
    this.host.renderStatus(this);
    if (!this.connected) return;
    const tb = this.host.toolbar;
    // 別タブで切断が起きて toolbar が connected=false にされていても、健全なタブへ
    // 切り替えたら確実に表示状態へ戻す。
    tb.setConnected(true, this.host.getSettings().toolbar_pinned);
    tb.setDisplays(this.toolbarDisplayList(), this.currentDisplay?.id);
    tb.setConnLabel(this.connLabel);
    tb.setAudioAvailable(this.audioAvailable);
    tb.setVideoParams(this.videoFps, this.videoMbps);
    tb.setCaptureToggled(this.input.isCaptured());
    if (this.latestStats) this.host.updateStats(this, this.latestStats);
    else this.host.clearStats(this);
  }

  // ---- 内部 ----

  private setStatus(text: string, kind: StatusKind, showClose = false): void {
    this.status = kind;
    this.statusText = text;
    this.statusShowClose = showClose;
    if (kind === "connecting") this.tabStatus = "connecting";
    else if (kind === "error") this.tabStatus = "error";
    this.host.refreshTab(this);
    this.host.renderStatus(this);
  }

  private onCaptureChange(captured: boolean): void {
    if (this.active) this.host.toolbar.setCaptureToggled(captured);
    this.host.toast(this, captured ? "mouse captured (Esc to release)" : "mouse released");
  }

  private toolbarDisplayList(): DisplayDto[] {
    if (this.host.getSettings().use_virtual_display) {
      const v = this.currentDisplays.find((d) => d.is_virtual);
      return v ? [v] : [];
    }
    return this.currentDisplays.filter((d) => !d.is_virtual);
  }

  private attachInputToCurrent(): void {
    const d = this.currentDisplay;
    if (!d) return;
    this.input.attach(this.host.canvas, { width: d.width, height: d.height }, {
      onCaptureChange: (c) => this.onCaptureChange(c),
    });
    this.host.canvas.focus();
  }

  // --- 接続 ---

  /** 新しい event/video Channel を作り、この世代 (gen) に紐づけて配線する。 */
  private newChannels(gen: number): {
    event: Channel<ClientEvent>;
    video: Channel<ArrayBuffer>;
  } {
    const event = new Channel<ClientEvent>();
    const video = new Channel<ArrayBuffer>();
    event.onmessage = (ev) => this.handleClientEvent(ev, gen);
    video.onmessage = (data) => {
      if (gen !== this.sessionGen) return; // 旧セッションの残映像は捨てる
      if (!this.active) return; // 非アクティブタブは描画しない (host も一時停止中)
      const buf =
        data instanceof ArrayBuffer ? data : new Uint8Array(data as unknown as number[]).buffer;
      this.decoder.handleChunk(buf);
    };
    return { event, video };
  }

  private async openSession(save: boolean): Promise<void> {
    if (this.connectInFlight) return;
    this.connectInFlight = true;
    const gen = ++this.sessionGen;
    this.disconnectHandled = false;
    this.decoder.reset();
    const { event, video } = this.newChannels(gen);
    try {
      await connectToHost(this.id, this.entry.host, this.entry.port, save, event, video);
    } finally {
      this.connectInFlight = false;
    }
  }

  private handleClientEvent(ev: ClientEvent, gen: number): void {
    if (gen !== this.sessionGen) return; // 旧セッションの残イベントは無視
    switch (ev.kind) {
      case "connected":
        this.setStatus("handshake complete…", "connecting");
        break;
      case "host_info":
        this.onHostInfo(ev);
        break;
      case "display_resized": {
        const idx = this.currentDisplays.findIndex((dd) => dd.id === ev.id);
        if (idx >= 0) {
          this.currentDisplays[idx] = { ...this.currentDisplays[idx], width: ev.width, height: ev.height };
        }
        if (this.currentDisplay && this.currentDisplay.id === ev.id) {
          this.currentDisplay = { ...this.currentDisplay, width: ev.width, height: ev.height };
        }
        this.decoder.reset();
        if (this.active) this.attachInputToCurrent();
        const dn = this.currentDisplay ? formatDisplayName(this.currentDisplay.name) : `Display ${ev.id}`;
        this.connLabel = `${this.entry.host} · ${dn} ${ev.width}x${ev.height}`;
        if (this.active) {
          this.host.toolbar.setDisplays(this.toolbarDisplayList(), ev.id);
          this.host.toolbar.setConnLabel(this.connLabel);
        }
        break;
      }
      case "stats":
        this.latestStats = ev;
        this.host.updateStats(this, ev);
        break;
      case "pong":
        break;
      case "file_result":
        this.host.fileResult(this, ev.id, ev.ok, ev.message);
        this.host.toast(
          this,
          ev.ok ? `file ok: ${ev.message}` : `file failed: ${ev.message}`,
          ev.ok ? "success" : "error",
        );
        break;
      case "disconnected":
        this.doDisconnect(`disconnected: ${ev.reason}`);
        break;
    }
  }

  private onHostInfo(ev: Extract<ClientEvent, { kind: "host_info" }>): void {
    // markSessionEstablished は reconnecting を落とすので、再接続成立かどうかは先に拾う。
    const wasReconnecting = this.reconnecting;
    this.markSessionEstablished();
    this.connected = true;
    this.tabStatus = "connected";
    this.setStatus("", "hidden");
    if (this.entry.host) void saveRecentHost(this.entry);
    const settings = this.host.getSettings();
    this.audioAvailable = ev.audio;
    this.currentDisplays = ev.displays;

    let target = ev.displays[0];
    if (settings.use_virtual_display) {
      const vd = ev.displays.find((dd) => dd.is_virtual);
      if (vd) {
        target = vd;
      } else {
        this.host.toast(this, "仮想ディスプレイが見つかりません (ホストに VDD 未導入?)", "error");
      }
    }
    this.currentDisplay = target ?? null;
    if (target) {
      if (target.id !== ev.displays[0]?.id) void selectDisplay(this.id, target.id);
      this.connLabel = `${this.entry.host} · ${formatDisplayName(target.name)} ${target.width}x${target.height}`;
      if (this.active) this.attachInputToCurrent();
      this.sendViewportNow(true);
    } else {
      this.connLabel = `${this.entry.host} · (no displays?)`;
    }

    // 映像パラメータ・解像度はホストへ適用 (capture スレッドはセッション跨ぎで設定を保持)。
    this.videoFps = settings.fps;
    this.videoMbps = settings.max_mbps;
    void setVideoParams(this.id, settings.fps, settings.max_mbps * 1_000_000);
    void setResolution(this.id, 0);
    // 非アクティブで接続完了したタブは即一時停止する (帯域節約)。
    if (!this.active) void setVideoPaused(this.id, true);

    const disp = target ? `${formatDisplayName(target.name)} ${target.width}x${target.height}` : "none";
    logger.info(
      `${wasReconnecting ? "reconnected" : "connected"}: ${this.entry.host} display=${disp} audio=${ev.audio}`,
    );

    this.host.refreshTab(this);

    if (this.active) {
      const tb = this.host.toolbar;
      tb.setPosition(settings.toolbar_position);
      tb.setConnected(true, settings.toolbar_pinned);
      tb.setAudioAvailable(ev.audio);
      this.host.setResizable(true);
      tb.setDisplays(this.toolbarDisplayList(), target?.id);
      tb.setConnLabel(this.connLabel);
      tb.setVideoParams(settings.fps, settings.max_mbps);
      if (settings.default_capture) this.input.setCaptureMode(true);
      this.host.toast(this, "connected", "success");
    }
  }

  // --- 切断 ---

  private doDisconnect(reason: string): void {
    if (this.disconnectHandled) return;
    this.disconnectHandled = true;
    logger.info(`session down: ${this.entry.host} — ${reason}`);
    this.clearStabilizeTimer();
    void disconnect(this.id);
    this.connected = false;
    if (this.viewportTimer !== null) {
      clearTimeout(this.viewportTimer);
      this.viewportTimer = null;
    }
    this.lastSentVw = 0;
    this.lastSentVh = 0;
    this.input.detach();
    if (this.active) {
      this.host.toolbar.setConnected(false);
      this.host.toolbar.setCaptureToggled(false);
      this.host.clearStats(this);
    }

    const errored = reason.startsWith("disconnected:");
    this.host.toast(this, reason, errored ? "error" : "info");
    if (errored && this.host.getSettings().auto_reconnect) {
      this.startReconnect();
    } else {
      this.tabStatus = "error";
      this.setStatus(errored ? reason : "切断されました", "error", true);
    }
  }

  private teardown(): void {
    this.disconnectHandled = true;
    this.cancelReconnect();
    this.connected = false;
    if (this.viewportTimer !== null) {
      clearTimeout(this.viewportTimer);
      this.viewportTimer = null;
    }
    this.input.detach();
    this.decoder.close();
  }

  // --- 自動再接続 ---

  private clearStabilizeTimer(): void {
    if (this.stabilizeTimer !== null) {
      clearTimeout(this.stabilizeTimer);
      this.stabilizeTimer = null;
    }
  }

  private cancelReconnect(): void {
    this.reconnecting = false;
    this.reconnectAttempt = 0;
    this.clearStabilizeTimer();
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private markSessionEstablished(): void {
    this.reconnecting = false;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.clearStabilizeTimer();
    this.stabilizeTimer = window.setTimeout(() => {
      this.stabilizeTimer = null;
      this.reconnectAttempt = 0;
    }, RECONNECT_STABLE_MS);
  }

  private startReconnect(): void {
    if (this.reconnecting && this.reconnectTimer !== null) return;
    this.reconnecting = true;
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
    const delay = RECONNECT_DELAYS_MS[Math.min(this.reconnectAttempt, RECONNECT_DELAYS_MS.length - 1)];
    this.setStatus(
      `再接続待機 ${Math.round(delay / 1000)}s… (試行 ${this.reconnectAttempt + 1})`,
      "connecting",
    );
    this.reconnectTimer = window.setTimeout(() => void this.attemptReconnect(), delay);
  }

  private async attemptReconnect(): Promise<void> {
    this.reconnectTimer = null;
    if (!this.reconnecting) return;
    if (this.reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
      this.cancelReconnect();
      logger.warn(`reconnect gave up after ${MAX_RECONNECT_ATTEMPTS} attempts: ${this.entry.host}`);
      this.setStatus("再接続に失敗しました", "error", true);
      this.host.toast(this, "再接続に失敗しました", "error");
      return;
    }
    this.reconnectAttempt++;
    logger.info(`reconnect attempt ${this.reconnectAttempt}: ${this.entry.host}`);
    this.setStatus(`再接続中… (試行 ${this.reconnectAttempt})`, "connecting");
    this.host.toast(this, `再接続を試行中… (${this.reconnectAttempt})`);
    try {
      // password=null で保存済み (DPAPI) を使う。再保存不要なので save=false。
      await this.openSession(false);
    } catch (e) {
      logger.warn(`reconnect attempt ${this.reconnectAttempt} failed`, e);
      if (!this.reconnecting) return;
      this.scheduleReconnect();
    }
  }

  // --- ホスト解像度の自動追従 ---

  private currentViewportPx(): { w: number; h: number } {
    const dpr = window.devicePixelRatio || 1;
    return {
      w: Math.max(1, Math.round(window.screen.width * dpr)),
      h: Math.max(1, Math.round(window.screen.height * dpr)),
    };
  }

  private sendViewportNow(force = false): void {
    if (!this.connected) return;
    const useVirtual = this.currentDisplay?.is_virtual ?? false;
    if (!useVirtual) return;
    const { w, h } = this.currentViewportPx();
    if (!force && Math.abs(w - this.lastSentVw) < 8 && Math.abs(h - this.lastSentVh) < 8) return;
    this.lastSentVw = w;
    this.lastSentVh = h;
    void setClientViewport(this.id, w, h);
  }

  private scheduleSendViewport(): void {
    if (this.viewportTimer !== null) clearTimeout(this.viewportTimer);
    this.viewportTimer = window.setTimeout(() => {
      this.viewportTimer = null;
      this.sendViewportNow();
    }, 400);
  }
}
