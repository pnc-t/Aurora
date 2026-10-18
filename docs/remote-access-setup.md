# 出先から自宅 PC へ接続する（無人起動セットアップ）

出先の PC から自宅 PC（host）へ接続するには、自宅 PC 側で **誰も操作しなくても
host が起動していて、パスワードが事前に分かっている** 必要がある。本書はその
ための一度きりのセットアップ手順をまとめる。

前提: ネットワークは **Tailscale (Tailnet)**。NAT 越え・中継は使わない。

> 配布インストーラ (`aurora-host-setup-<ver>.exe`) で導入した場合、サービス登録は
> インストーラの「サービスとして常駐」セクションが行うため、以降の `--install-service` 手順は
> 不要。以下の `cargo build` / 生成された exe を直接叩く手順は、ソースからビルドする開発者向け。

---

## 仕組み（なぜこの構成なのか）

接続が成立するには次の 3 つが同時に満たされている必要がある。

1. **パスワードを事前共有しておく**
   host は既定では起動ごとにランダムなパスワードを生成してコンソールへ表示する。
   無人起動ではこれを読めないので、`aurora-host --set-password` で**固定パスワード**を
   保存しておき、両端であらかじめ共有する（接続時に「取得」するのではなく、自分で
   決めた値を両方が知っている状態にする）。

   > **保存方法**: 入力したパスワードは **DPAPI**（Windows のデータ保護 API、マシン
   > スコープ）で暗号化して `%ProgramData%\Aurora\host\password.bin` に保存される。平文の
   > 環境変数やレジストリには残らない。サインイン前の SYSTEM サービスからも読めるよう
   > PC 単位で保存し、読み書きはフォルダの ACL で **SYSTEM と Administrators だけ**に
   > 限っている。したがって `--set-password` は**管理者の PowerShell** で実行する
   > （トレイの「パスワードを変更」なら昇格不要）。

2. **host を SYSTEM サービスとして常駐させる**
   誰も操作しなくても host プロセスが立ち上がっている必要がある。
   `aurora-host --install-service` が Windows サービス "AuroraHost" を登録する。

   > **仕組み**: 画面キャプチャに使う DXGI Desktop Duplication は対話ユーザセッション
   > でしか動作せず、Windows サービスが動く Session 0 からはキャプチャできない。そこで
   > サービスは本体を**対話セッションへ SYSTEM 権限で**起動し直す (RustDesk / Chrome
   > Remote Desktop と同じ構成)。SYSTEM で動くので、UAC の確認画面・ロック画面・
   > サインイン画面 (再起動直後を含む) も映して操作できる。トレイはログオンユーザーの
   > 昇格 (管理者) 権限で別に起動される (管理者でないユーザーのセッションでは出ない)。

3. **自宅 PC を起こし、Tailscale が起動している**
   - 電源: Wake-on-LAN で起こすか、スリープ運用 + WoL で復帰させる。
   - Tailscale: 起動時に自動接続する設定にしておけば、出先の client から
     `100.x.x.x:<port>` で届く。

```
[事前準備・自宅で一度]
  aurora-host --set-password でパスワードを DPAPI 暗号保存
        +
  aurora-host --install-service でサービスを登録
        +
  Tailscale を自動接続に、PC を WoL 対応に
        ↓
[出先]
  client の WoL タブで自宅 PC を起こす（aurora-wol 中継、任意）
        → Tailscale 接続 → サービスが host を起動 (サインイン画面のままでも接続可)
        ↓
  client 起動 → 100.x.x.x:<port> + 既知のパスワードを入力 → 接続
```

---

## セットアップ手順（自宅 PC で一度だけ）

### 1. host をリリースビルドする

```powershell
cargo build -p aurora-host --release
```

成果物は `target\release\aurora-host.exe`。

### 2. パスワードを設定する（DPAPI 暗号保存）

**管理者の PowerShell** で実行する（保存先 `%ProgramData%\Aurora\host` は SYSTEM と
Administrators しか書けない）:

```powershell
.\target\release\aurora-host.exe --set-password
```

