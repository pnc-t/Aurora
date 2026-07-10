# aurora-wol

自宅 LAN に常時稼働する小型機 (Raspberry Pi 等) の上で常駐し、Tailnet 経由の
wake 要求を受けて対象 PC へ Wake-on-LAN のマジックパケットを送るリレーデーモン。

client (出先の PC) → Tailnet → `aurora-wol` (自宅の raspi 等) → LAN ブロードキャスト
→ 対象 PC、という経路で、Tailscale が導入されていない (=電源が落ちていて起動できない)
対象 PC を起こす。

## 前提

- 対象 PC (起こしたい自宅 PC) で **Wake-on-LAN が有効**になっていること。
  - BIOS/UEFI: 「Wake on LAN」「PCIe デバイスからの復帰」等を有効化。
  - Windows: デバイスマネージャ → 対象 NIC のプロパティ → 「電源の管理」タブで
    「このデバイスで、コンピューターのスタンバイ状態を解除できるようにする」を
    有効化。加えて高速スタートアップは WoL と相性が悪いことがあるため無効化を推奨。
  - シャットダウン後も NIC に補助電源が供給される必要がある (メーカーにより
    「S5 での WoL」の設定項目が別にある場合がある)。
- `aurora-wol` を動かす機器と対象 PC が**同一 LAN (同一ブロードキャストドメイン)** に
  いること。ルータを跨いだ WoL は行わない (マジックパケットはブロードキャストのため)。

## ビルド

Raspberry Pi 上で直接ビルドする場合 (Rust toolchain がインストール済み前提):

```bash
cd crates/wol
cargo build --release
# 生成物: target/release/aurora-wol
```

開発機 (Windows 等) から Raspberry Pi 向けにクロスコンパイルする場合は
[`cross`](https://github.com/cross-rs/cross) を使うと楽:

```bash
cross build --release --target aarch64-unknown-linux-gnu -p aurora-wol
```

Windows 上でもそのままビルド・実行できる (OS 依存 API を使っていないため)。

## 実行

```bash
./aurora-wol --port 7100 --password-file /etc/aurora-wol/password.txt
```

- `--port <N>`: 待受ポート (既定 7100。host の 7000 と衝突しないよう別ポート)。
- `--password <PASS>` / `--password-file <PATH>`: 接続パスワード (優先順:
  `--password` > `--password-file`)。host の接続パスワードとは別に運用してよい
  (raspi は非 Windows で DPAPI が使えないため平文ファイル保管)。
- パスワード未指定はエラー終了する。

**`password.txt` は必ず `chmod 600` にすること** (所有者以外の読み取りを禁止する)。

```bash
sudo mkdir -p /etc/aurora-wol
echo -n 'your-strong-passphrase' | sudo tee /etc/aurora-wol/password.txt
sudo chmod 600 /etc/aurora-wol/password.txt
```

## systemd ユニット例

`/etc/systemd/system/aurora-wol.service`:

```ini
[Unit]
Description=Aurora WoL relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/aurora-wol --port 7100 --password-file /etc/aurora-wol/password.txt
Restart=on-failure
RestartSec=5
User=aurora-wol
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --no-create-home aurora-wol
sudo cp target/release/aurora-wol /usr/local/bin/aurora-wol
sudo systemctl daemon-reload
sudo systemctl enable --now aurora-wol
sudo journalctl -u aurora-wol -f
```

## ログ

host/client と同じ `aurora_common::logging` を使い、`%LOCALAPPDATA%\Aurora\logs`
(非 Windows は `~/.local/share/Aurora/logs`) に `wol.<日付>.log` を日次ローテ
(7 世代) + 非ブロッキングで残す。加えて起動時から stdout にも出す (systemd の
journal で追える)。
