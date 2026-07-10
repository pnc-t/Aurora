//! ウィンドウ/セッションのライフサイクル tauri コマンド (capture ウィンドウ生成・受け渡し・接続一覧)。
use super::*;

/// launcher からの接続をタブとして追加してもらうための通知ペイロード (`aurora://add-tab`)。
/// `open_capture_window` が既存 capture 窓へルーティングする際に emit する。
#[derive(Clone, serde::Serialize)]
struct AddTabPayload {
    session_id: String,
    host: String,
    port: u16,
    label: String,
    save: bool,
}

/// タブのドラッグ合流の通知ペイロード (`aurora://adopt-tab`)。移譲先窓へ「この session_id を
/// `take_handoff` で引き取れ」と伝える。
#[derive(Clone, serde::Serialize)]
struct AdoptTabPayload {
    session_id: String,
}

/// 接続中 host のタブアクティブ化通知 (`aurora://activate-host`)。`focus_host_window` が
/// 前面化した窓へ「この host のタブへ切り替えろ」と伝える。
#[derive(Clone, serde::Serialize)]
struct ActivateHostPayload {
    host: String,
}

/// launcher からの接続を流し込む先を決める。直近フォーカスした capture 窓が生存していれば
/// それを優先し、無ければ生存している capture 窓を (順序不定で) 1 つ選ぶ。1 つも無ければ
/// None を返し、呼び出し側が従来どおり新窓を生成する。
fn route_target(app: &tauri::AppHandle, state: &SessionState) -> Option<String> {
    if let Ok(g) = state.last_focused_capture.lock()
        && let Some(label) = g.as_deref()
        && app.get_webview_window(label).is_some()
    {
        return Some(label.to_string());
    }
    // フォーカス追跡が無い/死んでいる場合のフォールバック。webview_windows() の順序は
    // HashMap 由来で不定だが、生きている capture 窓のどれかへ合流できれば新窓乱立は防げる。
    app.webview_windows()
        .keys()
        .find(|l| l.starts_with("capture-"))
        .cloned()
}

/// capture ウィンドウを Rust 側で生成する (JS にウィンドウ生成権限を与えない)。
/// 接続資格情報 (password) は staging マップへ積み、URL には session/host/port/save のみ
/// 載せる (平文をウィンドウ URL に出さない)。生成した session_id を返す。
/// 既存の capture 窓があれば新窓は作らず、そちらへタブ追加を通知する (`route_target`)。
/// `new_window` が true ならこのルーティングをスキップし常に新しいウィンドウを作る
/// (ランチャーの「新しいウィンドウで接続」)。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn open_capture_window(
    app: tauri::AppHandle,
    host: String,
    port: u16,
    // タブの表示名 (空なら capture 側で host にフォールバック)。
    label: String,
    save: bool,
    // 手入力パスワード (今回だけ/保存) は Some、保存済み即接続/再接続は None。
    password: Option<String>,
    new_window: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<String, String> {
    let n = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let session_id = format!("s{n}");

    if !new_window && let Some(target_label) = route_target(&app, &state) {
        state
            .staging
            .lock()
            .await
            .insert(session_id.clone(), password.clone());
        let payload = AddTabPayload {
            session_id: session_id.clone(),
            host: host.clone(),
            port,
            label: label.clone(),
            save,
        };
        if app
            .emit_to(&target_label, "aurora://add-tab", payload)
            .is_ok()
        {
            return Ok(session_id);
        }
        // 窓は生きているが emit できない異常時は、接続不能にしないため新窓生成へフォールバックする。
        state.staging.lock().await.remove(&session_id);
    }

    let win_label = format!("capture-{session_id}");
    // パスワードを staging へ積んでから window を作る (connect_to_host が pop する)。
    state
        .staging
        .lock()
        .await
        .insert(session_id.clone(), password);
    let url = format!(
        "capture.html?session={}&host={}&port={port}&label={}&save={}",
        q(&session_id),
        q(&host),
        q(&label),
        if save { 1 } else { 0 },
    );
    let build = WebviewWindowBuilder::new(&app, &win_label, WebviewUrl::App(url.into()))
        .title("Aurora")
        .inner_size(1280.0, 800.0)
        .resizable(true)
        .decorations(false)
        .build();
    if let Err(e) = build {
        // 生成に失敗したら積んだ資格情報を残さない。
        state.staging.lock().await.remove(&session_id);
        return Err(format!("open capture window: {e}"));
    }
    Ok(session_id)
}

