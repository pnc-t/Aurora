# 出先から自宅 PC へ接続する（無人起動セットアップ）

出先の PC から自宅 PC（host）へ接続するには、自宅 PC 側で **誰も操作しなくても
host が起動していて、パスワードが事前に分かっている** 必要がある。本書はその
ための一度きりのセットアップ手順をまとめる。

前提: ネットワークは **Tailscale (Tailnet)**。NAT 越え・中継は使わない。

---

## 仕組み（なぜこの構成なのか）

接続が成立するには次の 3 つが同時に満たされている必要がある。

1. **パスワードを事前共有しておく**
   host は既定では起動ごとにランダムなパスワードを生成してコンソールへ表示する。
   無人起動ではこれを読めないので、環境変数 `DDD_PASSWORD` に**固定パスワード**を
   設定しておき、両端であらかじめ共有する（接続時に「取得」するのではなく、自分で
   決めた値を両方が知っている状態にする）。

2. **host をログオン時に自動起動する**
   誰も操作しなくても host プロセスが立ち上がっている必要がある。
   `ddd-host --install` がタスクスケジューラに「ログオン時」タスクを登録する。

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
  DDD_PASSWORD をシステム環境変数に設定
        +
  ddd-host --install で自動起動を登録
        +
  Tailscale を自動接続に、PC を WoL 対応に
        ↓
[出先]
  WoL で自宅 PC を起こす → Tailscale 接続 → ログオン時タスクで host 起動
        ↓
  client 起動 → 100.x.x.x:<port> + 既知のパスワードを入力 → 接続
```

---

## セットアップ手順（自宅 PC で一度だけ）

### 1. パスワードを固定する

システム環境変数として設定する（再起動後も保持され、ログオン時タスクからも読める）。
**管理者の PowerShell** で:

```powershell
setx DDD_PASSWORD "自分で決めた強いパスワード" /M
```

- `/M` はシステム全体（HKLM）に設定する指定。省略するとユーザ環境変数になる。
- 設定後、反映には再ログオン（またはタスクの再起動）が必要。
- このパスワードは出先の client で入力する値そのもの。秘密として扱い、リポジトリや
  ログに残さないこと。

### 2. host をリリースビルドする

```powershell
cargo build -p ddd-host --release
```

成果物は `target\release\ddd-host.exe`。

### 3. 自動起動を登録する

実行ファイルのある場所から、登録したい起動オプションを付けて実行する。
`--install` 以外の引数（`--port` など）はそのままタスクのコマンド行へ転送される。
`/RL HIGHEST`（昇格実行）で登録されるため、**管理者の PowerShell** で実行する:

```powershell
# 例: ポート 8000 / ビットレート上限 10Mbps で自動起動させる
.\target\release\ddd-host.exe --install --port 8000 --max-bitrate 10
```

登録内容を確認・解除するには:

```powershell
schtasks /Query /TN "DDD Host"          # 確認
.\target\release\ddd-host.exe --uninstall   # 解除
```

> パスワードはタスク定義へ埋め込まれない（手順 1 の環境変数から読む）。

### 4. Tailscale を自動接続にする

```powershell
tailscale up          # 一度ログインしておく
tailscale ip -4       # 自宅 PC の 100.x.x.x を控える（client で使う）
```

Tailscale はサービスとして常駐し、起動時に自動で tailnet へ参加する。

### 5. （任意）Wake-on-LAN を Raspberry Pi 中継で構成する

外出先から自宅 PC の電源を入れたい場合のみ（常時オン運用なら不要）。

> **なぜ中継役が要るのか**: WoL の「マジックパケット」は LAN 内ブロードキャスト
> (L2) で飛ぶが、Tailscale は L3 の VPN なのでブロードキャストを中継しない。
> 加えて寝ている自宅 PC は Tailscale に乗っていない。よって自宅 LAN 上に
> **常時起動 + Tailscale 常駐の中継役**を 1 台置き、出先からそこへ Tailscale で
> 入ってマジックパケットを LAN へ送る。Raspberry Pi は低消費電力で最適。
>
> ```
> [出先PC] --Tailscale(L3)--> [Raspberry Pi] --LANブロードキャスト--> [寝てる自宅PC]
> ```

#### 5-1. 自宅 PC 側（起こされる側）の設定

- **MAC アドレスを控える**: 自宅 PC で `getmac /v`（または `ipconfig /all` の物理アドレス）。
  WoL に使うのは host が待ち受ける有線/無線 NIC のもの。
- **BIOS/UEFI** で Wake-on-LAN（"Power On by PCI-E" 等）を有効化。
- **NIC ドライバ**: デバイスマネージャ → ネットワークアダプタ → プロパティ
  - 「電源の管理」→「このデバイスで…スタンバイ状態を解除」「Magic Packet でのみ…」を有効
  - 「詳細設定」→「Wake on Magic Packet」を Enabled
- **高速スタートアップを無効化**（完全シャットダウン S5 からの WoL が効かない主因）:
  ```powershell
  powercfg /h off
  ```

> S3（スリープ）/S4（休止）からの復帰が最も安定。運用は **「スリープ + WoL」** 推奨。

#### 5-2. Raspberry Pi 側（中継役）の設定

Pi は自宅 PC と**同じ LAN（同一サブネット）**に接続しておく（有線が確実）。

```bash
# 1. Tailscale を入れて tailnet に参加（常駐する）
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up --ssh          # --ssh で tailnet 内から鍵管理なしに SSH 可能

