<#
.SYNOPSIS
    aurora-wol (WoL 中継デーモン) を Windows 開発機から Raspberry Pi へ 1 コマンドで配備する。

.DESCRIPTION
    クロスビルド + scp + リモートで `aurora-wol install` を実行する薄いラッパー。
    パスワード保持/生成・systemd 化・稼働確認はバイナリ内蔵のインストーラ側が行う。
    再実行は安全 (既存パスワード保持のまま差し替え更新)。

.PARAMETER Target
    ssh 接続先。"user@host" 形式 (host は Tailnet IP や MagicDNS 名を想定)。

.PARAMETER Port
    aurora-wol の待受ポート (既定 7100。host 本体の 7000 と衝突しないよう別ポート)。

.PARAMETER SshPort
    ssh 接続ポート (既定 22)。

.PARAMETER Password
    接続パスワードを明示指定する場合。省略時はリモート側で既存保持 or 対話入力/自動生成。

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

function Invoke-RemoteInstall {
    param([string]$LocalBinPath)

    Write-Step "バイナリを転送しています"
    $remoteTmp = "/tmp/aurora-wol.new"
    & scp -P $SshPort $LocalBinPath "${Target}:${remoteTmp}"
    if ($LASTEXITCODE -ne 0) {
        Write-ErrorAndExit "scp によるバイナリ転送に失敗しました。"
    }

    $remoteInstallCmd = "sudo $remoteTmp install --port $Port"
    if ($Password) {
        # 単一引用符で包んだ上で、値中の ' を '\'' に置換してリモートシェルでの展開を防ぐ
        $escapedPassword = $Password.Replace("'", "'\''")
        $remoteInstallCmd += " --password '$escapedPassword'"
    }
    # Windows からの scp は実行ビットが付かないため
    $remoteCommand = "chmod +x $remoteTmp && $remoteInstallCmd && rm -f $remoteTmp"

    Write-Step "リモートで aurora-wol install を実行しています"
    # -t: install の対話プロンプト (sudo のパスワード・aurora-wol パスワード入力) に tty が必要なため
    & ssh -t -p $SshPort $Target $remoteCommand
    if ($LASTEXITCODE -ne 0) {
        Write-ErrorAndExit "aurora-wol install の実行に失敗しました。`ssh $Target sudo journalctl -u aurora-wol -e` でログを確認してください。"
    }
}

# --- メイン処理 ---

Test-Prerequisites

$remoteArch = Get-RemoteArchitecture
$rustTarget = Get-RustTarget -RemoteArch $remoteArch
Confirm-RustupTarget -RustTarget $rustTarget
$binPath = Build-AuroraWol -RustTarget $rustTarget

Invoke-RemoteInstall -LocalBinPath $binPath

Write-Host ""
Write-Host "デプロイ完了。登録情報は上の install 出力を参照してください。" -ForegroundColor Green
