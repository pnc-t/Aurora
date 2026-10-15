//! TCP 制御チャネルの確立とループ (connect/handshake, TCP 読み書き, hs_io)。
use super::*;

/// 「最後に Pong を受信してから何 ms 経ったか」で無言のネットワーク断 (Wi-Fi→有線切替等で
/// TCP が無言停滞するケース) を検知するまでの猶予。OS の再送タイムアウト (数十秒) を待たず、
/// 数秒でユーザーへ気付かせて自動再接続へ渡す。
const PONG_TIMEOUT: Duration = Duration::from_secs(5);

/// reader → ProjFS 作業タスクの要求キュー長 (fs_worker_loop)。
const FS_QUEUE: usize = 64;

/// ハンドシェイク I/O を `CONNECT_TIMEOUT` で包む。タイムアウト/失敗を String に正規化する。
async fn hs_io(fut: impl std::future::Future<Output = std::io::Result<()>>) -> Result<(), String> {
    tokio::time::timeout(CONNECT_TIMEOUT, fut)
        .await
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(err)
}

/// ハンドシェイク段階 (TCP 接続後) の失敗をユーザー向け日本語へ正規化する。プロトコル上、
/// パスワード不一致とデバイスピン留め未承認はクライアント側から区別できない (どちらも
/// ハンドシェイク後の読み取り/復号失敗になる) ため、両者をまとめた文言に丸め、技術詳細は
/// tracing にだけ残す。端末ID (fp) を併記し、host 側トレイの承認 UI と照合できるようにする。
fn hs_fail<E: std::fmt::Display>(fp: Option<&str>, e: E) -> String {
    tracing::debug!(error = %e, "handshake failed");
    match fp {
        Some(fp) => {
            format!(
                "パスワードが違うか、ホスト側でこの端末が承認されていません (この端末の端末ID: {fp})"
            )
        }
        None => "パスワードが違うか、ホスト側でこの端末が承認されていません".to_string(),
    }
}
/// `connect_to_host` の成功結果。通常接続 (`Connected`) と、デバイスピン留めの承認待ち
/// (`PendingApproval`) を区別してフロントへ返す。フロントは承認待ちを「エラー」ではなく
/// 待機状態として扱い、自動再接続する。`fingerprint` はこの端末自身の ID(照合表示用)。
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConnectOutcome {
    Connected,
    PendingApproval { fingerprint: String },
}

