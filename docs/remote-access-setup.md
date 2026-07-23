# 出先から自宅 PC へ接続する（無人起動セットアップ）

出先の PC から自宅 PC（host）へ接続するには、自宅 PC 側で **誰も操作しなくても
host が起動していて、パスワードが事前に分かっている** 必要がある。本書はその
ための一度きりのセットアップ手順をまとめる。

前提: ネットワークは **Tailscale (Tailnet)**。NAT 越え・中継は使わない。

> 配布インストーラ (`aurora-host-setup-<ver>.exe`) で導入した場合、自動起動タスクの登録は
> インストーラの Autostart セクションが行うため、以降の `--install` 手順は不要。以下の
> `cargo build` / 生成された exe を直接叩く手順は、ソースからビルドする開発者向け。

---

## 仕組み（なぜこの構成なのか）

接続が成立するには次の 3 つが同時に満たされている必要がある。

1. **パスワードを事前共有しておく**
   host は既定では起動ごとにランダムなパスワードを生成してコンソールへ表示する。
   無人起動ではこれを読めないので、`aurora-host --set-password` で**固定パスワード**を
   保存しておき、両端であらかじめ共有する（接続時に「取得」するのではなく、自分で
   決めた値を両方が知っている状態にする）。

   > **保存方法**: 入力したパスワードは **DPAPI**（Windows のデータ保護 API）で
   > 暗号化して `%LOCALAPPDATA%\Aurora\password.bin` に保存される。平文の環境変数や
   > レジストリには残らない。復号できるのは**保存したのと同じ Windows ユーザー
   > アカウントだけ**で、他ユーザーや他 PC では復号できない。したがって
   > `--set-password` は**ログオン時タスクを走らせるのと同じユーザー**で実行すること
   > （昇格は不要）。

2. **host をログオン時に自動起動する**
   誰も操作しなくても host プロセスが立ち上がっている必要がある。
   `aurora-host --install` がタスクスケジューラに「ログオン時」タスクを登録する。

   > **なぜ Windows サービスではないのか**: 画面キャプチャに使う DXGI Desktop
   > Duplication は対話ユーザセッションでしか動作しない。Windows サービスは
   > Session 0 で隔離実行されるためデスクトップをキャプチャできない。よって
   > 「ログオン時タスク（= ユーザセッションで実行）」を使う。

3. **自宅 PC を起こし、Tailscale が起動している**
   - 電源: Wake-on-LAN で起こすか、スリープ運用 + WoL で復帰させる。
   - Tailscale: 起動時に自動接続する設定にしておけば、出先の client から
     `100.x.x.x:<port>` で届く。

```
[事前準備・自宅で一度]
  aurora-host --set-password でパスワードを DPAPI 暗号保存
        +
  aurora-host --install で自動起動を登録
        +
  Tailscale を自動接続に、PC を WoL 対応に
        ↓
[出先]
  client の WoL タブで自宅 PC を起こす（aurora-wol 中継、任意）
        → Tailscale 接続 → ログオン時タスクで host 起動
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

ログオン時タスクを走らせるのと**同じ Windows ユーザー**で、**昇格しない**通常の
PowerShell で実行する（昇格すると別アカウント＝管理者の鍵で暗号化され、ユーザー
セッションのタスクから復号できなくなる）:

```powershell
.\target\release\aurora-host.exe --set-password
```

- パスワードを 2 回（確認入力）求められる。入力はエコーされない。
- 入力値は DPAPI で暗号化され `%LOCALAPPDATA%\Aurora\password.bin` に保存される
  （平文の環境変数・レジストリには残らない）。
- このパスワードは出先の client で入力する値そのもの。秘密として扱い、リポジトリや
  ログに残さないこと。
- 変更は再実行で上書き、削除は `--clear-password`。
- 未設定のまま host を起動すると、従来どおり起動ごとにランダムなパスワードを生成して
  表示する（手動接続・動作確認用）。無人起動にはこの手順での固定保存が必要。

### 3. 自動起動を登録する

実行ファイルのある場所から、登録したい起動オプションを付けて実行する。
`--install` 以外の引数（`--port` など）はそのままタスクのコマンド行へ転送される。
`/RL HIGHEST`（昇格実行）で登録されるため、**管理者の PowerShell** で実行する:

```powershell
# 例: ポート 8000 / ビットレート上限 10Mbps で自動起動させる
.\target\release\aurora-host.exe --install --port 8000 --max-bitrate 10
```

登録内容を確認・解除するには:

```powershell
schtasks /Query /TN "Aurora Host"          # 確認
.\target\release\aurora-host.exe --uninstall   # 解除
```

> パスワードはタスク定義へ埋め込まれない（手順 2 で保存した DPAPI ファイルを、同じ
> ユーザーで走るタスクが復号して読む）。

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
2. 自宅 PC がログオンすると host が自動起動し、保存済みパスワードで待ち受ける
3. 出先の PC で client を起動（「起動して接続」を使った場合はこの後の操作は不要）
4. 接続先に `100.x.x.x:<port>`、パスワードに手順 2 で設定した値を入力して接続

---

## トラブルシュート

| 症状 | 確認点 |
|------|--------|
| 接続できない | `tailscale status` で双方がオンラインか。`<port>` が host の `--port` と一致しているか |
| パスワードが通らない | `--set-password` をタスクと同じユーザーで（昇格せず）実行したか。`%LOCALAPPDATA%\Aurora\password.bin` があるか。client 側の入力値と一致しているか |
| タスクは動くが映像が来ない | ログオンセッションがアクティブか（ロック画面のみでは DXGI が止まる構成あり）。GPU ドライバ |
| `--install` が失敗する | 管理者権限で実行しているか（`/RL HIGHEST` 登録のため） |
| WoL で起きない | Pi（aurora-wol）と自宅 PC が同一サブネットか。MAC が正しいか。高速スタートアップ無効化したか。S5 不可なら S3/S4（スリープ/休止）で運用 |
| WoL タブで「保存済みパスワードがありません」と出る | 中継先（raspi）のパスワード未保存。表示されるダイアログでパスワードを入力（`deploy-aurora-wol.ps1` 実行時に控えた値）|
| 自動ログオンしたい | 無人運用で「ログオン時」を満たすには自動ログオン設定が要る（セキュリティと要相談） |