/// 既存の capture ウィンドウ内で新しいタブ (セッション) を開く準備をする。ウィンドウは
/// 作らず、session_id を発番して接続資格情報 (password) を staging へ積むだけ。呼び出した
/// capture ウィンドウの JS が、返った id で `connect_to_host` を呼んで実接続する。
/// 平文パスワードを JS のウィンドウ間でやり取りしないための受け渡し (open_capture_window と対)。
#[tauri::command]
pub async fn stage_session(
    // 手入力パスワード (今回だけ/保存) は Some、保存済み即接続は None。
    password: Option<String>,
    state: tauri::State<'_, SessionState>,
) -> Result<String, String> {
    let n = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let session_id = format!("s{n}");
    state
        .staging
        .lock()
        .await
        .insert(session_id.clone(), password);
    Ok(session_id)
}

/// 既存セッションを別ウィンドウへ移譲するための空ウィンドウを開く (Phase 3 ドラッグ分割)。
/// networking は止めず、表示先 (sink) を即デタッチして所有ウィンドウを生成先の新ラベルへ
/// 付け替える。これを **ウィンドウ生成より前に** 行うことで、移譲元ウィンドウが直後に
/// (最後のタブが消えて) 閉じても、その Destroyed ハンドラが移譲中セッションを巻き込んで
/// 停止しない (window_label が既に新ラベルになっている)。`snapshot` は移譲先で `take_handoff`
/// が取り出す表示状態 JSON。`x`/`y` は新ウィンドウの配置 (カーソル位置、論理 px)。
#[tauri::command]
pub async fn open_session_window(
    app: tauri::AppHandle,
    session_id: String,
    snapshot: String,
    x: Option<f64>,
    y: Option<f64>,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let n = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let label = format!("capture-move{n}");
    // 所有権 (window_label) を新ラベルへ移し、表示先を外す。移譲元が閉じても巻き込まれない。
    {
        let mut guard = state.inner.lock().await;
        let s = guard.get_mut(&session_id).ok_or("unknown session")?;
        s.window_label = label.clone();
        s.sink.clear();
    }
    state
        .adopt
        .lock()
        .await
        .insert(session_id.clone(), snapshot);
    let url = format!("capture.html?adopt={}", q(&session_id));
    let mut build = WebviewWindowBuilder::new(&app, &label, WebviewUrl::App(url.into()))
        .title("Aurora")
        .inner_size(1280.0, 800.0)
        .resizable(true)
        .decorations(false);
    if let (Some(x), Some(y)) = (x, y) {
        build = build.position(x, y);
    }
    if let Err(e) = build.build() {
        // 生成失敗時は積んだ引き継ぎデータを残さない。セッションは sink デタッチ済みのまま
        // 残る (元ウィンドウは既にタブを外している可能性があるため、ここでは停止しない)。
        state.adopt.lock().await.remove(&session_id);
        return Err(format!("open session window: {e}"));
    }
    Ok(())
}

/// 移譲先ウィンドウが引き継ぎデータ (表示状態 JSON) を取り出す。1 回限り (pop)。
#[tauri::command]
pub async fn take_handoff(
    session_id: String,
    state: tauri::State<'_, SessionState>,
) -> Result<Option<String>, String> {
    Ok(state.adopt.lock().await.remove(&session_id))
}

/// 既存セッションの表示先 (映像/イベント Channel) を、この呼び出し元 webview のものへ
/// 差し替える (Phase 3 移譲先がアタッチ)。所有ウィンドウもこの webview へ付け替える。
/// networking はそのままなので、移譲先は別途 `request_keyframe` で映像を復帰させる。
#[tauri::command]
pub async fn attach_session(
    window: WebviewWindow,
    session_id: String,
    event_channel: Channel<ClientEvent>,
    video_channel: Channel<InvokeResponseBody>,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let mut guard = state.inner.lock().await;
    let s = guard.get_mut(&session_id).ok_or("unknown session")?;
    s.sink.set(event_channel, video_channel);
    s.window_label = window.label().to_string();
    Ok(())
}

/// staging へ積んだ未使用の接続資格情報を破棄する。ルーティング先で同一 host のタブが
/// 既にあり接続しなかった場合など、平文パスワードをメモリに残さないための後始末。
#[tauri::command]
pub async fn unstage_session(
    session_id: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    state.staging.lock().await.remove(&session_id);
    Ok(())
}

/// ドロップ点 (論理 px、スクリーン座標) を含む「自分以外の capture ウィンドウ」のラベルを
/// 返す。タブのドラッグ合流のヒットテスト。z-order は考慮しない (重なりは不定) — ドラッグ
/// 合流はカーソル直下の窓を大まかに当てられれば十分で、厳密な最前面判定までは不要なため。
#[tauri::command]
pub async fn find_capture_window_at(
    window: WebviewWindow,
    app: tauri::AppHandle,
    x: f64,
    y: f64,
) -> Result<Option<String>, String> {
    let self_label = window.label().to_string();
    for (label, w) in app.webview_windows() {
        if label == self_label || !label.starts_with("capture-") {
            continue;
        }
        if w.is_minimized().unwrap_or(true) {
            continue;
        }
        let Ok(scale) = w.scale_factor() else {
            continue;
        };
        let Ok(pos) = w.outer_position() else {
            continue;
        };
        let Ok(size) = w.outer_size() else {
            continue;
        };
        let pos = pos.to_logical::<f64>(scale);
        let size = size.to_logical::<f64>(scale);
        if x >= pos.x && x < pos.x + size.width && y >= pos.y && y < pos.y + size.height {
            return Ok(Some(label));
        }
    }
    Ok(None)
}