/// 指定セッションの制御チャネルへ 1 メッセージ送る共通処理。未知 id は拒否する。
pub(crate) async fn send_ctrl(
    state: &SessionState,
    session_id: &str,
    msg: ControlMsg,
) -> Result<(), String> {
    let guard = state.inner.lock().await;
    let s = guard.get(session_id).ok_or("unknown session")?;
    s.ctrl_tx
        .send(msg)
        .map_err(|_| "ctrl channel closed".to_string())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
// Tauri コマンドの引数 (state/channel) 注入で増える
// session_id を span で全ログへ付ける。host 側 conn_id と peer/時刻で突き合わせて 1 接続を
// 追跡できる。後段で spawn する I/O タスクにも同じ span を継承させる (Span::current)。
// 失敗時はここで 1 度だけ warn に残す。実体 (connect_inner) は各 `?` で String を早期 return
// するため、そのままでは接続失敗 (誤パスワード・タイムアウト等) が client.log に出ない。
#[tracing::instrument(skip_all, fields(session = %session_id, host = %host, port = port))]
pub async fn connect_to_host(
    app: tauri::AppHandle,
    window: WebviewWindow,
    session_id: String,
    host: String,
    port: u16,
    save: bool,
    // このクライアントが対応する映像コーデック ("av1"/"hevc"/"h264" の小文字文字列。無指定/
    // 空は h264 のみ扱い)。接続確立直後に `SetSupportedCodecs` として host へ送る。
    supported_codecs: Vec<String>,
    event_channel: Channel<ClientEvent>,
    video_channel: Channel<InvokeResponseBody>,
    state: tauri::State<'_, SessionState>,
) -> Result<ConnectOutcome, String> {
    let r = connect_inner(
        app,
        window,
        session_id,
        host,
        port,
        save,
        supported_codecs,
        event_channel,
        video_channel,
        state,
    )
    .await;
    if let Err(e) = &r {
        tracing::warn!(error = %e, "connect failed");
    }
    r
}

/// この端末の表示名 (PC 名) を返す。host のデバイスピン留め登録時に「どの PC を登録したか」を
/// 判別できるよう、ハンドシェイク payload で host へ渡す。Windows は `COMPUTERNAME`、他 OS は
/// `HOSTNAME` を見る。取得できなければ空文字 (host 側で既定ラベル device-xxxx にフォールバック)。
fn local_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// HostInfo で受け取った MAC を `recent_hosts` の該当 host エントリへ自動保存する
/// (WoL 対象への流用用)。`icon` と同じ「None=情報なし・既存値保持」規約に従い、
/// このホストが**未登録** (recent_hosts に無い、まだ一度も保存されていない) なら何もしない
/// (フロントが接続後に `saveRecentHost` で新規登録した時点で改めて拾われる)。
/// 保存失敗はログのみ (MAC の自動保存は補助機能で接続を止める理由にはしない)。
async fn save_host_mac(app: &tauri::AppHandle, host: &str, mac: Option<[u8; 6]>) {
    let Some(mac) = mac else { return };
    let app = app.clone();
    let host = host.to_string();
    // load/save_recent_hosts は同期ファイル I/O のため tokio タスク内から直接呼ばない。
    let res = tokio::task::spawn_blocking(move || {
        let Ok(hosts) = crate::config::load_recent_hosts(app.clone()) else {
            return Ok(());
        };
        let Some(existing) = hosts.iter().find(|h| h.host.eq_ignore_ascii_case(&host)) else {
            return Ok(());
        };
        let entry = crate::config::HostEntry {
            label: existing.label.clone(),
            host,
            port: existing.port,
            icon: None,
            overrides: None,
            mac: Some(format_mac(mac)),
            last_connected: None,
        };
        crate::config::save_recent_host(app, entry)
    })
    .await;
    match res {
        Ok(Err(e)) => tracing::warn!(error = %e, "failed to save host MAC"),
        Err(e) => tracing::warn!(error = %e, "save host MAC task failed"),
        Ok(Ok(())) => {}
    }
}

/// HostInfo 受信 (= ハンドシェイク成功) の日時を `recent_hosts` の該当 host エントリへ記録する
/// (launcher の「最終接続」表示用)。`save_host_mac` と同じ「未登録なら何もしない・None=情報
/// なしで既存値保持」の経路 (load→更新→save)。保存失敗はログのみ (補助情報のため接続を止めない)。
async fn save_host_last_connected(app: &tauri::AppHandle, host: &str) {
    let app = app.clone();
    let host = host.to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // load/save_recent_hosts は同期ファイル I/O のため tokio タスク内から直接呼ばない。
    let res = tokio::task::spawn_blocking(move || {
        let Ok(hosts) = crate::config::load_recent_hosts(app.clone()) else {
            return Ok(());
        };
        let Some(existing) = hosts.iter().find(|h| h.host.eq_ignore_ascii_case(&host)) else {
            return Ok(());
        };
        let entry = crate::config::HostEntry {
            label: existing.label.clone(),
            host,
            port: existing.port,
            icon: None,
            overrides: None,
            mac: None,
            last_connected: Some(now),
        };
        crate::config::save_recent_host(app, entry)
    })
    .await;
    match res {
        Ok(Err(e)) => tracing::warn!(error = %e, "failed to save host last_connected"),
        Err(e) => tracing::warn!(error = %e, "save host last_connected task failed"),
        Ok(Ok(())) => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_inner(
    app: tauri::AppHandle,
    // このコマンドを呼んだ capture ウィンドウ。セッションを所有ウィンドウへ紐付け、
    // ウィンドウ破棄時にそのウィンドウの全タブをまとめて停止できるようにする。
    window: WebviewWindow,
    session_id: String,
    host: String,
    port: u16,
    // パスワードを DPAPI ストアへ保存してよいか (接続ダイアログの「保存する」由来)。
    // 手入力かつ true のときだけ保存し、「今回だけ接続」を可能にする。
    // 保存済み即接続/自動再接続では false 相当で、既存値に触れない。
    save: bool,
    supported_codecs: Vec<String>,
    event_channel: Channel<ClientEvent>,
    video_channel: Channel<InvokeResponseBody>,
    state: tauri::State<'_, SessionState>,
) -> Result<ConnectOutcome, String> {
    // 接続資格情報は staging マップ (open_capture_window が積む) から取り出す。平文を
    // ウィンドウ URL に載せないための受け渡し。remove ではなく参照コピーにし、このセッションが
    // 生きている間は staging に残す (自動/手動再接続でも同じ session_id で再度ここを通るため。
    // Android は Keystore 対応済みで保存済みパスワードなら secret::get で取れるが、「今回だけ
    // 接続」(未保存) の場合は依然 secret::get が None になるため、remove すると再接続が
    // 必ず失敗する)。トレードオフとして平文パスワードをセッション終了までメモリに保持する。
    // 恒久的な終了 (disconnect/disconnect_host/ウィンドウ破棄/孤児セッション停止) 側で明示的に
    // staging.remove する運用とする。
    // 接続処理中として登録する (完了・失敗のどちらでも guard の drop で外れる)。
    state
        .connecting
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            session_id.clone(),
            Connecting {
                host: host.clone(),
                cancelled: false,
            },
        );
    let _connecting = ConnectingGuard {
        state: &state,
        session_id: session_id.clone(),
    };
    let staged = state.staging.lock().await.get(&session_id).cloned();
    let password = match staged.flatten() {
        Some(p) => p,
        // secret ストアは同期 I/O + OS 呼び出し (Windows: ファイル読み + DPAPI、Android:
        // Keystore の同期 Binder IPC)。tokio ワーカーを塞がないよう spawn_blocking へ逃がす
        // (同ファイルの save_host_mac / save_host_last_connected と同じ扱い)。
        None => {
            let (app2, host2) = (app.clone(), host.clone());
            tokio::task::spawn_blocking(move || crate::secret::get(&app2, &host2))
                .await
                .map_err(|e| format!("パスワードの読み出しに失敗: {e}"))?
                .ok_or_else(|| "保存済みパスワードがありません".to_string())?
        }
    };
    // 同 id の既存セッションがあれば閉じる (再接続・置換)。
    {
        let mut guard = state.inner.lock().await;
        if let Some(s) = guard.remove(&session_id) {
            s.shutdown();
        }
    }

    // ホスト名 (Tailscale MagicDNS 名等) も解決する。SocketAddr::parse は数値 IP しか
    // 受け付けないため、lookup_host で名前解決し IPv4/IPv6 の最初のアドレスを採る。
    let target = format!("{host}:{port}");
    let tcp_addr: SocketAddr = tokio::net::lookup_host(&target)
        .await
        .map_err(|e| {
            tracing::debug!(error = %e, "dns resolve failed");
            format!("ホスト名を解決できません: {host}")
        })?
        .next()
        .ok_or_else(|| format!("ホスト名を解決できません: {host}"))?;
    let udp_addr = tcp_addr;

    tracing::info!(%tcp_addr, "connecting");
    let mut tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(tcp_addr))
        .await
        .map_err(|_| {
            "接続がタイムアウトしました (ホストが起動していないか、ネットワークに到達できません)"
                .to_string()
        })?
        .map_err(|e| format!("ホストに接続できません: {e}"))?;
    tcp.set_nodelay(true).ok();

    let psk = psk_from_password(&password);

    // この端末の永続 ID 鍵を読み込む (無ければ生成保存)。host のデバイスピン留めで端末を
    // 識別できるよう、毎回同じ静的公開鍵で接続する。取得失敗時は None=従来の使い捨て鍵。
    // 初回は鍵生成 + 保存 (Android は Keystore の TEE/StrongBox 生成まで走る) を伴う同期処理
    // なので spawn_blocking へ逃がす。join 失敗も「取得できなかった」と同じ扱いで None。
    let identity = {
        let app2 = app.clone();
        tokio::task::spawn_blocking(move || crate::secret::load_or_create_identity(&app2))
            .await
            .unwrap_or(None)
    };
    let identity_priv = identity.as_ref().map(|id| id.private.as_slice());
    // 端末ID (フィンガープリント)。ハンドシェイク以降の失敗メッセージに併記し、ユーザーが
    // ホスト側トレイの承認 UI と照合できるようにする (下の hs_fail 参照)。
    let fp = identity.as_ref().map(|id| device_fingerprint(&id.public));

    // --- Noise XX-PSK initiator ---
    // ハンドシェイク I/O には全てタイムアウトを設ける。これが無いと、ポートは開いているが
    // 応答しないホスト (別アプリがポートを掴んでいる等) に対して connect_to_host が永久に
    // 返らず、UI が「connecting…」のままキャンセル不能になる (host 側の HANDSHAKE_TIMEOUT と対)。
    let mut hs = HandshakeState::new_initiator(&psk, identity_priv).map_err(err)?;
    let mut buf = Vec::new();
    let mut tmp = Vec::new();
    // -> e
    let mut out_msg = Vec::new();
    hs.write_msg(&[], &mut out_msg)
        .map_err(|e| hs_fail(fp.as_deref(), e))?;
    hs_io(write_tcp_frame(&mut tcp, &out_msg))
        .await
        .map_err(|e| hs_fail(fp.as_deref(), e))?;
    // <- e, ee, s, es
    hs_io(read_tcp_frame(&mut tcp, &mut buf))
        .await
        .map_err(|e| hs_fail(fp.as_deref(), e))?;
    hs.read_msg(&buf, &mut tmp)
        .map_err(|e| hs_fail(fp.as_deref(), e))?;
    // -> s, se, psk
    // 最終メッセージの (暗号化済み) payload にこの端末名 (PC 名) を載せる。host はデバイス
    // ピン留め登録時にこれを表示ラベルにし、トレイの一覧で「どの PC を登録したか」を判別できる。
    // 名前は機密ではないが平文露出も避けたいので、payload (XXpsk3 の 3 通目は暗号化される) で渡す。
    hs.write_msg(local_device_name().as_bytes(), &mut out_msg)
        .map_err(|e| hs_fail(fp.as_deref(), e))?;
    hs_io(write_tcp_frame(&mut tcp, &out_msg))
        .await
        .map_err(|e| hs_fail(fp.as_deref(), e))?;

    // snow の StatelessTransportState は encrypt/decrypt とも &self なので、
    // 複数タスク (writer/reader/udp) 間の共有は Arc だけで足り Mutex は不要。
    // 映像 UDP のパケット毎 decrypt が TCP 制御とロック競合しなくなる。
    let session = Arc::new(hs.into_transport().map_err(|e| hs_fail(fp.as_deref(), e))?);
    tracing::info!("noise handshake complete");

    // ハンドシェイク成功 = PSK (パスワード) 一致。ユーザーが「保存する」を選んだときだけ
    // DPAPI 暗号化保存する (無断永続化を避け「今回だけ接続」を可能にする)。失敗してもログ
    // のみで接続は続行する (保存は補助機能)。`save=false` (今回だけ/保存済み即接続/再接続) は
    // 既存の保存値に触れない (= 保存済みなら維持、未保存なら未保存のまま)。
    if save {
        // 読み出し側と同じ理由で spawn_blocking (保存は暗号化 + ファイル書き込みを伴う)。
        let (app2, host2, pw2) = (app.clone(), host.clone(), password.clone());
        match tokio::task::spawn_blocking(move || crate::secret::put(&app2, &host2, &pw2)).await {
            Ok(Err(e)) => tracing::warn!(error = %e, "failed to save host password"),
            Err(e) => tracing::warn!(error = %e, "save host password task failed"),
            Ok(Ok(())) => {}
        }
    }

    // 最初の制御メッセージ (HostInfo) を受け取る。CTRL ドメインの nonce 0 で復号し、
    // 以降の TCP 受信は reader ループが nonce 1 から続ける。
    hs_io(read_tcp_frame(&mut tcp, &mut buf))
        .await
        .map_err(|e| hs_fail(fp.as_deref(), e))?;
    let mut plain = Vec::new();
    let mut hostinfo_rx_nonce: u64 = 0;
    let host_info = decode_ctrl_msg(&session, &buf, &mut hostinfo_rx_nonce, &mut plain)
        .map_err(|e| hs_fail(fp.as_deref(), e))?;

    // 承認待ちは接続確立ではないので、sink 生成・Connected 通知の前に早期 return する
    // (Connected イベントを先に飛ばすと client 側で承認待ち画面と handshake 表示がちらつく)。
    if let ControlMsg::PendingApproval = host_info {
        tracing::info!("host reports device pending owner approval");
        return Ok(ConnectOutcome::PendingApproval {
            fingerprint: fp.clone().unwrap_or_default(),
        });
    }

    // 映像/イベントの表示先を差し替え可能 sink にまとめる。以降タスクへはこの sink を
    // clone して渡し、タブのウィンドウ移動 (attach/detach) で表示先だけ付け替えられるようにする。
    let sink = Sink::new(event_channel, video_channel);

    sink.send_event(ClientEvent::Connected);
    // HostInfo の中身を取り出すだけにし、フロントへの通知はセッションハンドルを管理状態へ
    // 格納した後に行う (関数末尾)。先に通知すると、フロントが host_info を受けて即
    // selectDisplay/setClientViewport を invoke した時にハンドル未格納 (再接続時は古い閉じた
    // ctrl_tx) で送信が失敗し、仮想ディスプレイのテイクオーバーが起動しないレースになる。
    let (host_displays, host_audio) = match host_info {
        ControlMsg::HostInfo {
            displays,
            audio,
            mac,
        } => {
            save_host_mac(&app, &host, mac).await;
            save_host_last_connected(&app, &host).await;
            (displays, audio)
        }
        other => return Err(format!("expected HostInfo, got {other:?}")),
    };

    // UDP ソケットをローカル任意ポートで開き、サーバへ向けて "AURHELLO"。
    // 高 fps 時のバースト受信での取りこぼし (= 映像ノイズ) を抑えるため受信バッファを拡大する。
    let udp =
        Arc::new(make_udp_socket(udp_addr.is_ipv6()).map_err(|e| format!("UDP socket: {e}"))?);
    udp.connect(udp_addr)
        .await
        .map_err(|e| format!("UDP connect: {e}"))?;
    udp.send(b"AURHELLO")
        .await
        .map_err(|e| format!("UDP hello: {e}"))?;

    let (read_half, write_half) = tcp.into_split();
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let (file_tx, file_rx) = mpsc::channel::<ControlMsg>(32);

    // 接続確立直後の対応コーデック申告。writer タスクの起動前に積んでおけば、送出される
    // 最初の制御メッセージになる (nonce 0 の衝突を避けるため、書き込みは必ず writer タスク
    // 側の単一カウンタ経由に統一する。ここで直接 TCP へ書かない)。
    let _ = ctrl_tx.send(ControlMsg::SetSupportedCodecs {
        codecs: parse_supported_codecs(&supported_codecs),
    });

    // クリップボード同期 (このセッション専用に起動、disconnect で shutdown)
    let (clip_handle, mut clip_change_rx) = clipboard::spawn();
    let clip_apply_tx = clip_handle.apply_tx.clone();
    // Android 専用: tcp_reader_loop が clip_apply_tx へ流したリモート適用要求は、デスクトップでは
    // clipboard アクタースレッドがそのままローカルクリップボードへ書き込むが、Android は
    // arboard 非対応のため誰も消費しない。ここで JNI 経由の端末クリップボード書き込み
    // (`mobile_clipboard::write`) を呼ぶタスクを立てる。WebView の navigator.clipboard.writeText
    // はユーザー操作を伴わない呼び出しを拒否するため使わない (mobile_clipboard 冒頭コメント参照)。
    // `mobile_clipboard::write` は JNI 経由で ClipboardManager (binder IPC) を叩く同期・
    // ブロッキング呼び出しなので、tokio ワーカーを塞がないよう呼び出し毎に spawn_blocking へ
    // 委譲する (CLAUDE.md: tokio::spawn 内で同期 I/O を呼ばない。書き方は save_host_mac 等の
    // 既存 spawn_blocking に揃える)。
    // ホストがコピーするたびに来るため、成否ともセッションごとに 1 度だけ log する
    // (実機で client.log が 9.7MB まで膨れた実績があるため)。
    #[cfg(target_os = "android")]
    let mobile_clip_apply_task = {
        let clip_handle_for_apply = clip_handle.clone();
        tokio::spawn(async move {
            let Some(mut apply_rx) = clip_handle_for_apply.take_apply_rx().await else {
                return;
            };
            let mut logged = false;
            while let Some(text) = apply_rx.recv().await {
                let res =
                    tokio::task::spawn_blocking(move || crate::mobile_clipboard::write(&text))
                        .await;
                match res {
                    Ok(Ok(())) => {
                        if !logged {
                            logged = true;
                            tracing::info!("clipboard write from host succeeded");
                        }
                    }
                    Ok(Err(e)) => {
                        if !logged {
                            logged = true;
                            tracing::warn!(error = %e, "clipboard write from host failed");
                        }
                    }
                    Err(e) => {
                        if !logged {
                            logged = true;
                            tracing::warn!(error = %e, "clipboard write task failed");
                        }
                    }
                }
            }
        })
    };
    // クリップボード同期の可否ゲート。設定の初期値をここで直接反映する (TS 側の後追い適用に
    // 依存すると、起動直後の数回分がレースで漏れうるため Rust 側で初期化する)。設定読み込み
    // 失敗時は安全側の既定 (ON) にする。
    let clipboard_sync = Arc::new(AtomicBool::new(
        crate::config::load_settings(app.clone())
            .map(|s| s.clipboard_sync)
            .unwrap_or(true),
    ));

    // 受信品質メトリクス。Ping の基準時刻と共有して RTT を算出する。
    let stats = StatsCollector::new();
    let session_start = Instant::now();
    // 最後に Pong を受信した時刻 (session_start からの経過 ms)。0 = 未受信 (接続直後扱い)。
    // watchdog task が定期的にこれを見て、無言のネットワーク断 (PONG_TIMEOUT 超過) を検知する。
    let last_pong_ms = Arc::new(AtomicU64::new(0));
    // UDP 映像/音声受信の自己修復に使う共有状態 (途絶検知 + AURHELLO 再送)。
    let udp_health = Arc::new(UdpHealth::new());

    let sink_w = sink.clone();
    let sess_w = session.clone();
    let writer_task = tokio::spawn(
        async move {
            let _ = tcp_writer_loop(write_half, sess_w, ctrl_rx, file_rx).await;
            sink_w.send_event(ClientEvent::Disconnected {
                reason: "接続が閉じられました".into(),
            });
        }
        .instrument(tracing::Span::current()),
    );

    let sink_r = sink.clone();
    let sess_r = session.clone();
    let clip_apply_for_reader = clip_apply_tx.clone();
    let stats_for_reader = stats.clone();
    // FS RPC 応答は制御チャネル経由で writer ループへ流す (host の同期コールバックを起こす)。
    let fs_reply_tx = ctrl_tx.clone();
    // このセッションのファイル共有スコープ (既定 Disabled = 無効)。reader と SessionHandle で共有。
    let share_scope: Arc<Mutex<ShareScope>> = Arc::new(Mutex::new(ShareScope::Disabled));
    // ProjFS 要求は reader から作業タスクへ回す (reader を FS I/O で塞がない)。bounded: 悪意ある
    // ホストの要求殺到でメモリを膨らませない。満杯時は reader が待つが、host 側も RPC を bounded に
    // 持つため通常は満たない。
    let (fs_req_tx, fs_req_rx) = mpsc::channel::<ControlMsg>(FS_QUEUE);
    let fs_worker_task = tokio::spawn(
        fs_worker_loop(fs_req_rx, share_scope.clone(), fs_reply_tx)
            .instrument(tracing::Span::current()),
    );
    // ダウンロード要求済み id の集合。request_file が追加し、reader が未要求の FileOffer を弾く。
    let pending_downloads = Arc::new(std::sync::Mutex::new(HashSet::<u32>::new()));
    let pending_for_reader = pending_downloads.clone();
    let last_pong_for_reader = last_pong_ms.clone();
    let clipboard_sync_for_reader = clipboard_sync.clone();
    let reader_task = tokio::spawn(
        async move {
            let res = tcp_reader_loop(
                read_half,
                sess_r,
                sink_r.clone(),
                clip_apply_for_reader,
                stats_for_reader,
                session_start,
                fs_req_tx,
                pending_for_reader,
                last_pong_for_reader,
                clipboard_sync_for_reader,
            )
            .await;
            let reason = match res {
                Ok(()) => "ホスト側で接続が終了しました".to_string(),
                Err(e) => format!("接続が切断されました: {e}"),
            };
            tracing::info!(%reason, "session closed (tcp reader)");
            sink_r.send_event(ClientEvent::Disconnected { reason });
        }
        .instrument(tracing::Span::current()),
    );

    // 音声再生 (ベストエフォート)。失敗しても映像のみで継続する。デスクトップは cpal ネイティブ
    // 再生、モバイルは sink 経由で WebView へ転送する (audio.rs 参照、方式A)。
    #[cfg(desktop)]
    let audio_player = match audio::spawn() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "audio playback disabled");
            None
        }
    };
    #[cfg(mobile)]
    let audio_player = match audio::spawn(sink.clone()) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "audio playback disabled");
            None
        }
    };
    let audio_tx = audio_player.as_ref().map(|p| p.sender());

    let sess_u = session.clone();
    let stats_for_udp = stats.clone();
    let sink_u = sink.clone();
    // ロス検出時にキーフレームを要求して映像を即時回復させるための送信口。
    let keyframe_tx = ctrl_tx.clone();
    let udp_health_for_reader = udp_health.clone();
    // AURHELLO 再送 (self-heal watchdog) 用にソケットの clone を先に取っておく (udp 本体は
    // reader ループへ move する)。
    let udp_for_resend = udp.clone();
    let udp_task = tokio::spawn(
        async move {
            if let Err(e) = udp_reader_loop(
                udp,
                sess_u,
                sink_u,
                audio_tx,
                stats_for_udp,
                keyframe_tx,
                session_start,
                udp_health_for_reader,
            )
            .await
            {
                tracing::warn!(error = %e, "UDP reader ended");
            }
        }
        .instrument(tracing::Span::current()),
    );

    // クリップボード変化 → ControlMsg::Clipboard 送信 (OFF 中はローカル変化を送らない)
    let ctrl_for_clip = ctrl_tx.clone();
    let clipboard_sync_for_forward = clipboard_sync.clone();
    let clip_forward_task = tokio::spawn(async move {
        while let Some(text) = clip_change_rx.recv().await {
            if !clipboard_sync_for_forward.load(Ordering::Relaxed) {
                continue;
            }
            if ctrl_for_clip.send(ControlMsg::Clipboard { text }).is_err() {
                break;
            }
        }
    });

    // ping ループ (1 秒)。RTT を細かく取り、適応制御の応答性を上げる。
    let ctrl_for_ping = ctrl_tx.clone();
    let ping_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let ts_us = session_start.elapsed().as_micros() as u64;
            if ctrl_for_ping.send(ControlMsg::Ping { ts_us }).is_err() {
                break;
            }
        }
    });

    // Pong ウォッチドッグ (1 秒間隔)。TCP の read/write エラーだけでは、Wi-Fi→有線切替等で
    // ソケットが無言停滞するケース (OS の再送タイムアウトまで数十秒かかる) を検知できない。
    // 最後の Pong 受信から PONG_TIMEOUT を超えたら切断とみなし、既存の Disconnected 経路
    // (自動再接続) に載せる。
    let sink_watchdog = sink.clone();
    let last_pong_for_watchdog = last_pong_ms.clone();
    let pong_watchdog_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let now_ms = session_start.elapsed().as_millis() as u64;
            let last_ms = last_pong_for_watchdog.load(Ordering::Relaxed);
            if now_ms.saturating_sub(last_ms) > PONG_TIMEOUT.as_millis() as u64 {
                tracing::warn!(
                    timeout_s = PONG_TIMEOUT.as_secs(),
                    "no pong received; treating as disconnected (network stall?)"
                );
                sink_watchdog.send_event(ClientEvent::Disconnected {
                    reason: "ホストから応答がありません (ネットワーク断?)".into(),
                });
                break;
            }
        }
    });

    // UDP 途絶の自己修復ウォッチドッグ (3 秒間隔)。TCP は生きている (Pong 到達中) のに映像/音声
    // UDP パケットだけ届かなくなるケース (NAT の UDP マッピング切替等) を検知し、AURHELLO 再送 +
    // キーフレーム要求で回復を試みる。非アクティブタブの意図的な一時停止 (SetVideoPaused) 中は
    // 無受信が正常なので発動しない。回復するまでこの間隔で繰り返す。
    const UDP_STALL_TIMEOUT: Duration = Duration::from_secs(3);
    let udp_health_for_watchdog = udp_health.clone();
    let keyframe_tx_for_udp_watchdog = ctrl_tx.clone();
    let udp_watchdog_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(UDP_STALL_TIMEOUT);
        interval.tick().await;
        loop {
            interval.tick().await;
            if udp_health_for_watchdog.is_paused() {
                continue;
            }
            let now_ms = session_start.elapsed().as_millis() as u64;
            if udp_health_for_watchdog.elapsed_ms(now_ms) >= UDP_STALL_TIMEOUT.as_millis() as u64 {
                tracing::warn!("no UDP video/audio for 3s while TCP alive; resending AURHELLO");
                let _ = udp_for_resend.send(b"AURHELLO").await;
                let _ = keyframe_tx_for_udp_watchdog.send(ControlMsg::RequestKeyframe);
            }
        }
    });

    // 統計タスク (1 秒窓)。計測結果をホスト (適応制御用) とフロント (表示用) に送る。
    let ctrl_for_stats = ctrl_tx.clone();
    let sink_s = sink.clone();
    let stats_for_task = stats.clone();
    let stats_task = tokio::spawn(async move {
        let mut window = StatsWindow::new(&stats_for_task);
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            let s = window.sample(&stats_for_task);
            let _ = ctrl_for_stats.send(ControlMsg::Stats {
                bitrate_kbps: s.bitrate_kbps,
                fps: s.fps,
                rtt_us: s.rtt_us,
                loss_permille: s.loss_permille,
            });
            sink_s.send_event(ClientEvent::Stats {
                bitrate_kbps: s.bitrate_kbps,
                fps: s.fps,
                rtt_us: s.rtt_us,
                loss_permille: s.loss_permille,
            });
        }
    });

    let handle = SessionHandle {
        sink: sink.clone(),
        host: host.clone(),
        ctrl_tx,
        file_tx,
        tasks: {
            // mut は #[cfg(target_os = "android")] の push でのみ使う (それ以外の build では未使用になる)。
            #[cfg_attr(not(target_os = "android"), allow(unused_mut))]
            let mut tasks = vec![
                writer_task,
                reader_task,
                udp_task,
                ping_task,
                stats_task,
                clip_forward_task,
                pong_watchdog_task,
                udp_watchdog_task,
                fs_worker_task,
            ];
            #[cfg(target_os = "android")]
            tasks.push(mobile_clip_apply_task);
            tasks
        },
        clipboard: clip_handle,
        audio: audio_player,
        pending_downloads,
        share_scope,
        window_label: window.label().to_string(),
        udp_health,
        clipboard_sync,
    };
    {
        let mut guard = state.inner.lock().await;
        // 所有ウィンドウがハンドシェイク中に閉じられていたら、孤児セッション (どの窓からも
        // 停止できず単一クライアントの host を占有し続け、以降の接続を全て塞ぐ) を作らず即停止
        // する。ウィンドウ破棄ハンドラと inner ロックで直列化し取りこぼしを防ぐ。
        if app.get_webview_window(&handle.window_label).is_none() {
            drop(guard);
            handle.shutdown();
            // このセッションは永続せず終了するため、staging に残した接続資格情報も破棄する。
            state.staging.lock().await.remove(&session_id);
            return Err("ウィンドウが閉じられました".to_string());
        }
        // 接続処理中に切断要求 (タブの × / キャンセル / ランチャーの切断) が来ていたら登録しない。
        // inner ロック下で確認するので、disconnect 側の印付けと取りこぼし無く直列化される。
        let cancelled = state
            .connecting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id)
            .is_some_and(|c| c.cancelled);
        if cancelled {
            drop(guard);
            handle.shutdown();
            tracing::info!("connect cancelled while in progress");
            return Err("接続はキャンセルされました".to_string());
        }
        if let Some(old) = guard.insert(session_id.clone(), handle) {
            // 同 id の並行接続は UI 側で防いでいるが、来た場合も旧ハンドルを孤児にしない。
            old.shutdown();
        }
    }

    // ハンドル格納後に HostInfo を通知する (上部のレース回避)。これでフロントが host_info を
    // 受けて即 selectDisplay/setClientViewport を呼んでも ctrl_tx に確実に届く。
    sink.send_event(ClientEvent::HostInfo {
        displays: host_displays,
        audio: host_audio,
    });
    // ランチャーへ接続中 host 一覧を通知 (接続中表示・保存バッジ更新)。
    emit_connections(&app, &state).await;
    // 接続確立完了 (host 側「client connected」と対)。span に session/host/port が乗る。
    tracing::info!("connected");
    Ok(ConnectOutcome::Connected)
}
// -------- internal loops --------

