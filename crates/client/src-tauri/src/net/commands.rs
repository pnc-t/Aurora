//! フロントから呼ぶ制御系 tauri コマンド (入力・表示切替・ビットレート・ファイル要求など)。
use super::*;

#[tauri::command]
pub async fn disconnect(
    app: tauri::AppHandle,
    session_id: String,
    // タブごと恒久終了するときだけ true。切断検知→再接続の経路は false で呼ばれ、
    // 「今回だけ接続」(未保存) の再接続が staging の資格情報に依存するため残す必要がある。
    forget_credentials: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    {
        let mut guard = state.inner.lock().await;
        if let Some(s) = guard.remove(&session_id) {
            tracing::info!(session = %session_id, host = %s.host, "disconnected");
            // task abort だけでなくクリップボードスレッドにも shutdown を伝える
            // (connect_to_host のセッション置換経路と同じ後始末に統一)。
            s.shutdown();
        }
    }
    // 恒久終了なら staging に保持していた接続資格情報も破棄する
    // (connect_inner が再接続用に remove せず保持する方針にした分、ここで対称に消す)。
    if forget_credentials {
        state.staging.lock().await.remove(&session_id);
    }
    // ランチャーへ接続中 host 一覧を通知 (切断表示・接続中バッジ除去)。
    emit_connections(&app, &state).await;
    Ok(())
}

/// 指定 host の全セッションを停止する (ランチャーの切断ボタン)。session_id ではなく host で
/// 引くため、ウィンドウが既に閉じた後に残った孤児セッション (接続成立中にウィンドウを閉じる等)
/// もここで確実に止められる。capture ウィンドウの JS 経由 (`aurora://disconnect-host`) では
/// 窓が無い孤児に届かないため、Rust 側で直接掃く経路を用意する。
#[tauri::command]
pub async fn disconnect_host(
    app: tauri::AppHandle,
    host: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let target = host.trim().to_lowercase();
    let stopped_ids: Vec<String> = {
        let mut guard = state.inner.lock().await;
        let ids: Vec<String> = guard
            .iter()
            .filter(|(_, h)| h.host.trim().to_lowercase() == target)
            .map(|(id, _)| id.clone())
            .collect();
        let mut stopped = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(h) = guard.remove(&id) {
                h.shutdown();
                stopped.push(id);
            }
        }
        stopped
    };
    // 停止した全セッション分、staging に保持していた接続資格情報も破棄する。
    {
        let mut staging = state.staging.lock().await;
        for id in &stopped_ids {
            staging.remove(id);
        }
    }
    emit_connections(&app, &state).await;
    Ok(())
}

#[tauri::command]
pub async fn send_input(
    session_id: String,
    event: InputEvent,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(&state, &session_id, ControlMsg::Input(event)).await
}

#[tauri::command]
pub async fn request_keyframe(
    session_id: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(&state, &session_id, ControlMsg::RequestKeyframe).await
}

/// キャプチャ対象ディスプレイの切替を要求する。
#[tauri::command]
pub async fn select_display(
    session_id: String,
    id: u32,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(&state, &session_id, ControlMsg::SelectDisplay { id }).await
}

/// 映像パラメータ (fps 上限 / ビットレート上限) を実行時にホストへ送る。
#[tauri::command]
pub async fn set_video_params(
    session_id: String,
    fps: u32,
    max_bitrate_bps: u32,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(
        &state,
        &session_id,
        ControlMsg::SetVideoParams {
            fps,
            max_bitrate_bps,
        },
    )
    .await
}

/// クリップボード同期のセッション毎トグル (ツールバーから呼ぶ)。OFF の間は reader ループが
/// リモートの変更を適用せず、clip_forward タスクがローカル変化を送らない (ネットワークは
/// 継続したままの一時停止。切断・再接続は不要)。
#[tauri::command]
pub async fn set_clipboard_sync(
    session_id: String,
    enabled: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.get(&session_id).ok_or("unknown session")?;
    s.clipboard_sync.store(enabled, Ordering::Relaxed);
    Ok(())
}

/// 音声ミュートの切替をホストへ送る (ミュート中はホストが音声を送らない)。
#[tauri::command]
pub async fn set_audio_muted(
    session_id: String,
    muted: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(&state, &session_id, ControlMsg::SetAudioMuted { muted }).await
}

/// モバイル専用: 音声 (Opus) フレームを流すための Channel をセッションへアタッチする。
/// デスクトップは cpal ネイティブ再生のため呼ばない。接続/再接続のたびに (フロントの
/// `setAudioAvailable` から) 呼び直す必要がある: `connect_to_host` はセッション置換のたびに
/// 新しい `Sink` を作るため、以前アタッチした Channel は引き継がれない。
#[cfg(mobile)]
#[tauri::command]
pub async fn attach_audio_channel(
    session_id: String,
    audio_channel: Channel<InvokeResponseBody>,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.get(&session_id).ok_or("unknown session")?;
    s.sink.set_audio(audio_channel);
    Ok(())
}

/// クライアント再生音量を設定する (0.0〜1.0)。host へは送らず client 内部のゲインのみ変更する。
#[tauri::command]
pub async fn set_audio_volume(
    session_id: String,
    volume: f32,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.get(&session_id).ok_or("unknown session")?;
    // 音声非対応 (audio: None) の接続では no-op で成功扱いにする。
    if let Some(p) = &s.audio {
        p.set_volume(volume);
    }
    Ok(())
}

