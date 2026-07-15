# セキュリティ

Aurora は自宅 PC の画面・入力をネットワーク越しに開放するため、暗号化と露出範囲の
設計が中心です。本書は現状の保護・脅威モデル・既知の制約をまとめます。

> ⚠️ 個人プロジェクトであり、独立した第三者によるセキュリティ監査は受けていません。
> 自己責任で利用してください。脆弱性に気づいた場合は公開 issue ではなく非公開で連絡を。

## 前提となるネットワークモデル

- **Tailscale (Tailnet) 直結が前提**。NAT 越え・シグナリング・中継サーバは実装しません。
  通信は Tailnet 内 (`100.x.x.x`) に閉じ、外側からは Tailscale の認証・ACL で守られます。
- ホストは既定で **Tailscale インターフェースのみに bind** します (`--bind` 未指定なら
  `main.rs` の `resolve_bind_spec` が `Auto` を選び、実 IP は bind 時に `net.rs` の
  `resolve_bind_ip` が解決)。優先順位は `--bind` 明示 > Tailscale IP 自動検出 > `127.0.0.1`
  (loopback)。**Tailscale IP の解決は bind ループ内で行い、起動が `tailscaled` より先でも
  IP が出るまで待ってから bind します** (ホストは ONLOGON で自動起動するため起動レースが常態。
  一度きりの解決だと loopback へ落ちて固着するのを防ぐ)。一定時間 (既定 120 秒) 待っても取れない
  ときは **loopback に fail-closed** します (localhost での動作確認は可能、LAN/インターネットへは
  到達不能)。以前は `0.0.0.0` (全 NIC) へフォールバックしていましたが、これは Tailscale が無い
  最も無防備な環境で境界を全公開側へ倒す fail-open だったため改めました。LAN テストや意図的な
  公開は `--bind <ip>` (例: LAN IP、全 NIC なら `--bind 0.0.0.0`) で明示的にオプトインします。
  → 既定で LAN/インターネットへ露出せず、総当たりや接続枠 DoS の的を減らします。

## 暗号化と認証

| 項目 | 採用 |
|------|------|
| ハンドシェイク | Noise `XXpsk3` (`Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`) |
| 共通鍵暗号 | ChaCha20-Poly1305 (AEAD) |
| PSK 導出 | パスワード → Argon2id (m=64 MiB, t=3, p=1, 出力 32B) + 固定アプリソルト |

- 接続パスワードは host/client が**各自で独立に**同じ PSK を導出します。ソルトを通信で
  交換できないため固定ソルトですが、Argon2id のメモリ硬度により、弱いパスワードに対する
  オフライン総当たりコストを単一ハッシュより桁違いに高めています。
- パスワードは設定時に**最低 12 文字**を強制します (`MIN_PASSWORD_LEN`、CLI・トレイ両方)。
  単一要素認証 (パスワードのみ) を踏まえ、オフライン辞書攻撃への耐性を底上げするため 8 → 12 に
  引き上げています。
  加えて、よくある弱いパスワード・数字のみ・文字種が少なく短いものは設定時に**ソフト警告**を
  出し (`weak_password_reason`、強制はしない)、強いパスフレーズ候補を提示します
  (`suggest_passphrase`)。CLI `--set-password` とトレイのダイアログで同じ判定を使い、設定
  経路の防御を対称に保ちます。
- オンライン総当たりに対しては、ハンドシェイク失敗時にホストが**送信元 IP 単位の指数
  バックオフ** (1→2→4→…→最大 30 秒) を切断前に挟みます (`AuthThrottle`)。グローバルな
  ロックアウトにすると攻撃者が別 IP の正規ユーザーを締め出せる自爆 DoS になるため IP 単位とし、
  認証成功で即リセットします。累計失敗数はトレイメニューに表示し、総当たりに晒されていることに
  オーナーが気付けるようにしています。
- これらは主防御 (強いパスワード + Argon2id) を補助するレイヤです。XX-PSK は接続を試みた
  相手にハンドシェイク転写を渡すため、弱いパスワードはオフライン辞書攻撃で破られうる点に
  引き続き注意してください。

### デバイスピン留め (任意の 2 要素目)

パスワードは「知識要素」で、漏れると任意端末から接続できてしまいます。これを補う「所持要素」
として、**クライアントの永続静的公開鍵を許可リストで照合**する機能があります (既定 **無効**、
opt-in)。Noise XX は元々双方の静的鍵を交換するため**ワイヤ形式は不変**で、クライアントは
ID 鍵を生成・DPAPI 保存して毎回同じ鍵で接続し (`identity.bin`)、host は接続の `remote_static`
を許可リスト (`trusted_devices.txt`、公開鍵のみで非機密) と照合します (`DeviceGuard`)。