/// 既存セッションを「既存の別 capture ウィンドウ」へ移譲する (タブのドラッグ合流)。
/// `open_session_window` の姉妹コマンド: 新窓は作らず、所有権付替え + sink デタッチ + 引き継ぎ
/// JSON を積んでから移譲先へ adopt を通知する。
#[tauri::command]
pub async fn handoff_session_to_window(
    app: tauri::AppHandle,
    session_id: String,
    target_label: String,
    snapshot: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    if app.get_webview_window(&target_label).is_none() {
        return Err("target window not found".into());
    }
    // 所有権 (window_label) を移譲先へ移し、表示先を外す (open_session_window と同じ理由:
    // 通知より前に所有権を移すことで、移譲元の Destroyed ハンドラに巻き込まれない)。
    {
        let mut guard = state.inner.lock().await;
        let s = guard.get_mut(&session_id).ok_or("unknown session")?;
        s.window_label = target_label.clone();
        s.sink.clear();
    }
    state
        .adopt
        .lock()
        .await
        .insert(session_id.clone(), snapshot);
    let payload = AdoptTabPayload {
        session_id: session_id.clone(),
    };
    if app
        .emit_to(&target_label, "aurora://adopt-tab", payload)
        .is_err()
    {
        // 通知できなければ引き継ぎデータを残さない (open_session_window の失敗時と同様の後始末)。
        state.adopt.lock().await.remove(&session_id);
        return Err("failed to notify target window".into());
    }
    Ok(())
}

/// このセッションのファイル共有を有効/無効にする。有効時は This PC 全体 (全ドライブ) を
/// ホストの仮想ドライブへ公開する。設定 (launcher) のトグルに対応し、接続成立時に session が呼ぶ。
#[tauri::command]
pub async fn set_file_sharing(
    session_id: String,
    enabled: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let scope = {
        let guard = state.inner.lock().await;
        let s = guard.get(&session_id).ok_or("unknown session")?;
        s.share_scope.clone()
    };
    *scope.lock().await = if enabled {
        ShareScope::WholePc
    } else {
        ShareScope::Disabled
    };
    tracing::info!(session = %session_id, enabled, "file sharing set");
    Ok(())
}

/// 接続中セッションの host を重複排除して返す。
fn distinct_hosts(map: &HashMap<String, SessionHandle>) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for h in map.values() {
        set.insert(h.host.clone());
    }
    set.into_iter().collect()
}

/// 現在接続中の host 一覧を全ウィンドウへ通知する (`aurora://connections`)。ランチャーが
/// これを購読して接続中表示・切断ボタンの出し分け・保存バッジの再読込を行う。セッションの
/// 追加 (connect) / 除去 (disconnect・ウィンドウ破棄) のたびに呼ぶ。
pub(crate) async fn emit_connections(app: &tauri::AppHandle, state: &SessionState) {
    let hosts = distinct_hosts(&*state.inner.lock().await);
    let _ = app.emit("aurora://connections", hosts);
}

/// ランチャー初期表示用: 現在接続中の host 一覧 (重複排除)。
#[tauri::command]
pub async fn connected_hosts(state: tauri::State<'_, SessionState>) -> Result<Vec<String>, String> {
    Ok(distinct_hosts(&*state.inner.lock().await))
}

/// launcher からの接続中カードクリック: 指定 host のセッションを表示している capture
/// ウィンドウを前面化し、該当タブのアクティブ化を通知する。見つからなければ false
/// (launcher 側が接続中表示の取り直しにフォールバックする)。
#[tauri::command]
pub async fn focus_host_window(
    app: tauri::AppHandle,
    host: String,
    state: tauri::State<'_, SessionState>,
) -> Result<bool, String> {
    let host_norm = host.trim().to_lowercase();
    let label = {
        let guard = state.inner.lock().await;
        guard
            .values()
            .find(|h| h.host.trim().to_lowercase() == host_norm)
            .map(|h| h.window_label.clone())
    };
    let Some(label) = label else { return Ok(false) };
    let Some(w) = app.get_webview_window(&label) else {
        return Ok(false);
    };
    if w.is_minimized().unwrap_or(false) {
        let _ = w.unminimize();
    }
    let _ = w.set_focus();
    let _ = app.emit_to(
        &label,
        "aurora://activate-host",
        ActivateHostPayload { host },
    );
    Ok(true)
}