- パスワードを 2 回（確認入力）求められる。入力はエコーされない。
- 入力値は DPAPI で暗号化され `%ProgramData%\Aurora\host\password.bin` に保存される
  （平文の環境変数・レジストリには残らない）。稼働中の host には届かないので、
  サービス運用中に CLI で変えたら `Restart-Service AuroraHost` で反映する
  （トレイからの変更は即反映）。
- このパスワードは出先の client で入力する値そのもの。秘密として扱い、リポジトリや
  ログに残さないこと。
- 変更は再実行で上書き、削除は `--clear-password`。
- 未設定のまま host を起動すると、従来どおり起動ごとにランダムなパスワードを生成して
  表示する（手動接続・動作確認用）。無人起動にはこの手順での固定保存が必要。

### 3. サービスとして登録する

実行ファイルのある場所から、登録したい起動オプションを付けて**管理者の PowerShell** で
実行する。`--install-service` 以外の引数（`--port` など）はそのままサービスの起動引数へ
転送される。旧方式の「ログオン時」タスクが登録済みなら削除される。

```powershell
# 例: ポート 8000 / ビットレート上限 10Mbps で常駐させる
.\target\release\aurora-host.exe --install-service --port 8000 --max-bitrate 10
```

> サービスは登録した exe のパスを実行し続ける。`target\release` の exe を登録したまま
> 再ビルドすると exe がロックされて上書きできないので、先に `--uninstall-service` する。

登録内容を確認・解除するには:

```powershell
Get-Service AuroraHost                                  # 確認
.\target\release\aurora-host.exe --uninstall-service    # 解除
```

> パスワードはサービス定義へ埋め込まれない（手順 2 で保存した DPAPI ファイルを
> サービス配下の host が復号して読む）。旧版で `%LOCALAPPDATA%\Aurora` に保存した
> 設定/パスワードは、保存したユーザーの管理者 PowerShell で
> `aurora-host --migrate-user-state` を一度実行すると移行できる（インストーラは自動で行う）。

### 4. Tailscale を自動接続にする

```powershell
tailscale up          # 一度ログインしておく
tailscale ip -4       # 自宅 PC の 100.x.x.x を控える（client で使う）
```

Tailscale はサービスとして常駐し、起動時に自動で tailnet へ参加する。

### 5. （任意）Wake-on-LAN を aurora-wol 中継で構成する

外出先から自宅 PC の電源を入れたい場合のみ（常時オン運用なら不要）。

> **なぜ中継役が要るのか**: WoL の「マジックパケット」は LAN 内ブロードキャスト
> (L2) で飛ぶが、Tailscale は L3 の VPN なのでブロードキャストを中継しない。
> 加えて寝ている自宅 PC は Tailscale に乗っていない。よって自宅 LAN 上に
> **常時起動 + Tailscale 常駐の中継役**を 1 台置き、出先からそこへ Tailscale で
> 入ってマジックパケットを LAN へ送る。Raspberry Pi は低消費電力で最適。
> 中継デーモン本体はリポジトリ同梱の **`aurora-wol`**（`crates/wol`）で、client の
> WoL タブから Noise XX-PSK で認証して wake 要求を送る。
>
> ```
> [出先PC(client)] --Tailscale(L3)--> [aurora-wol on Raspberry Pi] --LANブロードキャスト--> [寝てる自宅PC]
> ```

詳細な前提・実行方法・systemd 化は [`crates/wol/README.md`](../crates/wol/README.md) が
一次情報。ここでは全体フローのみ示す。

#### 5-1. 自宅 PC 側（起こされる側）の設定

- **BIOS/UEFI** で Wake-on-LAN（"Power On by PCI-E" 等）を有効化。
- **NIC ドライバ**: デバイスマネージャ → ネットワークアダプタ → プロパティ
  - 「電源の管理」→「このデバイスで…スタンバイ状態を解除」「Magic Packet でのみ…」を有効
  - 「詳細設定」→「Wake on Magic Packet」を Enabled
- **高速スタートアップを無効化**（完全シャットダウン S5 からの WoL が効かない主因）:
  ```powershell
  powercfg /h off
  ```
- MAC アドレスは手動で控える必要はない。host が `HostInfo` でプライマリ NIC の MAC を
  client へ渡すため、client 側で一度接続していれば WoL タブの追加ダイアログが自動入力する
  （手動入力する場合は `getmac /v` で確認）。