- 有効化・端末登録・削除はすべて**トレイメニュー**から行います (CLI 経路は持たないので、CLI
  バイパスの非対称が生じない)。
- 登録は**ペアリング窓 + オーナー承認**方式: トレイで「次の接続を登録」を押して窓を開くと、
  接続してきた未知端末は**自動登録されず「承認待ち」に記録**されます (その回の接続は拒否)。
  オーナーはトレイに出るその端末の**公開鍵フィンガープリント** (先頭 8B を `xxxx-xxxx-xxxx-xxxx`
  表記) を、**接続元端末に表示された同じ値** (client 設定「その他」→「この端末の識別子」) と
  照合して承認します。承認された鍵だけが許可リストへ入り、窓は閉じます。
  → これにより、PSK を持つ攻撃者が窓の開いた瞬間にレースで割り込んでも、**フィンガープリント
  不一致でオーナーが弾ける** (旧来の「窓中に最初に来た端末を無条件登録」ではレースで攻撃者が
  恒久登録され得た)。承認はトレイのモーダル表示中に別接続が承認待ちリストを変えても、クリック時
  に控えた**公開鍵で対象を確定**するため、見た端末とは別物を登録しません (TOCTOU 対策)。
- ピン留め有効中に未登録端末が PSK を正しく出しても**拒否**し (IP バックオフを伸ばす)、
  登録端末のみ接続できます。接続中の操作者が入力注入でトレイを操作してピン留めを**無効化/
  端末削除/新規登録**するのを防ぐため、これらは接続中は拒否します (パスワード/ポート変更と
  同じ対称ガード)。
- Tailscale (網側の到達制御) + 強パスワードで足りる多くの個人用途では不要ですが、Tailnet を
  他者と共有する等で網の信頼境界が緩い場合に「登録端末以外は一切接続させない」を強制できます。
- Noise XX は相互に静的鍵を交換するため、PSK 検証と合わせてセッションごとに前方秘匿性の
  ある鍵を確立します。パラメータ (`NOISE_PARAMS` / Argon2 設定 / `PSK_SALT`) を変えると
  PSK が変わり旧クライアントと非互換になります。

### nonce ドメイン分離 (クロスチャネル注入の多重防御)

TCP 制御・UDP 映像・UDP 音声は**同一 Noise 方向 (= 同一鍵)** を共有します。ChaCha20-Poly1305 は
nonce 再利用が致命的なため、nonce 空間を上位 2 ビットでドメイン分割します。

```
nonce (u64) = [ 2bit ドメイン ][ 62bit ストリーム内カウンタ/seq ]
  制御 : NONCE_DOMAIN_CTRL  (1<<63)  … 連番カウンタ
  映像 : NONCE_DOMAIN_VIDEO (0)      … パケット seq
  音声 : NONCE_DOMAIN_AUDIO (1<<62)  … パケット seq
```

受信側は「想定ストリームのドメインか」を検証し、別ドメインの暗号文の注入
(例: TCP 制御暗号文を UDP 映像へ流し込む) を弾きます。これによりチャネルを跨いだ
nonce 衝突とクロスチャネル注入を防ぎます。

> 注意: 映像/音声は seq (u32) を nonce 下位に使うため、seq 一周 (約 42 億パケット) で
> nonce が一巡します。1 セッション長では到達しない前提です。

## パスワードの保存 (Windows)

- `aurora-host --set-password` で入力したパスワードは **DPAPI** (Windows データ保護 API) で
  暗号化し `%LOCALAPPDATA%\Aurora\password.bin` に保存します。平文の環境変数・レジストリには
  残しません。
- 復号できるのは**保存したのと同じ Windows ユーザーアカウントだけ**です。よって
  `--set-password` は、ログオン時タスクを走らせるのと**同じユーザー**で (昇格せず) 実行します。
- パスワード未設定で起動すると、起動ごとにランダムなパスワードを生成します (手動接続・
  動作確認用)。無人起動には固定保存が必要です ([remote-access-setup.md](remote-access-setup.md))。

### リモートからの締め出し/乗っ取り防止

リモートクライアントは入力注入でホストの端末を開き `aurora-host --set-password` /
`--clear-password` を実行できてしまいます。これを塞ぐため、パスワードの変更/削除は
トレイ UI・CLI の**両方**に対称のガードを入れています:

1. クライアント接続中は変更を**拒否**する (接続中だけ生存する名前付きイベント
   `Local\AuroraSessionActive` を各接続の `ActiveGuard` が保持し、トレイと CLI が
   `net::is_active()` で別プロセスからも確認する。`crates/host/src/net/session.rs`)。
2. 既存パスワードがある場合は、**現在のパスワード**を要求して照合してから変更する。

## WoL 中継 (aurora-wol) の露出面

