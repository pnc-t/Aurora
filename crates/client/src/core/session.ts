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
  setAudioVolume,
  setClipboardSync,
  setCursorOverlay,
  setClientViewport,
  setFileSharing,
  setKeyboardGrab,
  setResolution,
  setSupportedCodecs,
  setVideoParams,
  setVideoPaused,
  DEFAULT_SETTINGS,
  type ClientEvent,
  type ClientSettings,
  type DisplayDto,
  type HostEntry,
} from "./ipc";
import { probeSupportedCodecs, type CodecId } from "./codec";
import { Decoder } from "./decoder";
import { Input } from "./input";
import { logger } from "./log";
import type { ToolbarHandle } from "../ui/toolbar";
import type { ToastKind } from "../ui/toast";

/** 2 つの codec 配列が同じ内容 (順序込み) か。 */
function sameCodecs(a: CodecId[], b: CodecId[]): boolean {
  return a.length === b.length && a.every((c, i) => c === b[i]);
}

export type StatusKind = "connecting" | "pending" | "error" | "hidden";
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
  /**
   * ダウンロード結果を file パネルへ。取りこぼし防止のため非アクティブ時も必ず呼ばれる
   * (状態更新は常に行い、DOM 描画をアクティブタブに限定するのは実装側の責務)。
   */
  fileResult(s: Session, id: number, ok: boolean, message: string): void;
  /** ファイル転送の進捗を file パネルへ (fileResult と同様、非アクティブ時も呼ばれる)。done/total はバイト数。 */
  fileProgress(s: Session, id: number, done: number, total: number): void;
  /** 静かな再接続中の常駐バッジを表示する (アクティブ時のみ)。最後の映像を隠さない。 */
  reconnectBadge(s: Session, text: string): void;
  /** 再接続バッジを消す (アクティブ時のみ)。 */
  clearReconnectBadge(s: Session): void;
  /** タブの見た目 (ラベル/ドット) を更新するよう orchestrator へ通知。 */
  refreshTab(s: Session): void;
  /** このセッションが恒久終了した (ユーザー切断/接続不正)。タブ除去・最後なら窓を閉じる。 */
  removed(s: Session): void;
  /** 窓単位のリサイズ可否を設定する。 */
  setResizable(on: boolean): void;
  /**
   * ツールバーで変えた映像パラメータを保存済み設定へ書き戻す。
   * セッション内メモリだけだと再接続・新規接続のたびに保存値へ巻き戻ってしまうため、
   * ツールバーとランチャー設定は同一設定の 2 つのビューとして扱う。
   */
  persistVideoParams(fps: number, mbps: number): void;
}

const RECONNECT_DELAYS_MS = [1000, 1000, 1000, 2000, 4000, 8000, 15000];
const MAX_RECONNECT_ATTEMPTS = 10;
const RECONNECT_STABLE_MS = 6000;
/** この試行回数までは全面オーバーレイを出さず、常駐バッジのみで再接続を試みる (最後の映像を
 *  見せたまま静かに再接続する)。これを超えたら従来どおりの全面オーバーレイへ格上げする。 */
const QUIET_RECONNECT_ATTEMPTS = 3;
/** デバイスピン留め承認待ちの固定リトライ間隔。承認/拒否まで host 側で待たされる想定なので
 *  自動再接続のバックオフ (RECONNECT_DELAYS_MS) とは別系統にする。 */
