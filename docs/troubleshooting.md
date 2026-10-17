# トラブルシュート

症状から確認点を引くための早見表です。無人起動 (自動起動・WoL) 特有の項目は
[remote-access-setup.md](remote-access-setup.md) の末尾にも詳しい表があります。

## 接続まわり

| 症状 | 確認点 |
|------|------|
| 接続できない | `tailscale status` で双方がオンラインか。client の接続先 `<port>` が host の `--port` と一致しているか |
| host に届かない (Tailnet 経由) | host が Tailscale IP に bind しているか (起動ログで確認)。Tailscale 未導入だと `127.0.0.1` (loopback) に fail-closed し外部から到達不能。LAN 公開するなら `--bind <LAN IP>`、全 NIC なら `--bind 0.0.0.0` を明示する |
| ポートには届くはずなのに繋がらない | Windows ファイアウォールの受信許可規則 "Aurora Host" があるか (`Get-NetFirewallRule -DisplayName "Aurora Host"`)。サービスで動く host には Windows の許可確認が出ないため、規則が無いと黙って遮断される。無ければ管理者で `aurora-host --install-service` を実行し直すか、インストーラを再実行する |
| パスワードが通らない | client の入力値が host の保存値と一致しているか。`%ProgramData%\Aurora\host\password.bin` があるか。CLI (`--set-password`) で変えたならサービスを再起動したか (`Restart-Service AuroraHost`) |
| ハンドシェイクで切れる | パスワード (PSK) 不一致が最有力。各ステップ 10 秒でタイムアウトするため、無応答だと切断される |
| 5 台目から接続できない | host の同時接続は最大 4。上限到達時は新規接続が拒否される (host ログに `client limit reached`)。不要なセッションを切ってから接続する |
| Wi-Fi↔有線の切替や NIC 変更で切れる | シームレス再接続が自動でネットワーク切替を検知し裏で再接続を試みる。数秒待っても復帰しない場合は client を再接続する |

## 映像/音声

| 症状 | 確認点 |
|------|------|
| 接続はできるが映像が来ない | サービス "AuroraHost" が動いているか (`Get-Service AuroraHost`)。GPU ドライバ。host ログ (`%ProgramData%\Aurora\host\logs\host.<日付>.log`) を確認 |
| ノート PC を host にすると映像が真っ暗 / エンコーダ初期化に失敗する | ハイブリッド GPU (Optimus 等) 構成でキャプチャ GPU とエンコード GPU が食い違っている可能性。`--encode-gpu <名前の一部>` でエンコードに使う GPU を明示指定する (既定はキャプチャと同一 GPU) |
| カクつく / 遅延が増える | 高リフレッシュレートモニタでデコードが溢れている可能性。`--fps` で送出上限を下げる。帯域が細いなら `--max-bitrate` を下げる |
| UAC やロック画面に切り替わった瞬間に 1〜2 秒止まる | 正常。デスクトップが切り替わると画面取り込み用の子プロセス (取り込みワーカー) を作り直すため。接続は切れず、そのまま続く |
| 画面が乱れて戻らない | パケットロスでキーフレーム待ち。ツールバーから再要求するか、一時的なら次のキーフレームで回復する |
| 映像が眩しい/白飛びする | HDR 有効ディスプレイの既知制約 (SDR トーンマップ)。ディスプレイの HDR を切ると回避できる |
| 1440p 等の高解像度で HDR 有効時にカクつく | ハードウェアエンコーダがビットレート上限を尊重せずフレームレートが落ちる既知制約。ディスプレイの HDR を切った SDR 運用を推奨 |
| 音が出ない | host 起動時に音声ループバックの初期化に失敗すると映像のみで継続する (ログに `audio capture disabled`)。再生デバイスを確認 |

## 自動起動 / 無人運用

| 症状 | 確認点 |
|------|------|
| `--install-service` が失敗する | 管理者権限で実行しているか |
| サービスは動くが映像が来ない | `Get-Service AuroraHost` が Running か。`%ProgramData%\Aurora\host\logs\service.<日付>.log` に本体の起動失敗が無いか |
| トレイアイコンが出ない | サインインしているのが管理者か (管理者でないユーザーではトレイを出さない)。閉じた場合はスタートメニューの Aurora Host を起動すると UAC 確認のあとトレイだけ出し直す (サービス稼働中は本体を二重起動しない)。トレイのログは `%LOCALAPPDATA%\Aurora\logs\tray.<日付>.log` |
| WoL で起きない | 中継役 (`aurora-wol`) と自宅 PC が同一サブネットか。BIOS/UEFI と NIC ドライバで Wake-on-LAN が有効か。MAC が正しいか。高速スタートアップを無効化したか (`powercfg /h off`)。S5 不可なら S3/S4 (スリープ/休止) で運用。詳細は [`crates/wol/README.md`](../crates/wol/README.md) を参照 |
| 自動ログオンしたい | サービス運用では不要 (サインイン画面のまま接続し、遠隔でサインインできる) |

## パスワード変更ができない

| 症状 | 確認点 |
|------|------|
| `--set-password` / `--clear-password` が拒否される | クライアント接続中は変更できない (締め出し防止)。切断してから実行する |
| 「current password does not match」 | 既存パスワードがある場合は現在のパスワード照合が要る。トレイの「パスワード変更」からも同様 |

## インストール時の警告

| 症状 | 確認点 |
|------|------|
| SmartScreen / Defender が警告する | 配布インストーラは未署名。リモート操作 + 入力注入 + 画面キャプチャという性質上、誤検知もあり得る。配布元を信頼できる場合のみ続行する (自動更新でダウンロードされる更新物は別途 minisign 署名を検証している) |

## ログの見方

ホストのログは既定で `aurora=info`。詳細を見たいときは:

```powershell
# 端末起動 (debug ビルド) なら環境変数で
$env:RUST_LOG = "aurora=debug"; cargo run -p aurora-host

# トレイ常駐 (リリース) は引数でファイルログのレベルを上げる
.\aurora-host.exe --log-level debug
# 出力先: %ProgramData%\Aurora\host\logs\host.<日付>.log (サービス本体は service.<日付>.log)
```
