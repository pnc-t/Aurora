<#
.SYNOPSIS
    aurora-wol (WoL 中継デーモン) を Windows 開発機から Raspberry Pi へ 1 コマンドで配備する。

.DESCRIPTION
    cargo-zigbuild + zig でクロスビルドし、scp/ssh で raspi 側に配置・systemd 化まで行う。
    再実行時はバイナリを差し替えて再起動する (パスワード/unit ファイルは既存を尊重)。

.PARAMETER Target
    ssh 接続先。"user@host" 形式 (host は Tailnet IP や MagicDNS 名を想定)。

.PARAMETER Port
    aurora-wol の待受ポート (既定 7100。host 本体の 7000 と衝突しないよう別ポート)。

.PARAMETER SshPort
    ssh 接続ポート (既定 22)。

.PARAMETER Password
    接続パスワードを明示指定する場合。省略時は既存パスワードの保持 or 対話入力/自動生成。

.EXAMPLE
    .\deploy-aurora-wol.ps1 -Target pi@100.64.1.23
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Target,

    [int]$Port = 7100,

    [int]$SshPort = 22,

    [string]$Password
)

$ErrorActionPreference = "Stop"

# スクリプトは crates/wol 直下に置く想定。ワークスペースルートはここから2階層上。
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $ScriptDir "..\..")

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-ErrorAndExit {
    param([string]$Message)
    Write-Host "エラー: $Message" -ForegroundColor Red
    exit 1
}

function Invoke-Ssh {
    param([string]$Command)
    # -o BatchMode=no: パスワード認証併用環境もあるため対話を許容する
    & ssh -p $SshPort $Target $Command
    if ($LASTEXITCODE -ne 0) {
        Write-ErrorAndExit "ssh コマンド実行に失敗しました: $Command"
    }
}

function Test-Prerequisites {
    Write-Step "前提ツールを確認しています (ssh/scp, cargo-zigbuild, zig)"

    if (-not (Get-Command ssh -ErrorAction SilentlyContinue)) {
        Write-ErrorAndExit "ssh が見つかりません。Windows の「アプリと機能」→「オプション機能」から OpenSSH クライアントを追加してください。"
    }
    if (-not (Get-Command scp -ErrorAction SilentlyContinue)) {
        Write-ErrorAndExit "scp が見つかりません。Windows の「アプリと機能」→「オプション機能」から OpenSSH クライアントを追加してください。"
    }

    # cargo-zigbuild はサブコマンドなので `cargo zigbuild --version` の成否で判定する
    & cargo zigbuild --version *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Step "cargo-zigbuild が見つからないためインストールします"
        & cargo install cargo-zigbuild
        if ($LASTEXITCODE -ne 0) {
            Write-ErrorAndExit "cargo install cargo-zigbuild に失敗しました。手動で `cargo install cargo-zigbuild` を実行してください。"
        }
    }

    if (-not (Get-Command zig -ErrorAction SilentlyContinue)) {
        Write-Step "zig が見つからないため winget でインストールを試みます"
        & winget install -e --id zig.zig
        if ($LASTEXITCODE -ne 0 -or -not (Get-Command zig -ErrorAction SilentlyContinue)) {
            Write-ErrorAndExit "zig の自動インストールに失敗しました。手動で https://ziglang.org/download/ から導入し、PATH を通してから再実行してください。"
        }
    }
}

function Get-RustTarget {
    param([string]$RemoteArch)

    switch ($RemoteArch) {
        "aarch64" { return "aarch64-unknown-linux-musl" }
        "armv7l" { return "armv7-unknown-linux-musleabihf" }
        default {
            Write-ErrorAndExit "未対応のアーキテクチャです: $RemoteArch (aarch64 / armv7l のみ対応)"
        }
    }
}

function Get-RemoteArchitecture {
    Write-Step "raspi 側のアーキテクチャを確認しています ($Target)"
    $arch = (& ssh -p $SshPort $Target "uname -m").Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($arch)) {
        Write-ErrorAndExit "raspi への ssh 接続または uname -m の実行に失敗しました。$Target への到達性・鍵認証を確認してください。"
    }
    Write-Host "  検出アーキテクチャ: $arch"
    return $arch
}

function Confirm-RustupTarget {
    param([string]$RustTarget)

    Write-Step "rustup target ($RustTarget) を確認しています"
    $installed = & rustup target list --installed
    if ($LASTEXITCODE -ne 0) {
        Write-ErrorAndExit "rustup target list --installed の実行に失敗しました。rustup がインストールされているか確認してください。"
    }
    if ($installed -notcontains $RustTarget) {
        Write-Step "rustup target ($RustTarget) を追加します"
        & rustup target add $RustTarget
        if ($LASTEXITCODE -ne 0) {
            Write-ErrorAndExit "rustup target add $RustTarget に失敗しました。"
        }
    }
}

function Build-AuroraWol {
    param([string]$RustTarget)

    Write-Step "cargo zigbuild でクロスビルドします (target: $RustTarget)"
    Push-Location $RepoRoot
    try {
        & cargo zigbuild -p aurora-wol --release --target $RustTarget
        if ($LASTEXITCODE -ne 0) {
            Write-ErrorAndExit "cargo zigbuild に失敗しました。上記のビルドログを確認してください。"
        }
    }
    finally {
        Pop-Location
    }

    $binPath = Join-Path $RepoRoot "target\$RustTarget\release\aurora-wol"
    if (-not (Test-Path $binPath)) {
        Write-ErrorAndExit "ビルド成果物が見つかりません: $binPath"
    }
    return $binPath
}

function Test-RemoteFileExists {
    param([string]$RemotePath)
    & ssh -p $SshPort $Target "sudo test -f $RemotePath"
    return ($LASTEXITCODE -eq 0)
}