const PENDING_RETRY_MS = 3000;

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
  // ミュートの現在値。ツールバー UI 側にしか状態が無いと、syncSharedUi での復元時に必ず
  // 未ミュートへ戻ってしまう (モバイルは activate() 経由の syncSharedUi が唯一の初期化経路の
  // ため、ミュートしても再接続で毎回解除される実機バグの原因になっていた)。
  private audioMuted = false;
  // effectiveSettings で接続時に上書きされるが、初期値も既定設定 (core/ipc.ts) に揃えておく。
  private videoFps = DEFAULT_SETTINGS.fps;
  private videoMbps = DEFAULT_SETTINGS.max_mbps;
  private latestStats: StatsEvent | null = null;

  // 現在の映像コーデック (video_codec_changed で確定)。stats HUD 表示用に公開する。
  private currentCodec: CodecId = "h264";
  get videoCodec(): CodecId {
    return this.currentCodec;
  }
  // 直近に申告した (connect_to_host / set_supported_codecs で送った) コーデック集合。
  // 設定変更時に再計算した集合と比較し、変わっていなければ set_supported_codecs を送らない。
  private declaredCodecs: CodecId[] = [];

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
  // 静かな再接続バッジの直近テキスト ("" = 非表示中)。タブ切替での再同期 (syncSharedUi) に使う。
  private reconnectBadgeText = "";

  // ホスト解像度の自動追従 (RDP 風)。
  private viewportTimer: number | null = null;
  private lastSentVw = 0;
  private lastSentVh = 0;

  // 仮想ディスプレイのテイクオーバー待ち (host が物理→仮想へ切替中、完了 (display_resized) まで
  // 固まった物理側の映像を隠すためオーバーレイを維持する)。
  private preparingVirtual = false;
  private prepareTimer: number | null = null;

  // デバイスピン留め承認待ち (host のペアリング窓中)。承認/拒否/窓終了まで固定間隔で再試行する。
  private pendingApproval = false;
  private pendingTimer: number | null = null;

  // 別ウィンドウへの移譲 (分割/移動) 中フラグ。移譲確定 (detachForHandoff) 前に closeByUser が
  // 競合すると、移譲先で adopt した直後にこの窓からの disconnect が届いて shutdown してしまう。
  private migrating = false;

  constructor(
    id: string,
    entry: HostEntry,
    private readonly save: boolean,
    private readonly host: SessionHost,
  ) {
    this.id = id;
    this.entry = entry;
    const settings = this.effectiveSettings();
    this.videoFps = settings.fps;
    this.videoMbps = settings.max_mbps;
    this.decoder = new Decoder(host.canvas, {
      requestKeyframe: () => void requestKeyframe(this.id),
    });
    this.input = new Input({
      sendInput: (ev) => void sendInput(this.id, ev).catch((e) => logger.warn("sendInput failed", e)),
    });
    // OS の "online" イベント (Wi-Fi→有線切替等でネットワークが復帰した通知) を受けたら、
    // 再接続待機中ならその場で待機をスキップして即試行する (delay 満了を待たない)。
    window.addEventListener("online", this.onNetworkOnline);
  }

  /** "online" イベントハンドラ (bound field: destroy 時に同じ参照で removeEventListener する)。 */
  private onNetworkOnline = (): void => {
    if (!this.reconnecting || this.reconnectTimer === null) return;
    logger.info(`network online event; skipping reconnect wait: ${this.entry.host}`);
    clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    void this.attemptReconnect();
  };

  // ---- ライフサイクル ----

  /** 最初の接続を開く (起動時に orchestrator が呼ぶ)。 */
  async start(): Promise<void> {
    logger.info(`session start: ${this.entry.host}:${this.entry.port}`);
    this.setStatus(`connecting to ${this.entry.host}…`, "connecting", true);
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

  /**
   * 入力を外す。input.detach() は pointerlockchange リスナーを外してから exitPointerLock するため
   * onCaptureChange(false) (= キーボード占有 OFF) が発火しない。キャプチャ中のままタブ切替・切断・
   * 移譲・終了するとローカル PC の Win / Alt+Tab が握り潰されたまま残るため、ここで明示的に落とす。
   */
  private detachInput(): void {
    const wasCaptured = this.input.isCaptured();
    this.input.detach();
    if (wasCaptured) void setKeyboardGrab(false).catch((e) => logger.warn("setKeyboardGrab failed", e));
  }

  /** タブを非アクティブにする: 入力 detach・映像一時停止。 */
  deactivate(): void {
    this.active = false;
    this.detachInput();
    if (this.connected) void setVideoPaused(this.id, true);
  }

  /** ユーザー操作の切断 (ツールバー切断ボタン)。恒久終了し、orchestrator がタブを除去する。 */
  closeByUser(): void {
    // 移譲中 (splitTab/moveTabToNewWindow の await 中) は無視する。ここで disconnect すると
    // 移譲先が adopt した直後にセッションが shutdown される競合になるため。
    if (this.migrating) return;
    logger.info(`disconnect by user: ${this.entry.host}`);
    this.teardown();
    void disconnect(this.id, true);
    this.host.removed(this);
  }

  // ---- タブのウィンドウ間移動 (Phase 3 ドラッグ分割) ----

  /** 移譲 (別ウィンドウへの分割/移動) を開始する: 確定するまで closeByUser を無視させる。 */
  beginMigration(): void {
    this.migrating = true;
  }

  /** 移譲が失敗した: このタブを通常どおり (closeByUser 可能な状態に) 戻す。 */
  cancelMigration(): void {
    this.migrating = false;
  }

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
    window.removeEventListener("online", this.onNetworkOnline);
    this.disconnectHandled = true; // この窓ではもう切断イベントを扱わない
    this.cancelReconnect();
    if (this.viewportTimer !== null) {
      clearTimeout(this.viewportTimer);
      this.viewportTimer = null;
    }
    if (this.prepareTimer !== null) {
      clearTimeout(this.prepareTimer);
      this.prepareTimer = null;
    }
    this.preparingVirtual = false;
    this.detachInput();
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

  /** WH_KEYBOARD_LL で奪った OS ショートカット (Win/Alt+Tab 等) をこのセッションへ注入する。 */
  sendGrabbedKey(vk: number, down: boolean): void {
    void sendInput(this.id, { Key: { vk, down } }).catch((e) =>
      logger.warn("sendGrabbedKey failed", e),
    );
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
    this.host.persistVideoParams(fps, mbps);
  }
  setResolution(maxHeight: number): void {
    void setResolution(this.id, maxHeight);
    this.host.toast(this, maxHeight === 0 ? "解像度: フル" : `解像度: ${maxHeight}p`);
  }
  setAudioMuted(muted: boolean): void {
    this.audioMuted = muted;
    void setAudioMuted(this.id, muted);
    this.host.toast(this, muted ? "音声ミュート" : "音声ミュート解除");
  }
  /** クライアント再生音量の変更 (host へは送らない。client 内部のゲインのみ)。 */
  setAudioVolume(volume: number): void {
    void setAudioVolume(this.id, volume);
  }

  // クリップボード同期のセッション毎の現在値。Rust 側ゲートは接続時に設定値で初期化される
  // ため、ここも接続時 (onHostInfo) に同じ設定値へ揃え、以後はツールバートグルが更新する。
  private clipboardSync = DEFAULT_SETTINGS.clipboard_sync;
  get clipboardSyncEnabled(): boolean {
    return this.clipboardSync;
  }
  setClipboardSyncEnabled(enabled: boolean): void {
    this.clipboardSync = enabled;
    void setClipboardSync(this.id, enabled).catch((e) => logger.warn("setClipboardSync failed", e));
    this.host.toast(this, enabled ? "クリップボード同期を再開" : "クリップボード同期を停止");
  }

  /** 窓リサイズ (アクティブ Session のみ orchestrator が呼ぶ)。 */
  onWindowResize(): void {
    if (this.connected) this.scheduleSendViewport();
  }

  /**
   * エラー画面の「再接続」ボタン / タブメニューの再接続。自動再接続の打ち切り後でも
   * 試行カウンタをリセットして即時にやり直す (待機バックオフは挟まない)。
   */
  manualReconnect(): void {
    if (this.connected) return;
    logger.info(`manual reconnect: ${this.entry.host}`);
    this.cancelReconnect();
    this.reconnecting = true;
    void this.attemptReconnect();
  }

  /**
   * 設定保存 (`aurora://settings-changed`) を受けて fps/ビットレート上限を接続中のホストへ
   * 即時反映する (A-3: 再接続不要で効かせる)。ホスト別オーバーライドが付くフィールドは
   * 対象外にする: オーバーライドは接続毎の固定値で、グローバル変更に追従させると
   * ツールバーで変えた直後の persist で値が巻き戻る。差分が無ければ何も送らない。
   */
  refreshVideoParamsFromSettings(): void {
    if (!this.connected) return;
    const g = this.host.getSettings();
    const o = this.entry.overrides;
    const fps = o?.fps != null ? this.videoFps : g.fps;
    const mbps = o?.max_mbps != null ? this.videoMbps : g.max_mbps;
    if (fps === this.videoFps && mbps === this.videoMbps) return;
    this.videoFps = fps;
    this.videoMbps = mbps;
    void setVideoParams(this.id, fps, mbps * 1_000_000);
    if (this.active) this.host.toolbar.setVideoParams(fps, mbps);
  }

  /** アクティブ化時に共有 UI を自分の状態へ揃える。 */
  syncSharedUi(): void {
    this.host.renderStatus(this);
    // 静かな再接続バッジは全 Session で 1 つの DOM を共有するため、タブ切替のたびに
    // 自分の状態 (表示中/非表示) へ揃え直す (他タブのバッジが残留/消失しないようにする)。
    if (this.reconnectBadgeText) this.host.reconnectBadge(this, this.reconnectBadgeText);
    else this.host.clearReconnectBadge(this);
    if (!this.connected) return;
    const tb = this.host.toolbar;
    // 別タブで切断が起きて toolbar が connected=false にされていても、健全なタブへ
    // 切り替えたら確実に表示状態へ戻す。
    tb.setConnected(true, this.host.getSettings().toolbar_pinned);
    tb.setDisplays(this.toolbarDisplayList(), this.currentDisplay?.id);
    tb.setConnLabel(this.connLabel);
    // ミュートと再生音量も必ず復元する。モバイルは activate() が start() の後に来るため
    // onHostInfo の `if (this.active)` ブロックが初回接続では丸ごと飛び、この syncSharedUi が
    // 唯一の初期化経路になる。ここで渡さないとミュートは解除・音量は 100% に戻ってしまう。
    tb.setAudioAvailable(this.audioAvailable, this.audioMuted);
    tb.setAudioVolume(this.host.getSettings().audio_volume);
    tb.setClipboardSyncState(this.clipboardSync);
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
    if (kind === "connecting" || kind === "pending") this.tabStatus = "connecting";
    else if (kind === "error") this.tabStatus = "error";
    this.host.refreshTab(this);
    this.host.renderStatus(this);
  }

  /** 仮想ディスプレイのテイクオーバー待ちを終える (display_resized 到達 or タイムアウト保険)。 */
  private finishPreparingVirtual(): void {
    if (!this.preparingVirtual) return;
    this.preparingVirtual = false;
    if (this.prepareTimer !== null) {
      clearTimeout(this.prepareTimer);
      this.prepareTimer = null;
    }
    // 接続済みなら中央オーバーレイを消して映像を見せる。
    if (this.connected) this.setStatus("", "hidden");
  }

  /** 脱出方法ヒントを表示済みか (全 Session 共有)。初回キャプチャ時だけ案内する。 */
  private static breakoutHintShown = false;

  private onCaptureChange(captured: boolean): void {
    if (this.active) this.host.toolbar.setCaptureToggled(captured);
    // マウスキャプチャ中だけホストにカーソルを映像へ焼き込ませる (絶対座標モードはローカル
    // カーソルを使うので不要・二重表示防止)。ポインタロックの実状態に同期させる。
    void setCursorOverlay(this.id, captured).catch((e) => logger.warn("setCursorOverlay failed", e));
    // Win/Alt+Tab 等の OS ショートカットがクライアント OS に奪われるとホストへ届かないため、
    // マウスキャプチャ (ポインタロック) と連動してキーボード占有を切り替える。設定で OFF にした
    // 場合は ON 側の呼び出しだけ抑止する (OFF 側は残留防止のため captured の値によらず必ず呼ぶ)。
    if (!captured || this.effectiveSettings().grab_keyboard) {
      void setKeyboardGrab(captured).catch((e) => logger.warn("setKeyboardGrab failed", e));
    }
    if (!captured) {
      this.host.toast(this, "マウスキャプチャ OFF");
      return;
    }
    // 初回キャプチャ時だけ脱出方法を案内する。フルスクリーン中は単押し Esc がホストへ流れて
    // 抜けられないため、Esc 2 連打で戻れることを知らせる (以降は短い通知のみ)。
    if (Session.breakoutHintShown) {
      this.host.toast(this, "マウスキャプチャ ON");
    } else {
      Session.breakoutHintShown = true;
      this.host.toast(this, "マウスキャプチャ ON — Esc を素早く2回でマウス解放・全画面解除");
    }
  }

  /** グローバル設定にホスト別オーバーライド (entry.overrides) を重ねた実効設定。 */
  private effectiveSettings(): ClientSettings {
    const s = this.host.getSettings();
    const o = this.entry.overrides;
    if (!o) return s;
    return {
      ...s,
      use_virtual_display: o.use_virtual_display ?? s.use_virtual_display,
      fps: o.fps ?? s.fps,
      max_mbps: o.max_mbps ?? s.max_mbps,
      codec: o.codec ?? s.codec,
    };
  }

  /**
   * 申告する映像コーデック集合を実効設定から決める。"auto" はプローブで対応する全コーデックを
   * 優先順で申告する。手動選択がブラウザ非対応なら h264 へ落として警告トーストを出す。
   */
  private async decideSupportedCodecs(): Promise<CodecId[]> {
    const choice = this.effectiveSettings().codec;
    const supported = await probeSupportedCodecs();
    if (choice === "auto") return supported.length > 0 ? supported : (["h264"] as CodecId[]);
    if (supported.includes(choice)) return [choice];
    this.host.toast(this, `${choice.toUpperCase()} は非対応のため H.264 を使用します`, "error");
    return ["h264"];
  }

  /**
   * 設定変更 (グローバル設定 or ホスト別 override) を受けて申告コーデックを再計算し、接続中かつ
   * 前回申告と異なる場合だけ `set_supported_codecs` を送る (毎回送ってホストにエンコーダを
   * 作り直させない)。
   */
  async refreshCodecFromSettings(): Promise<void> {
    if (!this.connected) return;
    const codecs = await this.decideSupportedCodecs();
    if (sameCodecs(codecs, this.declaredCodecs)) return;
    this.declaredCodecs = codecs;
    try {
      await setSupportedCodecs(this.id, codecs);
    } catch (e) {
      logger.warn("setSupportedCodecs failed", e);
    }
  }

  private toolbarDisplayList(): DisplayDto[] {
    if (this.effectiveSettings().use_virtual_display) {
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
      const codecs = await this.decideSupportedCodecs();
      this.declaredCodecs = codecs;
      const outcome = await connectToHost(
        this.id,
        this.entry.host,
        this.entry.port,
        save,
        event,
        video,
        codecs,
      );
      if (outcome.status === "pending_approval") this.enterPendingApproval(outcome.fingerprint);
      else this.clearPending();
    } finally {
      this.connectInFlight = false;
    }
  }

  // --- デバイスピン留め承認待ち ---

  /** host のペアリング窓中で承認待ちになった。待機画面を出し、固定間隔で自動再試行する。 */
  private enterPendingApproval(fingerprint: string): void {
    // 通常の自動再接続バックオフと二重に走らないよう止める。
    this.cancelReconnect();
    this.pendingApproval = true;
    this.connected = false;
    const msg = `ホストの承認待ちです。\nホスト側でこの端末を承認してください。\n端末ID: ${fingerprint}`;
    this.setStatus(msg, "pending", true);
    this.schedulePendingRetry();
  }

  /** 承認待ちループを止める (承認成立/ユーザーキャンセル/資源解放時)。 */
  private clearPending(): void {
    this.pendingApproval = false;
    if (this.pendingTimer !== null) {
      clearTimeout(this.pendingTimer);
      this.pendingTimer = null;
    }
  }

  private schedulePendingRetry(): void {
    if (this.pendingTimer !== null) clearTimeout(this.pendingTimer);
    this.pendingTimer = window.setTimeout(() => void this.attemptPendingRetry(), PENDING_RETRY_MS);
  }

  private async attemptPendingRetry(): Promise<void> {
    this.pendingTimer = null;
    if (!this.pendingApproval) return;
    try {
      // pending が続けば openSession 内で enterPendingApproval が再スケジュールする。
      // 承認済みなら host_info (onHostInfo) が clearPending して接続完了 UI へ引き継ぐ。
      await this.openSession(false);
    } catch (e) {
      if (!this.pendingApproval) return;
      // 拒否/ペアリング窓終了などで例外になった場合は通常のエラー画面へ落とす。
      this.pendingApproval = false;
      this.setStatus(`接続に失敗しました: ${String(e)}`, "error", true);
    }
  }

  private handleClientEvent(ev: ClientEvent, gen: number): void {
    if (gen !== this.sessionGen) return; // 旧セッションの残イベントは無視
    switch (ev.kind) {
      case "connected":
        this.setStatus("handshake complete…", "connecting", true);
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
        // テイクオーバー完了の合図: 準備中オーバーレイを消して映像を見せる。
        if (this.preparingVirtual && this.currentDisplay?.id === ev.id) this.finishPreparingVirtual();
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
      case "video_codec_changed":
        this.currentCodec = ev.codec;
        this.decoder.setCodec(ev.codec);
        logger.info(`video codec changed: ${this.entry.host} -> ${ev.codec}`);
        break;
      case "pong":
        break;
      case "file_progress":
        this.host.fileProgress(this, ev.id, ev.done, ev.total);
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
    // currentDisplay もこの下で上書きされるため、復元用に切断前の選択を先に控える。
    const wasReconnecting = this.reconnecting;
    const prevDisplay = this.currentDisplay;
    this.markSessionEstablished();
    this.clearPending();
    this.connected = true;
    this.tabStatus = "connected";
    if (this.entry.host) void saveRecentHost(this.entry);
    const settings = this.effectiveSettings();
    this.audioAvailable = ev.audio;
    // Rust 側ゲートは接続時に設定値で初期化済み。UI 側の現在値も同じ出所へ揃える。
    this.clipboardSync = settings.clipboard_sync;
    this.currentDisplays = ev.displays;

    let target = ev.displays[0];
    if (settings.use_virtual_display) {
      const vd = ev.displays.find((dd) => dd.is_virtual);
      if (vd) {
        target = vd;
      } else {
        this.host.toast(this, "仮想ディスプレイが見つかりません (ホストに VDD 未導入?)", "error");
      }
    } else if (wasReconnecting && prevDisplay && !prevDisplay.is_virtual) {
      // 再接続成立時、切断前に選んでいた物理ディスプレイが既定 (displays[0]) と異なる非デフォルト
      // 選択なら復元する。仮想ディスプレイのテイクオーバーは上の use_virtual_display 分岐 (常設定
      // ベース) が独立に担うため、ここは物理の非デフォルト選択のみを対象にする。
      const restored = ev.displays.find((dd) => dd.id === prevDisplay.id && !dd.is_virtual);
      if (restored && restored.id !== target?.id) target = restored;
    }
    this.currentDisplay = target ?? null;
    // 仮想ディスプレイ対象時、host はこの直後にテイクオーバー (物理→仮想切替、数秒) を行う。
    // 完了までは物理側の固まった映像が映り込むため、display_resized (完了合図) までオーバーレイで隠す。
    if (settings.use_virtual_display && target?.is_virtual) {
      this.preparingVirtual = true;
      this.setStatus("仮想ディスプレイを準備中…", "connecting", true);
      if (this.prepareTimer !== null) clearTimeout(this.prepareTimer);
      // テイクオーバー失敗などで display_resized が来ない場合の保険。
      this.prepareTimer = window.setTimeout(() => {
        this.prepareTimer = null;
        this.finishPreparingVirtual();
      }, 10000);
    } else {
      this.setStatus("", "hidden");
    }
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
    // 設定でファイル共有が ON なら、この接続で This PC 全体を公開する (既定 OFF)。
    // 各セッションが自分の id で適用し、ホスト側は所有接続かつ登録端末のときだけドライブを mount する。
    if (settings.share_files) {
      void setFileSharing(this.id, true, settings.share_files_writable).catch((e) =>
        logger.warn("setFileSharing failed", e),
      );
    }
    // 接続時に音声をミュートで始める設定なら、ホストへも初期状態として送る (ツールバー表示は
    // 下の setAudioAvailable(initialMuted) / syncSharedUi 側で揃える)。
    this.audioMuted = settings.audio_muted;
    if (settings.audio_muted) {
      void setAudioMuted(this.id, true).catch((e) => logger.warn("setAudioMuted failed", e));
    }
    // 再生音量も接続直後に設定値で初期適用する (ホストへは送らない client 内部ゲイン)。
    void setAudioVolume(this.id, settings.audio_volume).catch((e) => logger.warn("setAudioVolume failed", e));
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
      tb.setAudioAvailable(ev.audio, settings.audio_muted);
      tb.setAudioVolume(settings.audio_volume);
      tb.setClipboardSyncState(this.clipboardSync);
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
    // タブは残り自動/手動再接続が同じ session_id で connect_to_host を通るため、資格情報は保持する。
    void disconnect(this.id, false);
    this.connected = false;
    if (this.viewportTimer !== null) {
      clearTimeout(this.viewportTimer);
      this.viewportTimer = null;
    }
    if (this.prepareTimer !== null) {
      clearTimeout(this.prepareTimer);
      this.prepareTimer = null;
    }
    this.preparingVirtual = false;
    this.lastSentVw = 0;
    this.lastSentVh = 0;
    this.detachInput();
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
    // "online" リスナを外さないと Session (と decoder/input の参照グラフ) が GC されず、
    // 単一 WebView 常駐のモバイルではタブ開閉/ホスト切替ごとに単調増加してしまう。
    window.removeEventListener("online", this.onNetworkOnline);
    this.disconnectHandled = true;
    this.cancelReconnect();
    this.clearPending();
    this.connected = false;
    if (this.viewportTimer !== null) {
      clearTimeout(this.viewportTimer);
      this.viewportTimer = null;
    }
    if (this.prepareTimer !== null) {
      clearTimeout(this.prepareTimer);
      this.prepareTimer = null;
    }
    this.preparingVirtual = false;
    this.detachInput();
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
    this.reconnectBadgeText = "";
    this.host.clearReconnectBadge(this);
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
    this.reconnectBadgeText = "";
    this.host.clearReconnectBadge(this);
  }

  private startReconnect(): void {
    if (this.reconnecting && this.reconnectTimer !== null) return;
    this.reconnecting = true;
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
    const delay = RECONNECT_DELAYS_MS[Math.min(this.reconnectAttempt, RECONNECT_DELAYS_MS.length - 1)];
    const nextAttempt = this.reconnectAttempt + 1;
    const text = `再接続待機 ${Math.round(delay / 1000)}s… (試行 ${nextAttempt})`;
    this.showReconnectProgress(nextAttempt, text);
    this.reconnectTimer = window.setTimeout(() => void this.attemptReconnect(), delay);
  }

  /**
   * 再接続の進行状況を表示する。試行 `QUIET_RECONNECT_ATTEMPTS` 回目までは最後の映像を隠さず
   * 常駐バッジのみで知らせ (静かな再接続)、それ以降は従来どおりの全面オーバーレイへ格上げする。
   * タブのドット (`tabStatus`) は常に "connecting" にする (バッジのみの間も実状態を反映する)。
   */
  private showReconnectProgress(attempt: number, text: string): void {
    this.tabStatus = "connecting";
    this.host.refreshTab(this);
    if (attempt <= QUIET_RECONNECT_ATTEMPTS) {
      if (this.status !== "hidden") this.setStatus("", "hidden");
      this.reconnectBadgeText = text;
      this.host.reconnectBadge(this, text);
    } else {
      this.reconnectBadgeText = "";
      this.host.clearReconnectBadge(this);
      this.setStatus(text, "connecting", true);
    }
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
    this.showReconnectProgress(this.reconnectAttempt, `再接続中… (試行 ${this.reconnectAttempt})`);
    // 静かな再接続 (バッジのみ) の間はトーストも出さない。全面オーバーレイへ格上げしてから
    // トーストで目立たせる (ユーザーが最後の映像を見失っていない前提が崩れたタイミング)。
    if (this.reconnectAttempt > QUIET_RECONNECT_ATTEMPTS) {
      this.host.toast(this, `再接続を試行中… (${this.reconnectAttempt})`);
    }
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
