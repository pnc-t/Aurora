// モバイル共通の「引っ張って更新」。スクロール領域の上端 (scrollTop <= 0) からの
// 下方向ドラッグだけを検知し、通常のスクロールは一切邪魔しない。
// 既存コードが全て Pointer Events で統一されているため touch* ではなく pointer* を使う。
//
// インジケータのスタイルは要素の style プロパティで直接当てる方式にした (専用 css ファイルを
// 作らない)。理由: このモジュールは任意の scroller に対して DOM を動的に生成/破棄する汎用
// アタッチメント関数であり、mobile-sheet.ts のように「常にマウントされた 1 画面」を持つ
// コンポーネントではない。co-locate css ファイルを作ると import 副作用でグローバル CSS に
// 常時登録されるが、インジケータの見た目は円形スピナー + 位置決めのみで CSS 変数を使った
// 複雑なスタイリングが不要なため、要素生成時に inline style で完結させる方が
// 依存を増やさずシンプルになる。

const THRESHOLD_PX = 64;
const MAX_PULL_PX = 96;
// この距離を超えて動いたら「引っ張り操作」とみなす (タップと区別する最小移動量)。
const GESTURE_PX = 8;

/** スクロール領域の上端でのドラッグ引き下げに「更新」を割り当てる。戻り値を呼ぶと解除できる。 */
export function attachPullToRefresh(
  scroller: HTMLElement,
  onRefresh: () => Promise<void>,
): () => void {
  // インジケータ本体 (円形スピナー)。scroller の直前 (兄弟) に固定配置し、
  // 引き下げ量に応じて transform で追従させる。
  const indicator = document.createElement("div");
  indicator.setAttribute("aria-hidden", "true");
  Object.assign(indicator.style, {
    position: "absolute",
    top: "0",
    left: "50%",
    width: "28px",
    height: "28px",
    marginLeft: "-14px",
    marginTop: "-32px",
    borderRadius: "50%",
    border: "2px solid var(--panel-border)",
    borderTopColor: "var(--accent)",
    background: "var(--panel)",
    boxShadow: "0 2px 6px var(--shadow)",
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    opacity: "0",
    pointerEvents: "none",
    transition: "opacity 0.15s ease-out",
    zIndex: "5",
  } satisfies Partial<CSSStyleDeclaration>);

  // scroller 自身を position 基準にする (既存の position を尊重し、static のときだけ補う)。
  const prevPosition = scroller.style.position;
  if (getComputedStyle(scroller).position === "static") {
    scroller.style.position = "relative";
  }
  scroller.prepend(indicator);

  let pulling = false;
  let refreshing = false;
  let startY = 0;
  let pullDist = 0;
  let pointerId: number | null = null;
  // GESTURE_PX を超えて動いたことがあるドラッグかどうか。指を離した直後、この一覧を持つ
  // カード側 (Hosts/WoL) が pointerup に続けて発火する click で「接続」「起こして接続」等の
  // 重い操作を実行してしまうのを防ぐために使う。上端 (scrollTop<=0) では実際にスクロールが
  // 発生しないため、ブラウザが通常のスクロール操作で行う click 抑止に頼れない
  // (スクロールが起きた形跡が無いので、ブラウザ視点では「その場でのタップ」と区別が付かない)。
  // そのため自前で次の click を 1 回だけ capture フェーズで止める。
  let gestureFired = false;
  let suppressClickTarget: HTMLElement | null = null;

  function onSuppressClick(e: MouseEvent): void {
    e.stopPropagation();
    e.preventDefault();
    (suppressClickTarget ?? scroller).removeEventListener("click", onSuppressClick, true);
    suppressClickTarget = null;
  }

  /** 指を離した直後に発火する click を 1 回だけ捕捉して止める。閾値未満のタップでは呼ばない。 */
  function armClickSuppression(): void {
    // 既に予約済みなら二重登録しない (連続した pointerup/pointercancel を想定した保険)。
    if (suppressClickTarget) return;
    suppressClickTarget = scroller;
    scroller.addEventListener("click", onSuppressClick, true);
  }

  function setIndicatorPull(dist: number): void {
    const clamped = Math.min(dist, MAX_PULL_PX);
    indicator.style.opacity = clamped > 4 ? String(Math.min(1, clamped / THRESHOLD_PX)) : "0";
    indicator.style.transform = `translateY(${clamped}px) rotate(${clamped * 3}deg)`;
  }

  function setIndicatorSpinning(spinning: boolean): void {
    indicator.style.opacity = spinning ? "1" : "0";
    indicator.style.transform = `translateY(${THRESHOLD_PX * 0.6}px)`;
    indicator.style.animation = spinning ? "ptr-spin 0.8s linear infinite" : "none";
  }

  function resetIndicator(): void {
    indicator.style.opacity = "0";
    indicator.style.transform = "translateY(0)";
    indicator.style.animation = "none";
  }

  function onPointerDown(e: PointerEvent): void {
    if (refreshing) return;
    if (scroller.scrollTop > 0) return;
    // 呼び出し側の一覧描画 (mobile-wol.ts の renderTargets / mobile-launcher.ts の renderHosts)
    // は listEl.replaceChildren() で子要素を全消しして作り直すため、scroller の子として
    // prepend したインジケータも巻き添えで外れてしまう (マウント直後の初回描画で即座に消える)。
    // 外れたまま気付かず放置されると「ジェスチャは効くがスピナーだけ出ない」という見えにくい
    // 症状になるため、ジェスチャ開始のたびに接続状態を確認し、外れていれば付け直す。
    if (!indicator.isConnected) scroller.prepend(indicator);
    pulling = true;
    pointerId = e.pointerId;
    startY = e.clientY;
    pullDist = 0;
    gestureFired = false;
  }

  function onPointerMove(e: PointerEvent): void {
    if (!pulling || e.pointerId !== pointerId) return;
    if (scroller.scrollTop > 0) {
      // ドラッグ中にスクロールが発生した (別要因で上端を離れた) 場合は引っ張り判定を打ち切る。
      // 実スクロールが起きた以上、以降の click 抑止はブラウザ本来の挙動に任せてよい。
      pulling = false;
      resetIndicator();
      return;
    }
    const dy = e.clientY - startY;
    // 上端では実スクロールが起きないため、移動量に関わらずここで一度 GESTURE_PX を
    // 超えていれば「タップではなくジェスチャ」と記録しておく (上方向の揺れも含む)。
    if (Math.abs(dy) > GESTURE_PX) gestureFired = true;
    if (dy <= 0) {
      pullDist = 0;
      resetIndicator();
      return;
    }
    pullDist = dy;
    setIndicatorPull(dy);
  }

  function endPull(): void {
    if (!pulling) return;
    pulling = false;
    const dist = pullDist;
    pullDist = 0;
    pointerId = null;
    if (dist > THRESHOLD_PX && !refreshing) {
      refreshing = true;
      setIndicatorSpinning(true);
      void onRefresh().finally(() => {
        refreshing = false;
        resetIndicator();
      });
    } else {
      resetIndicator();
    }
  }

  function onPointerUp(e: PointerEvent): void {
    if (e.pointerId !== pointerId) return;
    const shouldSuppressClick = gestureFired;
    gestureFired = false;
    endPull();
    // 閾値未満のタップ (gestureFired=false) では何も抑止しない。通常のカードタップを壊さないため。
    if (shouldSuppressClick) armClickSuppression();
  }

  function onPointerCancel(e: PointerEvent): void {
    if (e.pointerId !== pointerId) return;
    const shouldSuppressClick = gestureFired;
    gestureFired = false;
    pulling = false;
    pullDist = 0;
    pointerId = null;
    if (!refreshing) resetIndicator();
    if (shouldSuppressClick) armClickSuppression();
  }

  scroller.addEventListener("pointerdown", onPointerDown);
  scroller.addEventListener("pointermove", onPointerMove);
  scroller.addEventListener("pointerup", onPointerUp);
  scroller.addEventListener("pointercancel", onPointerCancel);

  // スピナー回転用の keyframes をグローバルに 1 度だけ注入する (専用 css ファイルは作らない方針のため)。
  // translateY は setIndicatorSpinning と同じ位置 (THRESHOLD_PX * 0.6) に固定して回転だけ animate する。
  if (!document.getElementById("ptr-keyframes")) {
    const style = document.createElement("style");
    style.id = "ptr-keyframes";
    const y = THRESHOLD_PX * 0.6;
    style.textContent = `@keyframes ptr-spin { to { transform: translateY(${y}px) rotate(360deg); } }`;
    document.head.appendChild(style);
  }

  return () => {
    scroller.removeEventListener("pointerdown", onPointerDown);
    scroller.removeEventListener("pointermove", onPointerMove);
    scroller.removeEventListener("pointerup", onPointerUp);
    scroller.removeEventListener("pointercancel", onPointerCancel);
    // 予約済みの click 抑止が残っていれば (発火せずに解除された場合) 必ず外す。
    if (suppressClickTarget) {
      suppressClickTarget.removeEventListener("click", onSuppressClick, true);
      suppressClickTarget = null;
    }
    indicator.remove();
    scroller.style.position = prevPosition;
  };
}