/// 映像送出の一時停止 / 再開をホストへ送る (非アクティブタブの帯域を止める)。
/// 再開時は呼び出し側が別途 `request_keyframe` で映像を復帰させる。
#[tauri::command]
pub async fn set_video_paused(
    session_id: String,
    paused: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    // UDP 自己修復ウォッチドッグへ pause 状態を伝える。pause 中の意図的な無受信を
    // ネットワーク途絶と誤検知しないようにする。
    {
        let guard = state.inner.lock().await;
        if let Some(s) = guard.get(&session_id) {
            s.udp_health.set_paused(paused);
        }
    }
    send_ctrl(&state, &session_id, ControlMsg::SetVideoPaused { paused }).await
}

/// 対応映像コーデックの (再) 申告をホストへ送る。接続中の手動切替 (設定で "auto"/"h264" 等を
/// 選び直した) 用。手動で特定コーデックへ固定する場合はその 1 つだけを含む配列で絞って送る
/// (host 側は全クライアントの共通集合から最良を選ぶため、これで実質固定できる)。
#[tauri::command]
pub async fn set_supported_codecs(
    session_id: String,
    codecs: Vec<String>,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(
        &state,
        &session_id,
        ControlMsg::SetSupportedCodecs {
            codecs: parse_supported_codecs(&codecs),
        },
    )
    .await
}

/// 映像へのカーソル合成 ON/OFF をホストへ送る (マウスキャプチャ中だけ ON にする)。
#[tauri::command]
pub async fn set_cursor_overlay(
    session_id: String,
    enabled: bool,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(
        &state,
        &session_id,
        ControlMsg::SetCursorOverlay { enabled },
    )
    .await
}

/// 送出する映像の最大高さ (0 = キャプチャ解像度) をホストへ送る。
#[tauri::command]
pub async fn set_resolution(
    session_id: String,
    max_height: u32,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(
        &state,
        &session_id,
        ControlMsg::SetResolution { max_height },
    )
    .await
}

/// クライアントの映像表示領域サイズ (物理 px) をホストへ送る。ホストはこれに最も近い
/// 対応モードへキャプチャ対象ディスプレイの解像度を変更する (RDP 風・黒帯解消)。
#[tauri::command]
pub async fn set_client_viewport(
    session_id: String,
    width: u32,
    height: u32,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    send_ctrl(
        &state,
        &session_id,
        ControlMsg::SetClientViewport { width, height },
    )
    .await
}

/// ホスト上のファイルのダウンロードを要求する。受信は reader ループが
/// `%USERPROFILE%\Aurora\downloads` に保存し、完了を FileResult イベントで通知する。
#[tauri::command]
pub async fn request_file(
    session_id: String,
    id: u32,
    path: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.get(&session_id).ok_or("unknown session")?;
    // この id を「要求済み」に登録してから送る。reader はこの集合にある id の FileOffer
    // だけを受理するため、登録は送信より先に行う (オファー受信との競合を避ける)。
    if let Ok(mut pending) = s.pending_downloads.lock() {
        pending.insert(id);
    }
    s.ctrl_tx
        .send(ControlMsg::FileRequest { id, path })
        .map_err(|_| "ctrl channel closed".to_string())
}

/// ローカルのファイルパス (キャプチャ画面への D&D 由来) を host へアップロードする。
/// ファイルは Rust 側で読み、既存のアップロードプロトコル (FileOffer + FileChunk) を
/// bounded file チャネル経由で流す。バイト列を WebView に載せず net 層を再利用する。
/// 完了 / 失敗は host からの FileResult イベントで通知される。
#[tauri::command]
pub async fn upload_file_from_path(
    session_id: String,
    id: u32,
    path: String,
    state: tauri::State<'_, SessionState>,
) -> Result<(), String> {
    const CHUNK: usize = 32 * 1024;
    // file_tx/sink を clone してロックは send の await を跨がない。sink は進捗 (FileProgress)
    // emit 用。
    let (file_tx, sink) = {
        let guard = state.inner.lock().await;
        let s = guard.get(&session_id).ok_or("unknown session")?;
        (s.file_tx.clone(), s.sink.clone())
    };
    let name = Path::new(&path)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("upload.bin")
        .to_string();
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| format!("open {path}: {e}"))?;
    // size=0 を誤って告知すると host 側が「告知サイズ超過」で初回チャンクから拒否して
    // しまうため、metadata 失敗はここでエラーにする。
    let size = file
        .metadata()
        .await
        .map(|m| m.len())
        .map_err(|e| format!("metadata {path}: {e}"))?;
    file_tx
        .send(ControlMsg::FileOffer { id, name, size })
        .await
        .map_err(|_| "file channel closed".to_string())?;
    let mut offset: u64 = 0;
    let mut buf = vec![0u8; CHUNK];
    // 進捗 emit は download 側 (net/connect.rs) と対称に 100ms 以上間隔を空けて間引く。
    let mut last_progress: Option<Instant> = None;
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read: {e}"))?;
        let last = n == 0 || offset + n as u64 >= size;
        file_tx
            .send(ControlMsg::FileChunk {
                id,
                offset,
                data: buf[..n].to_vec(),
                last,
            })
            .await
            .map_err(|_| "file channel closed".to_string())?;
        offset += n as u64;
        let now = Instant::now();
        let should_emit = last
            || match last_progress {
                Some(t) => now.duration_since(t) >= Duration::from_millis(100),
                None => true,
            };
        if should_emit {
            last_progress = Some(now);
            sink.send_event(ClientEvent::FileProgress {
                id,
                done: offset,
                total: size,
            });
        }
        if last {
            break;
        }
    }
    Ok(())
}
