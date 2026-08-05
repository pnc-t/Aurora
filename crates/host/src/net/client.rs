//! 1 接続の処理: Noise ハンドシェイク〜UDP hello〜映像/音声/入力/制御/ファイル転送のセッションループ。
use super::*;

/// tray 表示 (ツールチップ/バルーン) 用の端末ラベルを作る。クライアント申告の端末名を
/// 制御文字除去・trim・長さ制限してそのまま使い、空なら peer IP へフォールバックする。
/// 申告名は任意値なので表示専用に整える (認証は公開鍵で行い、ここでは身元保証に使わない)。
fn display_label(device_name: &str, ip: std::net::IpAddr) -> String {
    let cleaned: String = device_name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(40)
        .collect();
    if cleaned.is_empty() {
        ip.to_string()
    } else {
        cleaned
    }
}

/// アップロード異常終了時の書きかけファイルを削除する。正常完了ファイルと同名で
/// 不完全データが残り、ユーザーが破損ファイルを正常品と誤認するのを防ぐため。
/// 削除失敗は元のアップロードエラーを潰さないようログのみに留める。
async fn cleanup_aborted_upload(path: &str) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        tracing::warn!(path = %path, error = %e, "failed to remove aborted upload file");
    }
}

pub(crate) async fn handle_client(mut stream: TcpStream, args: ClientArgs) -> Result<()> {
    let ClientArgs {
        peer,
        psk,
        audio,
        mac,
        udp,
        enc_tx,
        input_tx,
        cap_select_tx,
        fps_tx,
        scale_tx,
        cursor_tx,
        clip_apply_tx,
        nal_btx,
        audio_btx,
        throttle,
        devices,
        shared,
        session_sem,
        registry,
        takeover_pending,
        display_op,
        conn_id,
        force_frame,
        tray_bus,
        hello_registry,
    } = args;
    // Noise XX-PSK レスポンダ。host 自身の静的鍵は使い捨て (None) でよい。デバイス識別は
    // 相手 (クライアント) の永続公開鍵 = remote_static で行う。
    let mut hs = HandshakeState::new_responder(&psk, None)?;
    let mut buf = Vec::new();
    let mut tmp = Vec::new();

    // ハンドシェイクの各 I/O にタイムアウトを設ける。これが無いと、認証前のクライアントが
    // TCP 接続だけして何も送らないとここで永久にブロックし、単一クライアントモデルでは
    // accept ループが進まず正規ユーザーまで締め出される (認証前 DoS)。
    const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    // client 側 ping ループ (`crates/client/src-tauri/src/net/connect.rs` の ping_task) は
    // 1 秒間隔で**無条件に** (映像一時停止中・バックグラウンド遷移中でも) Ping を送り続けるため、
    // host 側もこれを生存証明として使える。機内モード等で FIN/RST を出さずにネットワークを失うと
    // `read_tcp_frame` (reader_task 内) が永久にブロックし、MAX_CLIENTS の枠占有・ProjFS 単独所有の
    // 固着・HeldInputs 解放漏れが起きる (後始末セクションに到達しないため)。無応答を検知して通常の
    // 切断経路 (後始末を必ず通る break) へ抜ける。
    // 猶予は client 側の PONG_TIMEOUT (5 秒) より意図的に長くとる。host が先に見切ると、client が
    // まだ生きていると思っている接続を一方的に落とすことになり、モバイルの数秒のストールで
    // 毎回セッションが飛ぶ。ここは「本当に死んだ相手の枠を解放する」のが目的なので緩くてよい。
    const PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    // ハンドシェイク 3 メッセージ + transport 確立を 1 ブロックにまとめ、失敗時に遅延を挟む。
    // 相手 (クライアント) の静的公開鍵は transport 確立で hs が消費される前に控える
    // (デバイスピン留めの許可リスト照合に使う)。
    let (session, remote_key, device_name) = match async {
        // <- e
        tokio::time::timeout(HANDSHAKE_TIMEOUT, read_tcp_frame(&mut stream, &mut buf))
            .await
            .context("handshake timed out (e)")??;
        hs.read_msg(&buf, &mut tmp)?;
        // -> e, ee, s, es
        let mut out_msg = Vec::new();
        hs.write_msg(&[], &mut out_msg)?;
        tokio::time::timeout(HANDSHAKE_TIMEOUT, write_tcp_frame(&mut stream, &out_msg))
            .await
            .context("handshake timed out (e,ee,s,es)")??;
        // <- s, se, psk
        tokio::time::timeout(HANDSHAKE_TIMEOUT, read_tcp_frame(&mut stream, &mut buf))
            .await
            .context("handshake timed out (s,se,psk)")??;
        hs.read_msg(&buf, &mut tmp)?;
        let remote_key = hs.remote_static();
        // 最終メッセージの (暗号化済み) payload にクライアントが載せた端末名。デバイスピン留めの
        // 登録ラベルに使い「どの PC を登録したか」を判別できるようにする (空 = 旧クライアントは
        // host 側で既定ラベル device-xxxx にフォールバック)。値はクライアント任意なので devices で
        // サニタイズ・長さ制限する。机上のラベル用途のみで認証には使わない。
        let device_name = String::from_utf8_lossy(&tmp).into_owned();
        // snow の StatelessTransportState は encrypt/decrypt とも &self なので Arc<Mutex> は
        // 不要 (映像 UDP のパケット毎ロックを排除)。
        let session = hs.into_transport().map_err(anyhow::Error::from)?;
        Ok::<_, anyhow::Error>((session, remote_key, device_name))
    }
    .await
    {
        Ok(triple) => triple,
        Err(e) => {
            // 認証/ハンドシェイク失敗。snow は PSK 不一致を最終メッセージの復号失敗として
            // 返すため、ここに来る = パスワード誤りかプロトコル不正。送信元 IP 単位の指数
            // バックオフ分だけ切断前に待たせ、ロックアウトの無い単一要素パスワードへの
            // オンライン総当たりレートを落とす (正規ユーザーの成功パスは遅延しない)。
            let delay = throttle.record_failure(peer.ip());
            tracing::warn!(peer = %peer, error = %e, delay_ms = delay.as_millis() as u64, "handshake failed; throttling source IP");
            tokio::time::sleep(delay).await;
            return Err(e);
        }
    };

    // デバイスピン留め (有効時のみ): PSK は正しくても、登録済み端末以外を拒否する 2 要素目。
    // PSK 漏洩でも「登録した端末の秘密鍵」を持たない相手を弾く。判定は record_success の前に
    // 行い、拒否はバックオフを伸ばす (= 履歴を消さない) ことで未許可端末の連投も減速させる。
    let admit = devices.admit(remote_key.as_deref(), Some(device_name.as_str()));
    match &admit {
        crate::security::Admit::Rejected => {
            // PSK は正しいが登録外。送信元 IP 単位の指数バックオフで未登録端末の連投も減速。
            let delay = throttle.record_failure(peer.ip());
            tracing::warn!(peer = %peer, delay_ms = delay.as_millis() as u64, "device not in allowlist; rejecting (device pinning)");
            tokio::time::sleep(delay).await;
            return Err(anyhow::anyhow!("device not in allowlist"));
        }
        crate::security::Admit::PendingApproval => {
            // 承認待ち: この接続は登録もアクセスも与えず切断する。オーナーがトレイで指紋を照合して
            // 承認すれば、その端末は次回接続で Trusted になる (レースで攻撃者が割り込んでも指紋不一致で
            // 弾ける)。指紋+端末名を tray へ通知して承認 UI で照合できるようにする。throttle は掛けない
            // (正規端末の再接続を遅らせない。保留リストの上限管理は devices 側)。
            let fp = aurora_common::noise::device_fingerprint(remote_key.as_deref().unwrap_or(&[]));
            tracing::info!(peer = %peer, fingerprint = %fp, "device pending owner approval (pairing)");
            tray_bus.on_pending_device(fp, display_label(&device_name, peer.ip()));
            // 承認待ちを client が「パスワード誤り」と区別できるよう、暗号化セッション上で 1 度だけ合図を
            // 送る(nonce 0 = このセッション最初の CTRL フレーム。client は最初のフレームを nonce 0 で読む)。
            // ベストエフォート: 送信失敗しても従来どおり切断する(切断挙動・throttle 無し・tray 通知は不変)。
            let mut tx_nonce = 0u64;
            let _ = send_ctrl_msg(
                &mut stream,
                &session,
                &ControlMsg::PendingApproval,
                &mut tx_nonce,
            )
            .await;
            return Err(anyhow::anyhow!("device pending owner approval"));
        }
        _ => {}
    }
    // 接続中であることを名前付きイベントで公開する。トレイ/CLI はこれを見て、接続中の
    // パスワード変更 (リモートからの締め出し/乗っ取り) を拒否する。PSK + デバイスピン留めの
    // 認証を通過した後でのみ立てる (未認証の相手にハンドシェイクを引き延ばさせて is_active を
    // 立てさせると、持ち主自身がトレイ/CLI から締め出されてしまうため)。セッション終了 (Drop)
    // で当該ハンドルが閉じる。名前付きイベントは「ハンドルが 1 つでも開いていれば生存」する
    // ため、各接続が各々ガードを持つだけで「接続数 ≥ 1 の間 is_active=true」が成立する。
    let _active = crate::net::ActiveGuard::new();
    // 認証成功 (PSK + デバイス)。この IP の連続失敗履歴を消す (誤入力後に正しく入れた正規
    // ユーザーを罰しない)。
    throttle.record_success(peer.ip());
    // 認証成功。ここで初めて「確立済みセッション枠」(MAX_CLIENTS) を確保する。認証前
    // (ハンドシェイク中・失敗のバックオフ sleep 中) はこの scarce な枠に触れないため、未認証接続が
    // 正規セッションの枠を食い潰す DoS を防げる。枠が満杯なら認証済みでも丁寧に断る。
    let _session_permit = match session_sem.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            tracing::warn!(peer = %peer, max_clients = super::MAX_CLIENTS, "session slots full; rejecting after successful handshake");
            return Err(anyhow::anyhow!("session slots full"));
        }
    };
    // tray のツールチップ/バルーン通知に出す表示ラベル (端末名 or peer IP)。確立と切断で同じ
    // ラベルを使い、通知の対を揃える。
    let peer_label = display_label(&device_name, peer.ip());
    tracing::info!(peer = %peer, "noise handshake complete");

    // 映像 UDP 送信を専用タスクへ分離するため session を Arc 共有する。StatelessTransportState は
    // encrypt/decrypt とも &self なので、制御ループと映像送信タスクが同じ session を並行参照できる
    // (nonce は呼び出し側が明示・ドメイン分離済みで衝突しない)。
    let session = std::sync::Arc::new(session);

    // TCP を読み書きの 2 ハーフに分割する。読み取りは専用タスクへ隔離し、本体の select
    // ループからは外す。理由: `read_tcp_frame` は `read_exact` を 2 回行う複数段の await で
    // **キャンセル安全ではない**。これを select の枝に直接置くと、NAL 送信や FS 要求など別の
    // 枝が ready になった瞬間に「長さは読んだが本体は途中」の future が drop され、TCP から
    // 消費済みのバイトが失われてフレーム境界がズレる (= decrypt error / read len で切断)。
    // 専用タスク内ならフレーム読み取りは決して途中キャンセルされない。読み取った生フレームは
    // bounded チャネルで本体ループへ渡し、select 側はキャンセル安全な `recv()` だけにする。
    let (mut read_half, mut write_half) = stream.into_split();
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(64);
    let reader_task = tokio::spawn(async move {
        let mut rbuf = Vec::new();
        loop {
            if read_tcp_frame(&mut read_half, &mut rbuf).await.is_err() {
                break; // EOF / 切断 / フレーム超過 → 読み取り終了 (frame_tx drop で本体へ伝播)
            }
            if frame_tx.send(std::mem::take(&mut rbuf)).await.is_err() {
                break; // 本体ループ終了
            }
        }
    });
    // 本体ループ終了時に読み取りタスクを確実に止める (セッション置換でのリーク防止)。
    let _reader_guard = AbortOnDrop(reader_task);

    // TCP 制御チャネルの nonce カウンタ (CTRL ドメイン)。送受で別カウンタを持ち、
    // TCP の順序保証により対向と歩調が合う。UDP メディアとは別ドメインなので衝突しない。
    let mut tcp_tx_nonce: u64 = 0;
    let mut tcp_rx_nonce: u64 = 0;

    // HostInfo を即時送信 (このクライアントへ全ディスプレイを提示)。トポロジは共有 (SharedControl)
    // で、capture が追従する単一の active_idx を全クライアントで共有する (共有ミラー)。
    //
    // **セキュリティゲート**: 仮想ディスプレイ (RDP 風テイクオーバー = 物理を暗転させる特権操作) は
    // **ホストに登録済み (許可リスト) の端末だけ**に許可する。未登録端末には HostInfo で仮想ディス
    // プレイ (合成エントリ) を一切見せず is_virtual も立てない (SelectDisplay/SetClientViewport の
    // テイクオーバー経路にも入れない)。接続レベルのピン留め (devices.admit) が無効でも独立に効く。
    let client_trusted = devices.is_trusted(remote_key.as_deref());
    let host_info = {
        let sc = shared.lock().await;
        let displays = sc
            .displays
            .iter()
            .filter_map(|d| {
                let is_virtual = sc.vdd_gdi_name.as_deref() == Some(d.name.as_str());
                // 未登録端末には仮想ディスプレイ (合成エントリ) を提示しない。
                if is_virtual && !client_trusted {
                    return None;
                }
                let mut pd = Display::from(d);
                pd.is_virtual = is_virtual;
                Some(pd)
            })
            .collect();
        ControlMsg::HostInfo {
            displays,
            audio,
            mac,
        }
    };
    send_ctrl_msg(&mut write_half, &session, &host_info, &mut tcp_tx_nonce).await?;
    // 途中参加クライアントへ現在の送出コーデックを通知する (このクライアントが初回の
    // SetSupportedCodecs を送る前に届く NAL がどのコーデックかを知らせる)。
    {
        let codec = shared.lock().await.current_video_codec;
        send_ctrl_msg(
            &mut write_half,
            &session,
            &ControlMsg::VideoCodecChanged { codec },
            &mut tcp_tx_nonce,
        )
        .await?;
    }

    // UDP 対向アドレス確定待ち。クライアントは最初に 8B の "AURHELLO" を送る想定。
    // ハンドシェイクを完了した TCP ピアと同一 IP からの hello のみ受理する。これが無いと、
    // 任意の送信元が AURHELLO を送るだけで映像の宛先を奪える (正規クライアントを締め出す
    // ストリーム乗っ取り DoS)。さらに送信元 IP を詐称した単発 hello で、ホストが高ビットレート
    // 映像を被害者へ流し続けるリフレクション増幅にも悪用できる。bind が 0.0.0.0 で全 NIC
    // 公開のため Tailnet 外からも到達しうる。IP 一致の要求でこの両方を遮断する
    // (register_hello は expected_ip = 自分の TCP peer IP でのみ登録する)。
    let mut hello_rx = register_hello(&hello_registry, peer.ip(), conn_id);
    let peer_udp_first = wait_udp_hello(&mut hello_rx).await?;
    tracing::info!(peer_udp = %peer_udp_first, "UDP peer registered");
    // client 側の UDP 自己修復 (TCP 生存中の UDP 途絶検知 + AURHELLO 再送) に対応するため、
    // 対向アドレスは固定値ではなく共有可変にする。再送 hello が同じ IP の別ポートから届いたら
    // (NAT の UDP マッピング変更等) ここを更新し、映像/音声送信が新アドレス宛に切り替わる。
    let peer_udp = std::sync::Arc::new(std::sync::Mutex::new(peer_udp_first));
    // ここまで来たら TCP 認証 + UDP 対向確定 = 接続確立。映像配信を開始する地点。
    tracing::info!("client connected");
    // tray へ確立を通知する (接続中バルーン + ツールチップ/メニューの「接続中」表示)。以降は
    // 早期 return せずセッション後始末を必ず通るため、ここの on_connected と後始末の
    // on_disconnected が必ず対になる (接続数のカウントがズレない)。
    tray_bus.on_connected(peer_label.clone());

    let mut upload: Option<UploadState> = None;
    // ダウンロード (ホスト → クライアント) のチャンクと、host→クライアントへの制御 push
    // (DisplayResized / Clipboard 等、登録簿経由) を select ループへ流す出口。送信タスク/他接続が
    // ここに積み、ループが自分の Noise セッションで暗号化して TCP へ書く (書き込みは単一箇所に集約)。
    let (out_tx, mut out_rx) = mpsc::channel::<ControlMsg>(64);

    // 確立: 接続数を数え、登録簿へ自分の制御出口を登録する。ProjFS ドライブは固定パスのため
    // 最初に確立した 1 本だけが所有する (複数 mount は衝突する)。
    // さらに **ファイル共有ドライブは「ホストに登録済みの端末」だけに許可する** (仮想ディスプレイと
    // 同じ信頼ゲート client_trusted)。未登録端末は共有フォルダを設定していてもドライブを mount
    // しない。登録は devices のペアリング窓経由 (接続レベルのピン留め enforcement とは独立に効く)。
    let is_projfs_owner = {
        let mut sc = shared.lock().await;
        sc.active_conns += 1;
        // 未申告 = H264 とみなす (最初の SetSupportedCodecs 到着まではネゴシエーションを
        // 保守的に保つ)。到着したら下の select ループの SetSupportedCodecs アームで更新する。
        sc.client_codecs.insert(conn_id, vec![VideoCodec::H264]);
        if client_trusted && sc.projfs_owner.is_none() {
            sc.projfs_owner = Some(conn_id);
            true
        } else {
            false
        }
    };
    registry
        .lock()
        .expect("registry poisoned")
        .insert(conn_id, out_tx.clone());

    // ProjFS ドライブリダイレクト。コールバック (同期) → ここへ FsList/FsRead 要求が流れる。
    // 所有者でない接続は mount しない (要求も発生しない)。開始失敗 (Client-ProjFS 無効など) は
    // ファイル投影を諦めてリモデス本体は継続する。bounded: 書き戻し (FsWrite) の殺到で制御
    // チャネルとメモリを溢れさせないようにする (FsBridge が満杯時に blocking_send で ProjFS
    // スレッドを待たせ、ディスク読みを止める)。
    let (fs_req_tx, mut fs_req_rx) = mpsc::channel::<ControlMsg>(64);
    let fs_bridge = Arc::new(FsBridge::new(fs_req_tx));
    let _virt = if is_projfs_owner {
        // 投影ルートは CWD 相対ではなく固定の絶対パス (`%LOCALAPPDATA%\Aurora\remote`)。
        // セッション中だけ This PC にドライブとして現れ、切断 (Drop) で消える。
        let remote_root = projfs::remote_root();
        match projfs::start(fs_bridge.clone(), &remote_root) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(error = %e, "ProjFS drive redirection disabled");
                None
            }
        }
    } else if !client_trusted {
        tracing::info!("ProjFS drive redirection skipped (device not registered)");
        None
    } else {
        tracing::info!("ProjFS drive redirection skipped (another client owns it)");
        None
    };

    // 映像/音声 broadcast を subscribe する (確立後 = ハンドシェイク中のフレームを溜めない)。
    // nal_brx は下の専用映像送信タスクへ move する (そこで mut 受信)。
    let nal_brx = nal_btx.subscribe();
    let mut audio_brx = audio_btx.subscribe();

    // 接続直後に IDR を 1 枚要求し、最初のキーフレームを待つ黒画面時間を縮める。従来は
    // クライアントが delta を 1 枚受けてから RequestKeyframe を送り返す往復ぶん遅れていた。
    // 共有エンコーダだが、他クライアントには余分な keyframe が 1 枚増えるだけで無害。
    // あわせて capture に現在の画面の強制送出を促す。画面静止中は DXGI が新フレームを返さず
    // キーフレーム要求だけでは映像が生成されないため (接続直後ずっと黒くなる問題の本対策)。
    force_frame.store(true, std::sync::atomic::Ordering::Release);
    let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;

    // セッション中に押下されたまま解放されていないキー/ボタンの追跡。切断時 (正常・異常
    // どちらでも) に解放イベントを注入し、ネットワーク断の瞬間に押していたキーがホスト側で
    // 押しっぱなしになる (stuck key) のを防ぐ。本体ループを async ブロックに包むのは、
    // ループ内の `?` による早期 return でもこの後始末を必ず通すため。クライアント毎ローカル。
    let mut held = HeldInputs::default();
    // 音声ミュート状態 (クライアントの SetAudioMuted)。true の間は音声 UDP を送らない。
    // クライアント毎ローカル (各自が自分の送出のみ gate し、共有エンコーダは止めない)。
    let mut audio_muted = false;
    // 直近に送った音声 seq。音声 nonce も `NONCE_DOMAIN_AUDIO | au.seq` で seq を下位に使うため、
    // au.seq (u32) が一周すると nonce が再利用され ChaChaPoly が致命的に壊れる。映像送信タスクの
    // checked_add ガードと対称に、seq の巻き戻り (broadcast は順序保証なので減少 = u32 wrap) を
    // 検知したら以降の音声送出を止める (再接続で鍵が更新される)。
    let mut last_audio_seq: Option<u32> = None;
    let mut audio_nonce_exhausted = false;
    // この接続が SelectDisplay で仮想 (番兵 id) を明示選択したか。テイクオーバー発動はこれを
    // 必須条件にする (名前一致だけで発動すると、VDD の GDI 名が物理と衝突した際にクライアントが
    // 要求していないテイクオーバーが走る実機事象があった)。
    let mut virtual_selected = false;
    // 映像一時停止 (クライアントの SetVideoPaused)。true の間は映像 UDP を送らない
    // (マルチタブで非アクティブなタブの帯域を止める)。再開時はクライアントが RequestKeyframe を
    // 送って映像を復帰させる。これもクライアント毎ローカル。映像送信は専用タスクで走るため、
    // 制御ループ (ここ) とタスクで共有する atomic にする。
    let video_paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // takeover_pending (テイクオーバー実施中フラグ) は args から受け取る全接続共有の Arc
    // (net/mod.rs で生成)。単一エンコーダ=共有映像なので、テイクオーバー中の物理固まりフレーム
    // 抑止は要求元だけでなく全クライアントの映像送信タスクに効かせる必要があり、ここではローカル
    // Arc を新規に作らない。この接続が SelectDisplay(仮想) で pending=true にした場合は、
    // SetClientViewport 完了時に自分で false へ戻す (下記) のが通常経路だが、その前に本接続が
    // 異常終了すると shared フラグが true のまま残り、全クライアントの映像が永久停止する
    // (このセッションだけの話では済まない退行)。それを防ぐため、pending=true にしたのが
    // 自分かどうかをこのローカル bool で追跡し、後始末で確実に戻す。
    let mut takeover_pending_owned = false;

    // --- 映像 UDP 送信タスク (制御/入力から分離) ---
    // 映像送信を制御ループの select から外し、専用タスクで回す。同じ select に置くと、ドラッグ中の
    // 大量の mousemove (TCP 制御、フレームの ~2 倍/秒) が `nal_brx.recv` と公平に競合し、フレーム送出
    // タイミングがジッタる (= クライアントの到着ジッタ増 → ジッタバッファ肥大 → 体感遅延)。別タスク化
    // するとマルチスレッドランタイムで送信が入力処理と並行に走り、送出が制御トラフィックから独立する。
    // session は &self (StatelessTransportState) なので Arc 共有で安全。seq は映像専用ローカル。
    let video_task = {
        let session = session.clone();
        let udp = udp.clone();
        let enc_tx = enc_tx.clone();
        let video_paused = video_paused.clone();
        let takeover_pending = takeover_pending.clone();
        let peer_udp = peer_udp.clone();
        let mut nal_brx = nal_brx;
        tokio::spawn(async move {
            let mut seq: u32 = 0;
            loop {
                let nal = match nal_brx.recv().await {
                    Ok(n) => n,
                    // 遅延でリングから溢れた: 取りこぼし、キーフレームで復帰させる (共有エンコーダ宛なので
                    // 全クライアントが次キーフレームを得る)。
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "video broadcast lagged; requesting keyframe");
                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                        continue;
                    }
                    // 送信元 (encode → フォワーダ) 終了 = 映像供給停止。送信タスクを畳む。
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                // 一時停止中 (非アクティブタブ) またはテイクオーバー中 (物理の固まりフレームを
                // 送らせない) はドレインするだけで送らない。
                if video_paused.load(std::sync::atomic::Ordering::Relaxed)
                    || takeover_pending.load(std::sync::atomic::Ordering::Relaxed)
                {
                    continue;
                }
                let frame_id = nal.frame_id;
                let keyframe = nal.keyframe;
                let mut chunks = nal.data.chunks(MAX_PAYLOAD).peekable();
                let mut first = true;
                while let Some(chunk) = chunks.next() {
                    let is_last = chunks.peek().is_none();
                    let mut flags = VideoPacketFlags::empty();
                    if first {
                        flags |= VideoPacketFlags::FRAME_START;
                    }
                    if is_last {
                        flags |= VideoPacketFlags::FRAME_END;
                    }
                    if keyframe {
                        flags |= VideoPacketFlags::KEYFRAME;
                    }
                    let pkt = VideoPacket {
                        seq,
                        flags,
                        frame_id,
                        payload: chunk,
                    };
                    let mut framed = Vec::with_capacity(chunk.len() + 7);
                    if let Err(e) = pkt.encode(&mut framed) {
                        tracing::warn!(error = %e, "encode VideoPacket failed; ending video sender");
                        return;
                    }
                    let nonce = NONCE_DOMAIN_VIDEO | seq as u64;
                    let dgram = match encrypt_udp(&session, nonce, STREAM_VIDEO, &framed) {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::warn!(error = %e, "encrypt video failed; ending video sender");
                            return;
                        }
                    };
                    let addr = *peer_udp.lock().expect("peer_udp poisoned");
                    if let Err(e) = udp.send_to(&dgram, addr).await {
                        tracing::warn!(error = %e, "video UDP send failed; ending video sender");
                        return;
                    }
                    // 映像 nonce は seq (u32) を下位に使うため、一周で再利用される。ChaChaPoly は nonce
                    // 再利用が致命的なので巻き戻る前に送信を止める (クライアント再接続で鍵が更新される)。
                    seq = match seq.checked_add(1) {
                        Some(s) => s,
                        None => {
                            tracing::error!(
                                "video nonce space exhausted; ending video sender (reconnect to rekey)"
                            );
                            return;
                        }
                    };
                    first = false;
                }
            }
        })
    };
    // 本体終了 (切断・エラー) で映像送信タスクを確実に止める。
    let _video_guard = AbortOnDrop(video_task);

    // 最後に Ping を受信した時刻。client の ping ループが無条件 (1 秒毎) なので、これだけで
    // 生存証明として十分 (Input 等は静止中・無操作中に途絶えるため無応答判定の根拠にしない)。
    let mut last_ping = std::time::Instant::now();
    let mut ping_watchdog = tokio::time::interval(std::time::Duration::from_secs(1));

    let result: Result<()> = async {
    loop {
        tokio::select! {
            // Ping 無応答ウォッチドッグ。1 秒毎に最後の Ping からの経過を確認し、PING_TIMEOUT を
            // 超えたら通常の切断経路 (下の後始末セクション) へ抜ける。
            _ = ping_watchdog.tick() => {
                if last_ping.elapsed() > PING_TIMEOUT {
                    tracing::warn!(peer = %peer, conn_id, "no ping received; treating as disconnected (network stall?)");
                    break;
                }
            }
            // 制御メッセージ受信 (専用リーダタスクが読んだ生フレームをキャンセル安全に受け取る)
            maybe_frame = frame_rx.recv() => {
                let Some(frame) = maybe_frame else {
                    // リーダタスク終了 = 切断 / 読み取りエラー。セッションを正常終了する。
                    break;
                };
                let mut plain = Vec::new();
                let msg = decode_ctrl_msg(&session, &frame, &mut tcp_rx_nonce, &mut plain)?;
                match msg {
                    ControlMsg::Ping { ts_us } => {
                        last_ping = std::time::Instant::now();
                        send_ctrl_msg(&mut write_half, &session, &ControlMsg::Pong { ts_us }, &mut tcp_tx_nonce).await?;
                    }
                    ControlMsg::RequestKeyframe => {
                        // 画面静止中の再同期でも現在の画面を 1 枚送れるよう capture を促す
                        // (UDP でキーフレームが欠けたまま静止すると、これが無いと回復しない)。
                        force_frame.store(true, std::sync::atomic::Ordering::Release);
                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                    }
                    ControlMsg::SetVideoParams { fps, max_bitrate_bps } => {
                        tracing::info!(fps, max_bitrate_bps, "client set video params");
                        // fps 上限を capture スレッドへ (共有: last-writer-wins)。
                        let _ = fps_tx.send(fps);
                        // 共有エンコーダのビットレート上限を更新し、新しい現在値を即反映。
                        let new_bps = shared.lock().await.bitrate.set_max(max_bitrate_bps);
                        let _ = enc_tx.send(EncoderCommand::SetBitrate(new_bps)).await;
                    }
                    ControlMsg::SetAudioMuted { muted } => {
                        tracing::info!(muted, "client set audio muted");
                        audio_muted = muted;
                    }
                    ControlMsg::SetVideoPaused { paused } => {
                        tracing::info!(paused, "client set video paused");
                        video_paused.store(paused, std::sync::atomic::Ordering::Relaxed);
                    }
                    ControlMsg::SetSupportedCodecs { codecs } => {
                        // 空申告は無視して H264 のみ扱いに倒す (「対応コーデック無し」は
                        // あり得ない前提を守る)。
                        let codecs = if codecs.is_empty() { vec![VideoCodec::H264] } else { codecs };
                        tracing::info!(?codecs, "client declared supported codecs");
                        let (recompute, current) = {
                            let mut sc = shared.lock().await;
                            sc.client_codecs.insert(conn_id, codecs);
                            let recompute = sc.recompute_codec();
                            (recompute, sc.current_video_codec)
                        };
                        if recompute != current {
                            let _ = enc_tx.send(EncoderCommand::SetCodec(recompute)).await;
                        }
                    }
                    ControlMsg::SetCursorOverlay { enabled } => {
                        tracing::info!(enabled, "client set cursor overlay");
                        // 共有映像なので「1 つでも要求があれば合成」。集合を更新して capture へ。
                        let any = {
                            let mut sc = shared.lock().await;
                            if enabled {
                                sc.cursor_overlay.insert(conn_id);
                            } else {
                                sc.cursor_overlay.remove(&conn_id);
                            }
                            !sc.cursor_overlay.is_empty()
                        };
                        let _ = cursor_tx.send(any);
                    }
                    ControlMsg::SetResolution { max_height } => {
                        tracing::info!(max_height, "client set resolution");
                        // capture スレッドへ最大高さを伝える (GPU 縮小)。新キーフレームで即反映。
                        let _ = scale_tx.send(max_height);
                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                    }
                    ControlMsg::Stats { bitrate_kbps, fps, rtt_us, loss_permille } => {
                        tracing::debug!(bitrate_kbps, fps, rtt_us, loss_permille, "client stats");
                        // 共有エンコーダは全クライアントの最悪リンクに合わせる。自分の stats を記録し、
                        // 全クライアントの max(loss)/max(rtt) を適応制御へ与える。
                        let new = {
                            let mut sc = shared.lock().await;
                            sc.client_stats.insert(conn_id, (loss_permille, rtt_us));
                            let (wl, wr) = sc.worst_link();
                            sc.bitrate.on_stats(wl, wr)
                        };
                        if let Some(new_bps) = new {
                            tracing::info!(new_bps, "adapt bitrate (worst-link)");
                            let _ = enc_tx.send(EncoderCommand::SetBitrate(new_bps)).await;
                        }
                    }
                    ControlMsg::Input(ev) => {
                        held.observe(&ev);
                        if input_tx.send(InputCmd::Event(ev)).await.is_err() {
                            tracing::warn!("input channel closed");
                        }
                    }
                    ControlMsg::SelectDisplay { id } => {
                        // ロック順序は必ず display_op → shared (逆順は取らない。デッドロック防止)。
                        // このアームは end_takeover (数十〜数百ms の spawn_blocking) を伴いうるため、
                        // `shared` を保持したまま await せず、ディスプレイ操作専用の直列化ロックで
                        // 他接続のトポロジ操作 (begin_takeover/end_takeover) と排他する。`shared` 自体は
                        // 判断に必要な値を読んだら即座に手放し、Stats 等の低頻度制御パスを止めない。
                        let _display_guard = display_op.lock().await;
                        let (target, need_end_takeover, prev_active_idx, prev_taken_over) = {
                            let sc = shared.lock().await;
                            let target = sc.displays.iter().position(|d| d.id == id);
                            let need_end_takeover = sc.taken_over && id != VIRTUAL_SENTINEL_ID;
                            (target, need_end_takeover, sc.active_idx, sc.taken_over)
                        };
                        match target {
                            Some(idx) => {
                                if id == VIRTUAL_SENTINEL_ID && !client_trusted {
                                    // 未登録端末は仮想ディスプレイ (テイクオーバー) を選べない。
                                    tracing::warn!(id, "rejected virtual display select from untrusted client");
                                } else {
                                    // SetClientViewport のテイクオーバー判定はこのフラグを必須条件にする
                                    // (このアームを正常通過した = 明示的に選択済み)。
                                    virtual_selected = id == VIRTUAL_SENTINEL_ID;
                                    // 仮想選択と同時に送出を止める (この直後の SetClientViewport で
                                    // begin_takeover が走り、その間キャプチャは物理の固まりフレームを
                                    // 返し続けるため)。物理へ戻す/通常選択時は false にして残留させない。
                                    takeover_pending.store(
                                        virtual_selected,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    // 自分が true にしたのなら、後始末で戻す責任も自分が持つ
                                    // (SetClientViewport で false にできなかった場合の保険)。
                                    takeover_pending_owned = virtual_selected;
                                    // テイクオーバー中に物理ディスプレイへ切り替えるなら、先に
                                    // end_takeover で全トポロジを復元 (物理を再アタッチ) する。これを
                                    // 怠ると物理デタッチが残ったまま物理を選ぶことになり暗転が残る (穴③)。
                                    // `shared` を手放して実行する (display_op が他接続のトポロジ操作を
                                    // 排他しているので end_takeover 自体は直列化されている)。
                                    if need_end_takeover {
                                        let _ = tokio::task::spawn_blocking(
                                            crate::display::end_takeover,
                                        )
                                        .await;
                                    }
                                    // shared を取り直して適用する。display_op を保持し続けているため
                                    // 他接続のトポロジ操作はこの間走れないはずだが、念のため手放していた
                                    // 間に active_idx/taken_over が変わっていないか検証してから適用する。
                                    let mut sc = shared.lock().await;
                                    if need_end_takeover {
                                        if sc.active_idx != prev_active_idx
                                            || sc.taken_over != prev_taken_over
                                        {
                                            tracing::warn!(
                                                id,
                                                "display topology changed while ending takeover; discarding end_takeover result"
                                            );
                                        } else {
                                            sc.taken_over = false;
                                            tracing::info!(
                                                "ended takeover: switching to physical display"
                                            );
                                        }
                                    }
                                    // 物理ディスプレイの解像度は変更しないので、切替時の復元は不要。
                                    sc.active_idx = idx;
                                    let d = sc.displays[idx].clone();
                                    tracing::info!(id, name = %d.name, "select display");
                                    // 合成した仮想ディスプレイ (番兵 id) はまだ DXGI 出力が無い
                                    // (デタッチ中)。ここで cap_select すると EnumOutputs が外れて
                                    // 失敗するので送らない。直後の SetClientViewport で begin_takeover
                                    // がアタッチし、resolve_output が実 index で cap_select+座標系を
                                    // 張り替える。実 index を持つ通常ディスプレイのみ即切替する。
                                    if id != VIRTUAL_SENTINEL_ID {
                                        // capture スレッドへ切替指示 (GDI 名で。index はトポロジ変更で
                                        // 変わるため名前で引き直す)。
                                        let _ = cap_select_tx.send(d.name.clone());
                                        // input スレッドの座標系を新ディスプレイへ更新。
                                        let geom = DisplayGeom {
                                            x: d.x,
                                            y: d.y,
                                            width: d.width,
                                            height: d.height,
                                        };
                                        let _ = input_tx.send(InputCmd::SetDisplay(geom)).await;
                                        // 新キャプチャの最初の出力を keyframe にしてクライアント復帰を促す。
                                        let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                                    }
                                }
                            }
                            None => tracing::warn!(id, "SelectDisplay for unknown display id"),
                        }
                    }
                    ControlMsg::SetClientViewport { width, height } => {
                        // クライアントの表示領域にホスト解像度を合わせる。0 は無視。
                        // アクティブが仮想ディスプレイなら RDP 風テイクオーバー (物理オフ+仮想 primary)、
                        // そうでなければ物理ディスプレイの解像度変更 (方式B) を行う。
                        if width > 0 && height > 0 {
                            // ロック順序は必ず display_op → shared。begin_takeover (数十〜数百ms の
                            // spawn_blocking) を `shared` を保持したまま await すると、他接続の Stats
                            // 等の低頻度制御パスまで丸ごと止まるため、`shared` は判断に必要な値を
                            // 読んだら手放し、操作自体はディスプレイ操作専用の直列化ロックで排他する。
                            let _display_guard = display_op.lock().await;
                            let (ai, active_name, is_virtual, prev_taken_over) = {
                                let sc = shared.lock().await;
                                let ai = sc.active_idx;
                                let active_name = sc.displays[ai].name.clone();
                                // 仮想ディスプレイのテイクオーバーは登録済み端末のみ (特権ゲート)、かつ
                                // この接続が SelectDisplay で仮想を明示選択済みのときのみ発動する。名前
                                // 一致だけで判定すると、VDD の GDI 名が物理と衝突した場合にクライアントが
                                // 要求していないテイクオーバー (物理暗転) が走る実機事象があったため。
                                let is_virtual = client_trusted
                                    && virtual_selected
                                    && sc.vdd_gdi_name.as_deref() == Some(active_name.as_str());
                                (ai, active_name, is_virtual, sc.taken_over)
                            };
                            // 「リサイズで固まる」のライブ再現用 (RUST_LOG=aurora=debug)。どの viewport が
                            // いつ来てテイクオーバー/リサイズのどちらに入ったかを追える。
                            tracing::info!(
                                width,
                                height,
                                taken_over = prev_taken_over,
                                is_virtual,
                                virtual_selected,
                                active = %active_name,
                                "SetClientViewport"
                            );
                            // begin_takeover/detect は `shared` を手放した状態で実行する
                            // (display_op が他接続のトポロジ操作を排他しているので直列性は保たれる)。
                            let mut applied: Option<(String, i32, i32, u32, u32)> = None;
                            if is_virtual && !prev_taken_over {
                                // --- 仮想ディスプレイ: テイクオーバー ---
                                let vn = active_name.clone();
                                let r = tokio::task::spawn_blocking(move || {
                                    crate::display::begin_takeover(&vn, width, height)
                                })
                                .await;
                                match r {
                                    Ok(Ok((vx, vy, vw, vh))) => {
                                        // CCD で物理を落とすと仮想の GDI 名が変わることがあるため
                                        // MTT1337 で再検出し、capture 対象 (名前ベース) を更新する。
                                        let new_name =
                                            tokio::task::spawn_blocking(crate::display::detect)
                                                .await
                                                .ok()
                                                .flatten()
                                                .map(|v| v.gdi_name)
                                                .unwrap_or_else(|| active_name.clone());
                                        applied = Some((new_name, vx, vy, vw, vh));
                                    }
                                    Ok(Err(e)) => {
                                        tracing::warn!(error = %e, "begin_takeover failed");
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "begin_takeover task failed");
                                    }
                                }
                            } else if is_virtual {
                                // テイクオーバー中のリサイズ追従 (仮想解像度の動的変更) は**行わない**。
                                // アクティブな Desktop Duplication 中に仮想ディスプレイの解像度を
                                // 変えると、duplication が回復不能の DXGI_ERROR_ACCESS_LOST に陥り
                                // (同じ共有 D3D11 デバイス上では作り直しても復帰しない)、テイクオーバー中の
                                // re-exec 抑止と相まって映像が永久固着する (実機ログ 2026-06-19 で確定)。
                                // 解像度は接続時テイクオーバーで一度だけ確定し、以降の窓サイズ差は
                                // クライアントの object-fit が吸収する。再フィットしたい場合は再接続する。
                                tracing::debug!(
                                    width,
                                    height,
                                    "resize ignored during takeover (virtual resolution is fixed at connect to avoid capture ACCESS_LOST)"
                                );
                            }
                            // shared を取り直して適用する。display_op を保持し続けているため他接続の
                            // トポロジ操作はこの間走れないはずだが、念のため手放していた間に active_idx/
                            // taken_over が変わっていないか検証してから適用する (変わっていたら
                            // begin_takeover の結果は捨てて warn する。takeover_pending は下で必ず false
                            // に戻すので送出は固着しない)。
                            if let Some((new_name, vx, vy, vw, vh)) = applied {
                                let mut sc = shared.lock().await;
                                if sc.active_idx != ai || sc.taken_over != prev_taken_over {
                                    tracing::warn!(
                                        id = ai,
                                        "display topology changed during begin_takeover; discarding takeover result"
                                    );
                                } else {
                                    sc.taken_over = true;
                                    // 仮想の**実位置**を入力座標系に使う。stage2 の (0,0) 移動が
                                    // 失敗すると仮想は右端 (例 5120,0) に残るため、決め打ち (0,0)
                                    // だとマウスがずれる (実機で確認)。begin_takeover が読み戻した
                                    // 実ジオメトリ (vx,vy,vw,vh) を信頼する。
                                    sc.displays[ai].name = new_name.clone();
                                    sc.displays[ai].x = vx;
                                    sc.displays[ai].y = vy;
                                    sc.displays[ai].width = vw;
                                    sc.displays[ai].height = vh;
                                    let d = sc.displays[ai].clone();
                                    drop(sc);
                                    let _ = cap_select_tx.send(new_name);
                                    let geom = DisplayGeom {
                                        x: d.x,
                                        y: d.y,
                                        width: vw,
                                        height: vh,
                                    };
                                    let _ = input_tx.send(InputCmd::SetDisplay(geom)).await;
                                    // 解像度追従を全クライアントへ配信する (要求元以外の canvas も合わせる)。
                                    broadcast_ctrl(
                                        &registry,
                                        &ControlMsg::DisplayResized {
                                            id: d.id,
                                            width: vw,
                                            height: vh,
                                        },
                                    );
                                    let _ = enc_tx.send(EncoderCommand::RequestKeyframe).await;
                                    tracing::info!(id = d.id, vw, vh, "virtual display takeover/resize");
                                }
                            }
                            // is_virtual 判定に関わらず、この viewport 処理を抜ける時点で
                            // テイクオーバーは完了 (begin_takeover は上のブロックで await 済み) または
                            // 非該当なので、送出停止を必ず解除する。ここより上で早期リターンしないため
                            // is_virtual=false 経路や失敗時にも必ず到達し、直前の SelectDisplay で立てた
                            // takeover_pending=true が残って送出が永久固着することはない。
                            takeover_pending.store(false, std::sync::atomic::Ordering::Relaxed);
                            takeover_pending_owned = false;
                            // 物理ディスプレイのキャプチャ時はホスト解像度を一切変更しない
                            // (はみ出しはクライアント側の object-fit / レターボックスで吸収する)。
                        }
                    }
                    ControlMsg::Clipboard { text } => {
                        if clip_apply_tx.send(text).is_err() {
                            tracing::warn!("clipboard apply channel closed");
                        }
                    }
                    ControlMsg::FileOffer { id, name, size } => {
                        match open_upload(&name).await {
                            Ok((file, path)) => {
                                tracing::info!(id, name = %name, size, path = %path, "file upload start");
                                upload = Some(UploadState { id, file, written: 0, total: size, path });
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "open upload failed");
                                let msg = ControlMsg::FileResult { id, ok: false, message: e.to_string() };
                                send_ctrl_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                            }
                        }
                    }
                    ControlMsg::FileChunk { id, offset, data, last } => {
                        match upload.as_mut() {
                            Some(u) if u.id == id => {
                                if offset != u.written {
                                    // TCP は順序保証があるため offset 不一致は本来起きない。発生したら
                                    // データ欠落/重複の可能性が高く、書き込むと破損ファイルを残すため中断する。
                                    let message = format!("chunk offset mismatch: expected {}, got {offset}", u.written);
                                    tracing::warn!(id, expected = u.written, got = offset, "chunk offset mismatch, aborting upload");
                                    cleanup_aborted_upload(&u.path).await;
                                    upload = None;
                                    let msg = ControlMsg::FileResult { id, ok: false, message };
                                    send_ctrl_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                                } else if u.written.saturating_add(data.len() as u64) > u.total {
                                    // 告知サイズ (FileOffer.size) を超える書き込みは拒否する。これが無いと
                                    // size:0 を告知してから無限にチャンクを送り、ホストのディスクを埋められる
                                    // (total の検証が抜けており実質デッドだった)。
                                    let message = format!("upload exceeds declared size {}", u.total);
                                    tracing::warn!(id, total = u.total, written = u.written, got = data.len(), "upload exceeds declared size, aborting");
                                    cleanup_aborted_upload(&u.path).await;
                                    upload = None;
                                    let msg = ControlMsg::FileResult { id, ok: false, message };
                                    send_ctrl_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                                } else if let Err(e) = u.file.write_all(&data).await {
                                    let message = format!("write failed: {e}");
                                    tracing::warn!(id, error = %e, "file write failed");
                                    cleanup_aborted_upload(&u.path).await;
                                    upload = None;
                                    let msg = ControlMsg::FileResult { id, ok: false, message };
                                    send_ctrl_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                                } else {
                                    u.written += data.len() as u64;
                                    if last {
                                        let (written, total, path) = (u.written, u.total, u.path.clone());
                                        let res = u.file.flush().await;
                                        upload = None;
                                        let msg = match res {
                                            Ok(()) => {
                                                tracing::info!(id, written, total, path = %path, "file upload complete");
                                                ControlMsg::FileResult { id, ok: true, message: path }
                                            }
                                            Err(e) => {
                                                tracing::warn!(id, error = %e, "file flush failed");
                                                cleanup_aborted_upload(&path).await;
                                                ControlMsg::FileResult { id, ok: false, message: format!("flush failed: {e}") }
                                            }
                                        };
                                        send_ctrl_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
                                    }
                                }
                            }
                            _ => {
                                tracing::warn!(id, "file chunk without matching offer, ignored");
                            }
                        }
                    }
                    ControlMsg::FileRequest { id, path } => {
                        tracing::info!(id, path = %path, "file download requested");
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            stream_file_to_client(id, &path, out_tx).await;
                        });
                    }
                    // ProjFS コールバックへの応答。相関 ID で待機中のコールバックを起こす。
                    ControlMsg::FsListReply { req_id, entries, done, error } => {
                        fs_bridge.complete(req_id, FsReply::List { entries, done, error });
                    }
                    ControlMsg::FsReadReply { req_id, data, error } => {
                        fs_bridge.complete(req_id, FsReply::Read { data, error });
                    }
                    other => {
                        tracing::debug!(?other, "received control msg (ignored in MVP)");
                    }
                }
            }
            // ダウンロード送信タスクからのメッセージ (FileOffer/FileChunk/FileResult)
            Some(msg) = out_rx.recv() => {
                send_ctrl_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
            }
            // ProjFS コールバックからの FS RPC 要求 (FsList/FsRead) を TCP へ送る
            Some(msg) = fs_req_rx.recv() => {
                send_ctrl_msg(&mut write_half, &session, &msg, &mut tcp_tx_nonce).await?;
            }
            // (映像 UDP 送信は専用タスク (上の video_task) へ分離済み。ここでは扱わない。)
            // 音声フレーム受信 (broadcast ファンアウト) → UDP 送信 (STREAM_AUDIO)
            au = audio_brx.recv() => {
                let au = match au {
                    Ok(a) => a,
                    // 音声は欠落しても次フレームで自然回復する。取りこぼしは debug ログのみ。
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "audio broadcast lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                // 音声 nonce 空間の枯渇 (au.seq の u32 一周) を検知して以降の送出を止める。映像送信
                // タスクの checked_add ガードと対称の防御。broadcast は順序保証なので、前回送った
                // seq より小さい seq = wrap とみなせる (Lagged は前方スキップのみで減少しない)。
                if !audio_nonce_exhausted
                    && let Some(prev) = last_audio_seq
                    && au.seq < prev
                {
                    audio_nonce_exhausted = true;
                    tracing::error!(
                        "audio nonce space exhausted; stopping audio send (reconnect to rekey)"
                    );
                }
                // seq 進行はミュート/枯渇と独立に追跡する (ミュート中に wrap しても mute 明けで
                // 検知できるよう、送出可否に関わらず毎フレーム更新する)。
                last_audio_seq = Some(au.seq);
                // ミュート中はチャネルをドレインするだけで送らない (帯域節約 + 詰まり防止)。
                // seq は送出時のみ進めればよいが、AudioPacket は au.seq をそのまま使い nonce も
                // au.seq 由来なので、送らない=その seq の nonce を消費しないだけで一貫性は保たれる。
                if !audio_muted && !audio_nonce_exhausted {
                    let pkt = AudioPacket { seq: au.seq, payload: &au.data };
                    let mut framed = Vec::with_capacity(au.data.len() + 4);
                    pkt.encode(&mut framed).context("encode AudioPacket")?;
                    let nonce = NONCE_DOMAIN_AUDIO | au.seq as u64;
                    let dgram = encrypt_udp(&session, nonce, STREAM_AUDIO, &framed)?;
                    let addr = *peer_udp.lock().expect("peer_udp poisoned");
                    udp.send_to(&dgram, addr).await.context("UDP send audio")?;
                }
            }
            // client 側 UDP 自己修復の AURHELLO 再送 (TCP 生存中の UDP 途絶検知への対応)。
            // 同じ IP からの再送で、NAT の UDP マッピング変更等により対向ポートが変わっていたら
            // 更新する (変化が無ければ no-op)。
            Some(new_addr) = hello_rx.recv() => {
                let mut cur = peer_udp.lock().expect("peer_udp poisoned");
                if *cur != new_addr {
                    tracing::info!(old = %*cur, new = %new_addr, "UDP peer re-learned from AURHELLO resend");
                    *cur = new_addr;
                }
            }
        }
    }
    Ok(())
    }
    .await;

    // --- セッション後始末 (正常終了・エラーのどちらでも実行) ---
    // -0.5) この接続が SelectDisplay(仮想) で立てた takeover_pending=true を SetClientViewport
    // で戻せずに終了した場合 (異常切断・ping タイムアウト等) の保険。shared フラグなので、戻さずに
    // 抜けると他の全クライアントの映像送出が永久に止まる (このセッションだけの問題では済まない)。
    if takeover_pending_owned {
        tracing::warn!("connection ended while takeover was pending; clearing shared gate");
        takeover_pending.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    // 0) tray へ切断を通知する (切断バルーン + 接続数デクリメント)。確立時の on_connected と対。
    tray_bus.on_disconnected(peer_label);
    // 0.5) AURHELLO 登録簿から自分の枠を外す (自分の conn_id のときだけ除去)。
    unregister_hello(&hello_registry, peer.ip(), conn_id);
    // 1) stuck key 防止: 押下中のキー/マウスボタンへ解放イベントを注入する (クライアント毎)。
    let releases = held.release_events();
    if !releases.is_empty() {
        tracing::info!(
            count = releases.len(),
            "releasing held inputs on disconnect"
        );
        for ev in releases {
            let _ = input_tx.send(InputCmd::Event(ev)).await;
        }
    }
    // 2) ProjFS RPC を閉じる (所有者でなければ no-op): 待機中のコールバックを即時エラーで起こし、
    //    以後の要求も即時失敗にする。`_virt` の Drop (PrjStopVirtualizing = コールバック完了待ち)
    //    が RPC_TIMEOUT まで引きずられ、次セッションの受理と Explorer が固まるのを防ぐ。
    fs_bridge.close();
    fs_req_rx.close();
    // 3) 登録解除 + 接続数減算 + ProjFS 所有解放。最後の 1 本かどうかとテイクオーバー復元の要否を
    //    ロック下で原子的に判定する (複数同時切断でも復元は 1 回だけ走る)。
    // ロック順序は必ず display_op → shared。この後の end_takeover (数十〜数百ms の
    // spawn_blocking) を SelectDisplay/SetClientViewport の begin_takeover と同じ直列化ロックで
    // 排他する (このロックを取らずに end_takeover すると、切断処理中に別の新規接続が
    // begin_takeover を同時に走らせうる)。restore_takeover の判定自体もこのロックの中で行う
    // (判定後にロックを取ると、判定〜取得の間に新規接続の begin_takeover が割り込む余地が残る)。
    let _display_guard = display_op.lock().await;
    registry.lock().expect("registry poisoned").remove(&conn_id);
    let (is_last, restore_takeover, codec_recompute) = {
        let mut sc = shared.lock().await;
        sc.active_conns = sc.active_conns.saturating_sub(1);
        sc.client_stats.remove(&conn_id);
        sc.client_codecs.remove(&conn_id);
        // 切断で申告集合が変わり、共通集合が上のコーデックへ広がる (=昇格) ことがある。
        // 残クライアントが 0 なら次回接続時にゼロから再申告されるため再計算は不要。
        let codec_recompute = if sc.active_conns > 0 {
            let recompute = sc.recompute_codec();
            (recompute != sc.current_video_codec).then_some(recompute)
        } else {
            None
        };
        // カーソル合成要求を取り下げ、残りの要求有無を capture へ反映する (取りこぼし防止)。
        if sc.cursor_overlay.remove(&conn_id) {
            let _ = cursor_tx.send(!sc.cursor_overlay.is_empty());
        }
        if sc.projfs_owner == Some(conn_id) {
            sc.projfs_owner = None;
        }
        let is_last = sc.active_conns == 0;
        let restore = is_last && sc.taken_over;
        if restore {
            sc.taken_over = false;
        }
        (is_last, restore, codec_recompute)
    };
    if let Some(new_codec) = codec_recompute {
        let _ = enc_tx.send(EncoderCommand::SetCodec(new_codec)).await;
    }
    // 4) 最後の 1 本が切れたときだけグローバルな後始末を行う (他クライアントが視聴中なら維持)。
    //    テイクオーバー中なら全トポロジ (物理の再アタッチ含む) を復元する。物理解像度は変更しない
    //    方針なので、テイクオーバー以外の解像度復元は不要。
    if restore_takeover {
        let _ = tokio::task::spawn_blocking(crate::display::end_takeover).await;
        // テイクオーバー解除後は入力座標系と選択ディスプレイを物理 primary へ戻す。仮想 OFF で
        // 再接続したクライアントは既定の displays[0] (物理) を明示 SelectDisplay しない
        // (session.ts: target==displays[0] のとき送らない) ため、ここで戻さないと入力座標系が
        // 仮想解像度 (例 2560x1440) に取り残され、物理 (1920x1200) でカーソルがずれる (実機確認)。
        let geom = {
            let mut sc = shared.lock().await;
            sc.active_idx = 0;
            let d = &sc.displays[0];
            DisplayGeom {
                x: d.x,
                y: d.y,
                width: d.width,
                height: d.height,
            }
        };
        let _ = input_tx.send(InputCmd::SetDisplay(geom)).await;
    }
    // 5) VDD を再デタッチ: テイクオーバー復元後も VDD がデスクトップに残るため切り離す。
    //    未接続時にカーソルや Window が仮想ディスプレイへ迷い込む問題を防ぐ。最後の 1 本のみ。
    if is_last && let Some(vdd) = crate::display::detect() {
        let vn = vdd.gdi_name;
        let _ = tokio::task::spawn_blocking(move || {
            crate::display::detach_vdd_if_attached(&vn);
        })
        .await;
    }
    result
}