Wake-on-LAN で自宅 PC を起こす機能は、自宅 LAN に常駐する別プロセス
**`aurora-wol`** (`crates/wol`、raspi 等での運用を想定) が担います。host 本体とは
別バイナリ・別ポート (既定 7100) ですが、Tailnet に露出するため脅威モデルに含めます。

- **認証**: host への接続と同じ **Noise `XXpsk3`** ハンドシェイクを使い、PSK も同じ
  `psk_from_password` (Argon2id) で導出します。中継用パスワードは host の接続パスワードとは
  **別の値**として運用でき、host/client 間の鍵と混用しません (`crates/wol/src/main.rs`,
  `crates/client/src-tauri/src/net/wol.rs`)。
- **パスワードの保存場所が host と非対称**: client 側 (中継先パスワードのキャッシュ) は
  host 接続用パスワードと同じく DPAPI で暗号化して保存します。一方 **raspi 側は非 Windows
  で DPAPI が使えないため平文ファイル (`password.txt` 等) で保管**します。ファイル権限を
  `chmod 600` にする運用が前提で、これが破られる (raspi への侵入等) と中継パスワードが漏れます。
- **影響範囲は起動のみ**: wake 要求 (`WolMsg::Wake { mac }`) が認証を通ると、
  `aurora-wol` は指定 MAC 宛にマジックパケットを LAN ブロードキャストして対象 PC を
  起動させます。それ以上の操作 (画面・入力・ファイル等) はできません。host への実際の
  リモート操作には、別途 host 自身のパスワード/Noise ハンドシェイクを通す必要があります。
- **MAC アドレスの開示範囲**: host は `HostInfo` メッセージでプライマリ NIC の MAC を
  client へ渡します (`crates/common/src/proto.rs`)。これは**映像セッションが確立した後**
  (=host のパスワード認証を通過した接続) にのみ流れるため、未認証の相手には開示されません。
  client はこれを host エントリに保存し、WoL タブの wake 要求に使います。
- **総当たり・DoS 対策 (host と対称化)**: 接続ごとに独立タスクで処理し (accept ループは
  塞がない)、ハンドシェイク I/O は 10 秒、ハンドシェイク〜Wake〜Ack の 1 接続全体は 15 秒で
  タイムアウトさせて切断します。加えて host と同じ**送信元 IP 単位の指数バックオフ**
  (1→最大 30 秒、`WolThrottle`) をハンドシェイク失敗時に挟み、**同時接続数を上限
  (`MAX_WOL_CLIENTS`=8) のセマフォで制限**してコネクションフラッドを抑えます。1 接続 =
  1 Wake 要求 = 即切断の軽量ラウンドトリップです。
- **bind も host と対称に Tailscale-only 既定**: 以前は `0.0.0.0` (全 NIC) 固定で自宅 LAN
  全体へ露出していましたが、既定を Tailscale IPv4 の自動解決 (`--bind` で上書き可) に改めました。
  Tailscale が取れなければ loopback に fail-closed し、明示公開は `--bind <ip>` (全 NIC なら
  `--bind 0.0.0.0`) でオプトインします。raspi が tailscaled より先に起動しても IP が出るまで
  待ってから bind します。

## ログ

