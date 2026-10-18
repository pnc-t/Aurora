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

## raspi へのインストール

インストール方式は 2 通りある。どちらも最終的にはリモート (raspi) 上で
`aurora-wol install` が実行され、バイナリ配置・パスワード解決・systemd 常駐化・
稼働確認・client 登録情報の表示までを行う。

### 方式 A (開発機からワンコマンド、推奨)

Windows 開発機から raspi へクロスビルド・配備・`install` 実行まで 1 コマンドで行える。
スクリプトはクロスビルド + scp + リモート `install` 実行の薄いラッパーで、
パスワード保持/生成・systemd 化・稼働確認のロジックは持たない。

前提:
- raspi: SSH でログイン可能 (鍵認証推奨)、Tailscale 導入済み
- Windows: Rust toolchain 導入済み (`cargo-zigbuild`/`zig` は無ければスクリプトが導入を試みる)

```powershell
cd crates/wol
.\deploy-aurora-wol.ps1 -Target pi@100.x.x.x
# ポート/sshポートを変える場合
.\deploy-aurora-wol.ps1 -Target pi@100.x.x.x -Port 7100 -SshPort 22
```

初回はパスワード入力を求められる (空 Enter で自動生成、生成値はコンソールに表示される)。
2 回目以降の実行はバイナリ差し替え + サービス再起動のみで、既存パスワードは保持される。

完了後に表示される `ホスト:ポート` と `パスワード` を、client の WoL タブに登録する。

### 方式 B (Pi 単体で完結)

バイナリを 1 ファイル raspi に持ち込めば (scp / USB / 別途ダウンロードなど手段は問わない)、
それだけでインストールできる。

```bash
sudo ./aurora-wol install            # 対話でパスワード設定 (空 Enter で自動生成)
sudo ./aurora-wol install --port 7100 --password 'your-strong-passphrase'
```

注意: `--password` はコマンドライン引数のため `ps` によるプロセス一覧やシェル履歴に
残ってしまう。通常は引数を付けずに実行し、対話プロンプト (空 Enter で自動生成) を
使うことを推奨する。

`install` が行うこと:
- 自分自身を `/usr/local/bin/aurora-wol` へ配置
- パスワードの解決 (`--password` 指定で上書き / 未指定かつ既存があれば保持 / どちらもなければ対話入力・自動生成)
- 専用サービスユーザー `aurora-wol` の作成と、そのユーザーでの systemd 常駐 (sandbox 付き)
- systemd unit の書き込み・有効化・再起動・稼働確認
- client の WoL タブに登録する `ホスト:ポート` と (新規設定時のみ) パスワードの表示

## アップデート

新しいバイナリで `sudo ./aurora-wol.new install` を再実行するだけでよい。既存パスワードは
保持され、client 側の再登録は不要。方式 A の再実行も内部的には同じ動きになる。

重要な注意: host/client と暗号パラメータを共有しているため、ワイヤ非互換の変更
(例: 2026-07 の KDF 強化) を跨ぐ更新では host/client/wol を同じリビジョンで揃えること。

## アンインストール

```bash
sudo aurora-wol uninstall            # パスワードは残す
sudo aurora-wol uninstall --purge    # パスワードも削除
```

`--purge` はパスワードに加えて状態ディレクトリ (`/var/lib/aurora-wol`、ログ含む) と
サービスユーザー `aurora-wol` も削除する。

## 代替手順 (raspi 上で直接ビルド)

方式 A のスクリプトが使えない環境向けの手順。

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

ビルド後は `sudo ./target/release/aurora-wol install` で常駐化できる。手動で unit を
書きたい場合は、install が書く unit は `/etc/systemd/system/aurora-wol.service` に
あるので必要ならそれを編集すればよい。

## 実行

通常は `install` で常駐化するため、直接実行は動作確認用。

```bash
./aurora-wol --port 7100 --password-file /etc/aurora-wol/password
```

- `--port <N>`: 待受ポート (既定 7100。host の 7000 と衝突しないよう別ポート)。
- `--password <PASS>` / `--password-file <PATH>`: 接続パスワード (優先順:
  `--password` > `--password-file`)。host の接続パスワードとは別に運用してよい
  (raspi は非 Windows で DPAPI が使えないため平文ファイル保管)。
- パスワード未指定はエラー終了する。

**`password` は必ず `chmod 600` にすること** (所有者以外の読み取りを禁止する)。

```bash
sudo mkdir -p /etc/aurora-wol
echo -n 'your-strong-passphrase' | sudo tee /etc/aurora-wol/password
sudo chmod 600 /etc/aurora-wol/password
```

`install` 経由であれば所有はサービスユーザー `aurora-wol`、権限は 0400 に自動設定される。

## ログ

host/client と同じ `aurora_common::logging` を使い、`%LOCALAPPDATA%\Aurora\logs`
(非 Windows は `~/.local/share/Aurora/logs`) に `wol.<日付>.log` を日次ローテ
(7 世代) + 非ブロッキングで残す。加えて起動時から stdout にも出す (systemd の
journal で追える)。

`install` で常駐化した場合、unit は `HOME=/var/lib/aurora-wol` で動くため、ログは
`/var/lib/aurora-wol/.local/share/Aurora/logs` に残る。直接実行時は従来どおり
`~/.local/share/Aurora/logs` (実行ユーザーのホーム配下) になる。
