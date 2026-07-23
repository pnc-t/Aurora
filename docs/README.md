# Aurora ドキュメント

Aurora は Tailscale (Tailnet) 直結を前提にした、自作の軽量リモートデスクトップアプリです。
ここには利用者・開発者向けの解説をまとめています。プロジェクト全体の概要は
リポジトリ直下の [README](../README.md) を参照してください。

## 目次

| ドキュメント | 内容 |
|------|------|
| [architecture.md](architecture.md) | クレート構成、ホスト側パイプライン、タスク分割の全体像 |
| [protocol.md](protocol.md) | 制御チャネル (TCP) と映像/音声チャネル (UDP) のワイヤフォーマット |
| [security.md](security.md) | 暗号化・認証・ネットワーク露出・脅威モデルと既知の制約 |
| [build.md](build.md) | 必要環境、ホスト/クライアントのビルドと実行、CLI オプション、WoL リレー (aurora-wol) のビルド、自動アップデート |
| [remote-access-setup.md](remote-access-setup.md) | 出先から自宅 PC へ接続する無人起動セットアップ (自動起動・WoL) |
| [troubleshooting.md](troubleshooting.md) | 接続できない・映像が来ない等の切り分け |

配布インストーラの作り方は [`installer/README.md`](../installer/README.md) にあります。

## どこから読むか

- **まず動かしたい** → [build.md](build.md) → [remote-access-setup.md](remote-access-setup.md)
- **設計を知りたい** → [architecture.md](architecture.md) → [protocol.md](protocol.md)
- **安全性を確認したい** → [security.md](security.md)
- **つまずいた** → [troubleshooting.md](troubleshooting.md)