# 2. マジックパケット送出ツール
sudo apt update && sudo apt install -y wakeonlan

# 3. 動作テスト（自宅 PC をスリープさせてから）
wakeonlan AA:BB:CC:DD:EE:FF      # ← 自宅 PC の MAC
```

控えておく値: `tailscale ip -4`（Pi の 100.x.x.x）または MagicDNS 名（例 `raspberrypi`）。

#### 5-3. 出先から起こす

出先 PC（Tailscale 接続済み）から、Pi へ SSH してコマンドを 1 発打つだけ:

```bash
ssh pi@raspberrypi 'wakeonlan AA:BB:CC:DD:EE:FF'
```

毎回 MAC を打ちたくなければ、Pi に下記ヘルパーを置いておくと「起こして host が
上がるまで待つ」までやってくれる（`~/wake-home.sh`）:

```bash
#!/usr/bin/env bash
# 自宅 PC を WoL で起こし、host のポートが開くまで待つ。
MAC="AA:BB:CC:DD:EE:FF"      # 自宅 PC の MAC
HOST="100.x.x.x"            # 自宅 PC の Tailscale IP
PORT="${1:-7000}"          # host の --port（既定 7000）

echo "sending magic packet to $MAC ..."
wakeonlan "$MAC"
echo -n "waiting for $HOST:$PORT "
until nc -z -w1 "$HOST" "$PORT" 2>/dev/null; do
  echo -n "."
  sleep 2
done
echo " up!"
```

```bash
chmod +x ~/wake-home.sh
# 出先からは:
ssh pi@raspberrypi '~/wake-home.sh 8000'   # 引数は host の --port
```

> WoL のマジックパケット送出自体は本アプリの管轄外（中継役 = Raspberry Pi の役割）。
> 上のスクリプトはあくまで運用例。

---

## 出先からの接続

1. （PC が落ちているなら）Pi 経由で WoL を送って自宅 PC を起こす
   （例: `ssh pi@raspberrypi '~/wake-home.sh 8000'`）
2. 自宅 PC がログオンすると host が自動起動し、`DDD_PASSWORD` で待ち受ける
3. 出先の PC で client を起動
4. 接続先に `100.x.x.x:<port>`、パスワードに `DDD_PASSWORD` の値を入力して接続

---

## トラブルシュート

| 症状 | 確認点 |
|------|--------|
| 接続できない | `tailscale status` で双方がオンラインか。`<port>` が host の `--port` と一致しているか |
| パスワードが通らない | `DDD_PASSWORD` がシステム環境変数か（`/M`）。設定後に再ログオンしたか |
| タスクは動くが映像が来ない | ログオンセッションがアクティブか（ロック画面のみでは DXGI が止まる構成あり）。GPU ドライバ |
| `--install` が失敗する | 管理者権限で実行しているか（`/RL HIGHEST` 登録のため） |
| WoL で起きない | Pi と自宅 PC が同一サブネットか。MAC が正しいか。高速スタートアップ無効化したか。S5 不可なら S3/S4（スリープ/休止）で運用 |
| 自動ログオンしたい | 無人運用で「ログオン時」を満たすには自動ログオン設定が要る（セキュリティと要相談） |