function Resolve-DeployPassword {
    # 既存パスワードを勝手に上書きすると client 側の登録済みパスワードと食い違うため、
    # 既存があれば必ず保持する (再実行を安全にするための規約)。
    if ($Password) {
        Write-Step "指定されたパスワードを使用します"
        return $Password
    }

    Write-Step "raspi 上の既存パスワードを確認しています"
    if (Test-RemoteFileExists "/etc/aurora-wol/password") {
        Write-Host "  既存パスワードを保持します"
        return $null
    }

    # $input は PowerShell の予約自動変数のため使わない (代入が環境依存で失敗する)。
    $entered = Read-Host "aurora-wol の接続パスワードを入力してください (空 Enter で自動生成)"
    if ([string]::IsNullOrWhiteSpace($entered)) {
        $generated = New-RandomPassword
        Write-Host ""
        Write-Host "自動生成したパスワード (client の WoL タブ登録に必要。控えてください):" -ForegroundColor Yellow
        Write-Host "  $generated" -ForegroundColor Yellow
        Write-Host ""
        return $generated
    }
    return $entered
}

function New-RandomPassword {
    # 英数24文字。記号は shell/ini エスケープの手間が増えるため避ける。
    $chars = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789"
    $rand = [System.Random]::new()
    $sb = [System.Text.StringBuilder]::new()
    for ($i = 0; $i -lt 24; $i++) {
        [void]$sb.Append($chars[$rand.Next($chars.Length)])
    }
    return $sb.ToString()
}

function Deploy-Binary {
    param([string]$LocalBinPath)

    Write-Step "バイナリを転送しています"
    $remoteTmp = "/tmp/aurora-wol.new"
    & scp -P $SshPort $LocalBinPath "${Target}:${remoteTmp}"
    if ($LASTEXITCODE -ne 0) {
        Write-ErrorAndExit "scp によるバイナリ転送に失敗しました。"
    }

    Write-Step "バイナリを配置しています (sudo install)"
    Invoke-Ssh "sudo install -m 755 $remoteTmp /usr/local/bin/aurora-wol && rm -f $remoteTmp"
}

function Deploy-PasswordAndUnit {
    param(
        [string]$PasswordToWrite,
        [int]$ServicePort
    )

    # sudo プロンプトを最小化するため、パスワード書き込み・unit 配置・有効化を
    # 1 回の `sudo bash -c` にまとめる。
    $unitContent = @"
[Unit]
Description=Aurora WoL relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/aurora-wol --port $ServicePort --password-file /etc/aurora-wol/password
Restart=always
RestartSec=5
User=root

[Install]
WantedBy=multi-user.target
"@
    # User=root: ブロードキャスト送出/待受自体に root 権限は不要だが、
    # 専用ユーザー作成の手間を省くため簡素化として root で動かす。

    $unitB64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($unitContent))

    if ($PasswordToWrite) {
        $passB64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($PasswordToWrite))
        $remoteScript = @"
set -e
mkdir -p /etc/aurora-wol
echo '$passB64' | base64 -d > /etc/aurora-wol/password
chmod 600 /etc/aurora-wol/password
echo '$unitB64' | base64 -d > /etc/systemd/system/aurora-wol.service
systemctl daemon-reload
systemctl enable --now aurora-wol
systemctl restart aurora-wol
"@
    }
    else {
        $remoteScript = @"
set -e
echo '$unitB64' | base64 -d > /etc/systemd/system/aurora-wol.service
systemctl daemon-reload
systemctl enable --now aurora-wol
systemctl restart aurora-wol
"@
    }

    Write-Step "パスワード/systemd unit を配置し、サービスを有効化しています"
    $scriptB64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($remoteScript))
    Invoke-Ssh "echo $scriptB64 | base64 -d | sudo bash -c 'bash -s'"
}

function Confirm-ServiceRunning {
    param([int]$ServicePort)

    Write-Step "サービスの稼働状態を確認しています"

    $active = (& ssh -p $SshPort $Target "systemctl is-active aurora-wol").Trim()
    if ($active -ne "active") {
        Write-ErrorAndExit "aurora-wol サービスが起動していません (状態: $active)。`ssh $Target sudo journalctl -u aurora-wol -e` でログを確認してください。"
    }
    Write-Host "  systemctl is-active: active"

    & ssh -p $SshPort $Target "sudo ss -tlnp | grep -q ':$ServicePort '"
    if ($LASTEXITCODE -ne 0) {
        Write-ErrorAndExit "ポート $ServicePort での待受が確認できませんでした。`ssh $Target sudo journalctl -u aurora-wol -e` でログを確認してください。"
    }
    Write-Host "  ポート $ServicePort で待受中"
}

# --- メイン処理 ---

Test-Prerequisites

$remoteArch = Get-RemoteArchitecture
$rustTarget = Get-RustTarget -RemoteArch $remoteArch
Confirm-RustupTarget -RustTarget $rustTarget
$binPath = Build-AuroraWol -RustTarget $rustTarget

$deployPassword = Resolve-DeployPassword

Deploy-Binary -LocalBinPath $binPath
Deploy-PasswordAndUnit -PasswordToWrite $deployPassword -ServicePort $Port
Confirm-ServiceRunning -ServicePort $Port

$hostForClient = ($Target -split "@")[-1]
Write-Host ""
Write-Host "デプロイ完了。client の WoL タブに以下を登録してください:" -ForegroundColor Green
Write-Host "  ホスト: ${hostForClient}:${Port}"
if ($deployPassword) {
    Write-Host "  パスワード: $deployPassword"
}
else {
    Write-Host "  パスワード: (既存のものを使用。未把握なら raspi 上の /etc/aurora-wol/password を確認)"
}