暗号化キー・パスワードはログに出しません。保存済みパスワードを使う場合も値は表示せず、
"(from secure store)" とだけ記録します。ファイルログは host/client/aurora-wol とも
`crates/common/src/logging.rs` に集約した共通初期化で、`%LOCALAPPDATA%\Aurora\logs\`
(非 Windows は `~/.local/share/Aurora/logs`) に `host.<日付>.log` / `client.<日付>.log` /
`wol.<日付>.log` を日次ローテーション + 7 世代保持で残します。

## 脅威モデル (何を守り、何を守らないか)

**守るもの**
- Tailnet 経路上の盗聴・改竄 (Noise + AEAD で機密性・完全性)。
- 弱いパスワードのオフライン総当たり (Argon2id メモリ硬化で緩和)。
- チャネルを跨いだパケット注入・リプレイ的 nonce 衝突 (ドメイン分離 + AEAD)。
- 接続中の操作者によるパスワード変更での締め出し/乗っ取り (上記ガード)。
- 受信ファイル名による保存先ディレクトリ外への書き出し・NTFS 代替データストリーム (ADS)
  生成 (host/client 対称のファイル名サニタイズ。`crates/common/src/fsname.rs`)。
- LAN/インターネットへの不用意な露出 (既定 Tailscale-only bind)。
- パスワード漏洩時の任意端末からの接続 (任意の 2 要素目=デバイスピン留めを有効化した場合のみ。
  既定無効)。
- WoL 中継 (`aurora-wol`) への到達も Noise `XXpsk3` + Argon2id 導出 PSK で認証 (host と同じ
  仕組み)。ただし中継パスワードの保存先は運用機器 (raspi 等) 側で DPAPI 相当の保護がない
  (下記「WoL 中継 (aurora-wol) の露出面」参照)。

**守らない/前提に委ねるもの**
- **ネットワーク到達制御は Tailscale に委譲**。Tailnet への参加可否・ACL は Tailscale 側で
  設定します。Aurora 自体は IP allowlist を持ちません。
- **ホスト OS ユーザーの信頼**。DPAPI 鍵を持つユーザーや管理者は保存パスワードを扱えます。
- **物理アクセス・マルウェア感染済みホスト**は対象外。
- **DoS 全般**。Tailnet 内からの妨害は想定の主眼ではありません (露出を絞ることで緩和)。

## 既知の制約

- **認証前 DoS (接続枠の占有)**: 接続は受理ごとに独立タスクで処理し、同時接続を **2 段の
  セマフォ**で制限します。認証前のハンドシェイク枠 (`PREAUTH_SLOTS`=32) と、認証を通した
  確立済みセッション枠 (`MAX_CLIENTS`=4) を分離し、**セッション枠は Noise + デバイス認証を
  通過してから確保**します。これにより未認証の stall 接続や総当たり失敗のバックオフ待ちが、
  正規セッション用の scarce な 4 枠を食い潰せません (以前は単一セマフォで、失敗時のバックオフ
  sleep 中も枠を握り続けたため、認証前接続だけで正規ユーザーを締め出せました)。ハンドシェイク
  枠 (32) の飽和は依然あり得ますが、より大きく取っており、Tailnet 内の信頼を前提に許容します
  (到達制御は Tailscale 側 ACL に委譲)。
- **UDP 音声のリプレイ**: 音声パケットの `seq` は欠落検出のみで、厳密な anti-replay は
  していません。経路上の攻撃者は音声データグラムを再送できます (映像はフレーム再構成と seq
  連続性で実質無害化)。Tailscale 下層 (WireGuard) が経路上の攻撃者をほぼ排除するため実害は
  低い前提です。
- **HDR ディスプレイのキャプチャ**: 現状 SDR 前提のトーンマップで取り込むため、HDR 有効な
  ディスプレイでは映像が眩しく (白飛び気味に) なります。
- **クライアント側ファイルのダウンロード範囲**: ドライブ投影でホストへ見せるルートの扱いは
  設計上の選択があります。共有ルートは既定でホーム全体ではなく明示指定 (opt-in) です。
- 配布インストーラは未署名です。SmartScreen / Defender に警告される場合があります
  (リモート操作 + 入力注入 + 画面キャプチャという性質上、誤検知もあり得る)。

## 関連コード

| 関心事 | ファイル |
|------|------|
| Noise ハンドシェイク・transport・nonce ドメイン | `crates/common/src/noise.rs` |
| PSK 導出 (Argon2id) | `crates/common/src/noise.rs` (`psk_from_password`) |
| パスワードの DPAPI 保存 | `crates/host/src/security/secret.rs` |
| 同時接続数の上限 (2 段セマフォ: ハンドシェイク枠 `PREAUTH_SLOTS`=32 + 確立セッション枠 `MAX_CLIENTS`=4) | `crates/host/src/net/mod.rs` |
| 接続中ガード (名前付きイベント `Local\AuroraSessionActive`) | `crates/host/src/net/session.rs` |
| bind アドレス解決 (Tailscale-only 既定) | `crates/host/src/main.rs` (`resolve_bind_spec`) → `crates/host/src/net/mod.rs` (`resolve_bind_ip`, bind 時に Tailscale IP を待つ) |
| パスワード変更ガード | `crates/host/src/main.rs` / `crates/host/src/tray.rs` |
| パスワード最低長・認証失敗遅延 | `crates/common/src/noise.rs` (`MIN_PASSWORD_LEN`) / `crates/host/src/net/auth.rs` (`AuthThrottle`) / `crates/wol/src/main.rs` (`WolThrottle`) |
| デバイスピン留め (静的鍵許可リスト) | `crates/host/src/security/devices.rs` / `crates/host/src/net/client.rs` (`remote_static` 照合) / `crates/host/src/tray.rs` |
| クライアント永続 ID 鍵 (静的鍵) | `crates/common/src/noise.rs` (`generate_static_keypair`/`remote_static`) / `crates/client/src-tauri/src/secret.rs` |
| 受信ファイル名サニタイズ | `crates/common/src/fsname.rs` |
| WoL 中継デーモン (Noise ハンドシェイク・マジックパケット送出) | `crates/wol/src/main.rs` |
| WoL 中継への wake 要求 (client 側) | `crates/client/src-tauri/src/net/wol.rs` |
| `HostInfo` の MAC 開示 | `crates/common/src/proto.rs` (`ControlMsg::HostInfo`) |