> S3（スリープ）/S4（休止）からの復帰が最も安定。運用は **「スリープ + WoL」** 推奨。

#### 5-2. Raspberry Pi 側（中継役）に aurora-wol をデプロイする

Pi は自宅 PC と**同じ LAN（同一サブネット）**に接続し、Tailscale 導入・SSH 接続可能な
状態にしておく（有線が確実）。Windows 開発機からワンコマンドでビルド・配備・systemd 化
できる:

```powershell
cd crates/wol
.\deploy-aurora-wol.ps1 -Target pi@100.x.x.x
# ポートを変える場合
.\deploy-aurora-wol.ps1 -Target pi@100.x.x.x -Port 7100
```

初回はパスワード入力を求められる（空 Enter で自動生成）。完了後に表示される
`ホスト:ポート`（Pi の Tailscale IP + 既定 7100）と**パスワード**を控える
（host の接続パスワードとは別物）。2 回目以降の実行はバイナリ差し替え + サービス
再起動のみで、既存パスワードは保持される。スクリプトが使えない環境向けの手動手順
（直接ビルド・手動 systemd ユニット作成）は README を参照。

#### 5-3. client の WoL タブに登録する

1. client を起動し、サイドバーの **Wake on LAN** タブを開く
2. 右上の ＋ で WoL 先を追加
   - 「登録済みホストから選ぶ」で自宅 PC の host エントリを選ぶと、MAC が自動入力される
     （host から取得済みの MAC。未接続なら手動で `AA:BB:CC:DD:EE:FF` 形式で入力）
   - 中継先（raspi）host/port と、5-2 で控えたパスワードを入力して保存
     （パスワードは以後 DPAPI 暗号保存され、次回以降は空欄で使い回せる）

#### 5-4. 出先から起こす

WoL タブのカードにある操作:
- **起動**（雷アイコン）: マジックパケットを送るだけ
- **起動して接続**（雷 + chevron、host エントリと紐付けている場合のみ表示）: マジック
  パケット送出後、host のポートが開くまで到達性を自動でポーリングし、開いたらそのまま接続する
  （事前に一度そのホストへ手動接続してパスワードを保存しておく必要がある）

---

## 出先からの接続

1. （PC が落ちているなら）client の WoL タブから「起動」または「起動して接続」を実行し、
   自宅 PC を起こす（手順 5 参照。常時オン運用なら不要）
2. 自宅 PC が起動すると、サインイン前でもサービスが host を起動し、保存済みパスワードで
   待ち受ける (サインイン画面のまま接続し、遠隔でサインインできる)
3. 出先の PC で client を起動（「起動して接続」を使った場合はこの後の操作は不要）
4. 接続先に `100.x.x.x:<port>`、パスワードに手順 2 で設定した値を入力して接続

---

## トラブルシュート

| 症状 | 確認点 |
|------|--------|
| 接続できない | `tailscale status` で双方がオンラインか。`<port>` が host の `--port` と一致しているか。ファイアウォール規則 "Aurora Host" があるか (`--install-service` が作る。[troubleshooting.md](troubleshooting.md) 参照) |
| パスワードが通らない | `%ProgramData%\Aurora\host\password.bin` があるか。CLI で変えたならサービスを再起動したか。client 側の入力値と一致しているか |
| サービスは動くが映像が来ない | `Get-Service AuroraHost` が Running か。host ログ (`%ProgramData%\Aurora\host\logs\`) の `service.<日付>.log` / `host.<日付>.log`。GPU ドライバ |
| `--install-service` が失敗する | 管理者権限で実行しているか |
| WoL で起きない | Pi（aurora-wol）と自宅 PC が同一サブネットか。MAC が正しいか。高速スタートアップ無効化したか。S5 不可なら S3/S4（スリープ/休止）で運用 |
| WoL タブで「保存済みパスワードがありません」と出る | 中継先（raspi）のパスワード未保存。表示されるダイアログでパスワードを入力（`deploy-aurora-wol.ps1` 実行時に控えた値）|
| 自動ログオンしたい | サービス運用では不要（サインイン画面のまま接続し、遠隔でサインインできる） |