/// セッション中に押下されたまま解放されていない入力の追跡 (切断時の stuck key 防止)。
/// クライアントから届いた `InputEvent` を観察して押下状態を保持し、切断時に
/// [`HeldInputs::release_events`] で解放イベント列を合成する。OS 非依存のここで
/// 追跡することで、各 OS の injector に状態を持たせず host/client の対称性も保つ。
#[derive(Default)]
struct HeldInputs {
    keys: std::collections::HashSet<u32>,
    left: bool,
    right: bool,
    middle: bool,
}

impl HeldInputs {
    fn observe(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key { vk, down } => {
                if down {
                    self.keys.insert(vk);
                } else {
                    self.keys.remove(&vk);
                }
            }
            InputEvent::MouseButton { button, down } => match button {
                MouseButton::Left => self.left = down,
                MouseButton::Right => self.right = down,
                MouseButton::Middle => self.middle = down,
            },
            _ => {}
        }
    }

    /// 押下中の全ボタン/キーに対する解放イベント列を返す。ボタンを先に上げる
    /// (ドラッグ中の切断で「修飾キーだけ先に上がる」中途半端な状態を避ける)。
    fn release_events(&self) -> Vec<InputEvent> {
        let mut evs = Vec::new();
        for (down, button) in [
            (self.left, MouseButton::Left),
            (self.right, MouseButton::Right),
            (self.middle, MouseButton::Middle),
        ] {
            if down {
                evs.push(InputEvent::MouseButton {
                    button,
                    down: false,
                });
            }
        }
        evs.extend(
            self.keys
                .iter()
                .map(|&vk| InputEvent::Key { vk, down: false }),
        );
        evs
    }
}

/// drop でラップした tokio タスクを abort するガード。handle_client 終了時に
/// 読み取りタスクを確実に停止させ、セッション置換でのリークを防ぐ。
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