async fn tcp_writer_loop(
    mut write_half: OwnedWriteHalf,
    session: Arc<TransportSession>,
    mut ctrl_rx: mpsc::UnboundedReceiver<ControlMsg>,
    mut file_rx: mpsc::Receiver<ControlMsg>,
) -> anyhow::Result<()> {
    // クライアント → ホストの TCP 制御は CTRL ドメインの連番 nonce。ホスト側 rx と
    // TCP の順序保証により歩調が合う。
    let mut tcp_tx_nonce: u64 = 0;
    loop {
        // 制御チャネルとファイルチャネルの両方を待つ。どちらかが閉じたら終了。
        let msg = tokio::select! {
            m = ctrl_rx.recv() => m,
            m = file_rx.recv() => m,
        };
        let Some(msg) = msg else { break };
        send_ctrl_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
async fn tcp_reader_loop(
    mut read_half: OwnedReadHalf,
    session: Arc<TransportSession>,
    sink: Sink,
    clip_apply_tx: mpsc::UnboundedSender<String>,
    stats: Arc<StatsCollector>,
    session_start: Instant,
    fs_req_tx: mpsc::Sender<ControlMsg>,
    pending_downloads: Arc<std::sync::Mutex<HashSet<u32>>>,
    last_pong_ms: Arc<AtomicU64>,
    clipboard_sync: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    let mut plain = Vec::new();
    let mut download: Option<DownloadState> = None;
    // HostInfo (nonce 0) は connect_to_host で消費済みなので 1 から続ける。
    let mut tcp_rx_nonce: u64 = 1;
    loop {
        read_tcp_frame(&mut read_half, &mut buf).await?;
        let msg = decode_ctrl_msg(&session, &buf, &mut tcp_rx_nonce, &mut plain)?;
        match msg {
            ControlMsg::Pong { ts_us } => {
                // ts_us は Ping 送信時の session_start からの経過。往復差で RTT を得る。
                let now_us = session_start.elapsed().as_micros() as u64;
                let rtt_us = now_us.saturating_sub(ts_us);
                stats.set_rtt_us(rtt_us.min(u32::MAX as u64) as u32);
                // pong watchdog (無言のネットワーク断検知) の基準時刻を更新する。
                last_pong_ms.store(now_us / 1000, Ordering::Relaxed);
                sink.send_event(ClientEvent::Pong {
                    rtt_us: rtt_us as i64,
                });
            }
            ControlMsg::HostInfo {
                displays,
                audio,
                mac: _,
            } => {
                // 再接続時等の後続 HostInfo。MAC は接続確立時 (connect_inner) に一度保存すれば
                // 十分 (実行中に変わらない値) なので、ここでは表示用データだけ通知する。
                sink.send_event(ClientEvent::HostInfo { displays, audio });
            }
            ControlMsg::DisplayResized { id, width, height } => {
                sink.send_event(ClientEvent::DisplayResized { id, width, height });
            }
            ControlMsg::VideoCodecChanged { codec } => {
                sink.send_event(ClientEvent::VideoCodecChanged {
                    codec: codec_to_str(codec).to_string(),
                });
            }
            ControlMsg::Clipboard { text } => {
                // OFF 中はリモートの変更を適用しない (トグルはセッション単位の一時停止であり、
                // クリップボード内容自体はログに出さない)。
                if !clipboard_sync.load(Ordering::Relaxed) {
                    tracing::debug!("clipboard sync disabled; ignoring remote update");
                } else if clip_apply_tx.send(text).is_err() {
                    tracing::debug!("clipboard apply channel closed");
                }
            }
            ControlMsg::FileResult { id, ok, message } => {
                // ダウンロード中に host 側で読めなくなった等の失敗通知。受信状態を残すと以後の
                // FileOffer が全て「進行中」で拒否されるため、ここで畳む (drop で書きかけも消える)。
                if !ok && download.as_ref().is_some_and(|d| d.id == id) {
                    download = None;
                    if let Ok(mut s) = pending_downloads.lock() {
                        s.remove(&id);
                    }
                }
                sink.send_event(ClientEvent::FileResult { id, ok, message });
            }
            // --- ダウンロード受信 (ホスト → クライアント) ---
            // ユーザーが request_file で明示的に要求した id の FileOffer だけを受理する。
            // 未要求のオファーは、悪意あるホストがファイルを無断で押し付ける経路なので拒否する。
            ControlMsg::FileOffer { id, name, size } => {
                let solicited = pending_downloads
                    .lock()
                    .map(|s| s.contains(&id))
                    .unwrap_or(false);
                if !solicited {
                    tracing::warn!(id, name = %name, "unsolicited FileOffer ignored (no matching request)");
                } else if download.is_some() {
                    // 進行中のダウンロードがある間に新しい FileOffer (別 id の割り込み、または
                    // 同一 id の再送) を受理すると、単一スロット download を無条件で上書きしてしまい
                    // (a) 同一 id 再送では File::create の truncate で受信済みバイトが無言で失われ、
                    // (b) 別 id の割り込みでは先行転送の FileChunk が「offer に一致しない」として
                    // 捨てられ FileResult も届かず進捗表示が永久にハングする。よって拒否する。
                    tracing::warn!(
                        id,
                        name = %name,
                        "FileOffer rejected: a download is already in progress"
                    );
                    // 拒否した新しい方の pending_downloads エントリを解放しないとリークし、
                    // request_file を呼んだ側の進捗表示が永久にハングしたままになる。
                    if let Ok(mut s) = pending_downloads.lock() {
                        s.remove(&id);
                    }
                    sink.send_event(ClientEvent::FileResult {
                        id,
                        ok: false,
                        message: "a download is already in progress".into(),
                    });
                } else {
                    match open_download(&name).await {
                        Ok((file, path)) => {
                            tracing::info!(id, name = %name, size, path = %path, "download start");
                            download = Some(DownloadState {
                                id,
                                file,
                                written: 0,
                                total: size,
                                path,
                                last_progress: None,
                                keep: false,
                            });
                        }
                        Err(e) => {
                            if let Ok(mut s) = pending_downloads.lock() {
                                s.remove(&id);
                            }
                            sink.send_event(ClientEvent::FileResult {
                                id,
                                ok: false,
                                message: format!("open failed: {e}"),
                            });
                        }
                    }
                }
            }
            ControlMsg::FileChunk {
                id,
                offset,
                data,
                last,
            } => match download.as_mut() {
                Some(d) if d.id == id => {
                    if offset != d.written {
                        // TCP は順序保証があるため offset 不一致は本来起きない。発生したら
                        // データ欠落/重複の可能性が高く、書き込むと破損ファイルを残すため
                        // 中断する (host 側アップロード受信と対称の防御)。
                        tracing::warn!(
                            id,
                            expected = d.written,
                            got = offset,
                            "download offset mismatch, aborting"
                        );
                        if let Ok(mut s) = pending_downloads.lock() {
                            s.remove(&id);
                        }
                        sink.send_event(ClientEvent::FileResult {
                            id,
                            ok: false,
                            message: format!(
                                "download offset mismatch: expected {}, got {offset}",
                                d.written
                            ),
                        });
                        download = None;
                    } else if d.written.saturating_add(data.len() as u64) > d.total {
                        // 告知サイズ超過は中断する。悪意あるホストが告知より多く送り、
                        // クライアントのディスクを埋めるのを防ぐ。
                        tracing::warn!(
                            id,
                            total = d.total,
                            "download exceeds declared size, aborting"
                        );
                        if let Ok(mut s) = pending_downloads.lock() {
                            s.remove(&id);
                        }
                        sink.send_event(ClientEvent::FileResult {
                            id,
                            ok: false,
                            message: "download exceeds declared size".into(),
                        });
                        download = None;
                    } else if let Err(e) = d.file.write_all(&data).await {
                        let message = format!("write failed: {e}");
                        if let Ok(mut s) = pending_downloads.lock() {
                            s.remove(&id);
                        }
                        sink.send_event(ClientEvent::FileResult {
                            id,
                            ok: false,
                            message,
                        });
                        download = None;
                    } else {
                        d.written += data.len() as u64;
                        // 進捗 emit は転送ごとに 100ms 以上間隔を空ける (最終チャンクは必ず emit)。
                        // チャンク毎に emit すると Tauri IPC (JSON) が高頻度になり映像の遅延予算を食う。
                        let now = Instant::now();
                        let should_emit = last
                            || match d.last_progress {
                                Some(t) => now.duration_since(t) >= Duration::from_millis(100),
                                None => true,
                            };
                        if should_emit {
                            d.last_progress = Some(now);
                            sink.send_event(ClientEvent::FileProgress {
                                id,
                                done: d.written,
                                total: d.total,
                            });
                        }
                        if last {
                            let (written, total, path) = (d.written, d.total, d.path.clone());
                            let res = d.file.flush().await;
                            // 告知サイズに届かずに終わった = 欠損。保存を確定せず drop で消す。
                            d.keep = res.is_ok() && written >= total;
                            download = None;
                            if let Ok(mut s) = pending_downloads.lock() {
                                s.remove(&id);
                            }
                            let ev = match res {
                                Ok(()) if written < total => {
                                    tracing::warn!(
                                        id,
                                        written,
                                        total,
                                        "download ended short of declared size"
                                    );
                                    ClientEvent::FileResult {
                                        id,
                                        ok: false,
                                        message: format!(
                                            "incomplete download: {written}/{total} bytes"
                                        ),
                                    }
                                }
                                Ok(()) => {
                                    tracing::info!(id, written, path = %path, "download complete");
                                    ClientEvent::FileResult {
                                        id,
                                        ok: true,
                                        message: path,
                                    }
                                }
                                Err(e) => ClientEvent::FileResult {
                                    id,
                                    ok: false,
                                    message: format!("flush failed: {e}"),
                                },
                            };
                            sink.send_event(ev);
                        }
                    }
                }
                _ => tracing::warn!(id, "download chunk without matching offer, ignored"),
            },
            // --- ProjFS ドライブリダイレクト: 作業タスクへ回す (fs_worker_loop 参照) ---
            msg @ (ControlMsg::FsList { .. }
            | ControlMsg::FsRead { .. }
            | ControlMsg::FsCreate { .. }
            | ControlMsg::FsWrite { .. }
            | ControlMsg::FsDelete { .. }
            | ControlMsg::FsRename { .. }) => {
                // 作業タスクが居ない (セッション終了中) なら捨てるだけ。
                let _ = fs_req_tx.send(msg).await;
            }
            other => tracing::debug!(?other, "ignored control msg"),
        }
    }
}

/// ProjFS 要求 (ホスト → クライアント) を受信順に 1 件ずつ処理する。列挙/読込はディスク次第で
/// 秒単位かかるため、tcp_reader_loop で直接 await すると Pong が読めず PONG_TIMEOUT で偽の切断に
/// なる。書き戻しは順序依存 (Create→Write→Rename) なので並列化せず、この 1 タスクで直列に流す。
/// 共有スコープが Disabled なら resolve が error を返し、read は error 応答・破壊系はログのみで
/// 無視する (無断アクセス防止)。WholePc なら This PC 全体を投影する (書き戻しは writable のときだけ)。
async fn fs_worker_loop(
    mut rx: mpsc::Receiver<ControlMsg>,
    share_scope: Arc<Mutex<ShareScope>>,
    fs_reply_tx: mpsc::UnboundedSender<ControlMsg>,
) {
    // 読み取り専用共有で書き戻しを拒否した旨を記録済みか。ホストは保存のたびに FsWrite を
    // チャンク単位で連投するため、毎回 warn するとログが埋まる。
    let mut read_only_logged = false;
    while let Some(msg) = rx.recv().await {
        match msg {
            ControlMsg::FsList {
                req_id,
                path,
                offset,
            } => {
                let scope = share_scope.lock().await.clone();
                let reply = fs_list(&scope, &path, offset).await;
                let _ = fs_reply_tx.send(reply.into_msg(req_id));
            }
            ControlMsg::FsRead {
                req_id,
                path,
                offset,
                length,
            } => {
                let scope = share_scope.lock().await.clone();
                let (data, error) = fs_read(&scope, &path, offset, length).await;
                let _ = fs_reply_tx.send(ControlMsg::FsReadReply {
                    req_id,
                    data,
                    error,
                });
            }
            // --- 書き戻し (ホスト Explorer → クライアント、一方向) ---
            ControlMsg::FsCreate { path, is_dir } => {
                let scope = share_scope.lock().await.clone();
                if let Err(e) = fs_create(&scope, &path, is_dir).await {
                    log_write_back_failure(&mut read_only_logged, "FsCreate", &path, &e);
                }
            }
            ControlMsg::FsWrite { path, offset, data } => {
                let scope = share_scope.lock().await.clone();
                if let Err(e) = fs_write(&scope, &path, offset, &data).await {
                    log_write_back_failure(&mut read_only_logged, "FsWrite", &path, &e);
                }
            }
            ControlMsg::FsDelete { path } => {
                let scope = share_scope.lock().await.clone();
                if let Err(e) = fs_delete(&scope, &path).await {
                    log_write_back_failure(&mut read_only_logged, "FsDelete", &path, &e);
                }
            }
            ControlMsg::FsRename { from, to } => {
                let scope = share_scope.lock().await.clone();
                if let Err(e) = fs_rename(&scope, &from, &to).await {
                    log_write_back_failure(&mut read_only_logged, "FsRename", &from, &e);
                }
            }
            _ => {}
        }
    }
}

/// 書き戻し失敗のログ。読み取り専用による拒否は想定内なので 1 セッション 1 回だけ記録する。
fn log_write_back_failure(read_only_logged: &mut bool, op: &str, path: &str, error: &str) {
    if error == READ_ONLY_SHARE {
        if !*read_only_logged {
            *read_only_logged = true;
            tracing::info!(op, path = %path, "write-back rejected: share is read-only (further rejections not logged)");
        }
        return;
    }
    tracing::warn!(op, path = %path, error = %error, "write-back failed");
}

/// 接続処理中の登録 ([`SessionState::connecting`]) を、connect_inner のどの return 経路でも外す。
struct ConnectingGuard<'a> {
    state: &'a SessionState,
    session_id: String,
}

impl Drop for ConnectingGuard<'_> {
    fn drop(&mut self) {
        self.state
            .connecting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.session_id);
    }
}
