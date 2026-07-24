# Install latest adb-hub + adb-proxy from GitHub Releases and write client config.
# Does not replace the official adb binary.
#
# Usage:
#   .\adb_setup.ps1                 # download + install, then interactive config
#   .\adb_setup.ps1 -Install        # download + install only
#   .\adb_setup.ps1 -Config         # interactive config only
#
# Environment:
#   $env:ADB_PROXY_INSTALL_DIR   Install directory (default: $HOME\.local\bin)
#   $env:ADB_PROXY_REPO          GitHub repo (default: Ken-u/adbproxy-rs)
#   $env:ADB_SETUP_SKIP_DOWNLOAD=1   Skip download (tests / offline config)
[CmdletBinding()]
param(
    [switch]$Install,
    [switch]$Config,
    [switch]$Help
)

$ErrorActionPreference = 'Stop'

$Repo         = if ($env:ADB_PROXY_REPO) { $env:ADB_PROXY_REPO } else { 'Ken-u/adbproxy-rs' }
$InstallDir   = if ($env:ADB_PROXY_INSTALL_DIR) { $env:ADB_PROXY_INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
$ConfigDir    = Join-Path $HOME '.config\adb-hub'
$ConfigFile   = Join-Path $ConfigDir 'config.toml'
$LegacyConfig = Join-Path $HOME '.adbproxy'
$ApiBase      = "https://api.github.com/repos/$Repo"
$ReleaseBase  = "https://github.com/$Repo/releases/download"

function Show-Help {
    Write-Host @"
adb-proxy / adb-hub setup

Downloads the latest GitHub release for this OS/arch, installs adb-hub and
adb-proxy into $InstallDir, and optionally writes client config.

Usage:
  .\adb_setup.ps1                 Download+install, then interactive config
  .\adb_setup.ps1 -Install        Download+install only
  .\adb_setup.ps1 -Config         Interactive config only
  .\adb_setup.ps1 -Help

Environment:
  `$env:ADB_PROXY_INSTALL_DIR   Install directory (default: `$HOME\.local\bin)
  `$env:ADB_PROXY_REPO          GitHub repo (default: Ken-u/adbproxy-rs)
  `$env:ADB_SETUP_SKIP_DOWNLOAD=1   Skip download (tests / offline config)
"@
}

function Test-Host([string]$h) { -not [string]::IsNullOrWhiteSpace($h) }
function Test-Port([string]$p) {
    $n = 0
    [int]::TryParse($p, [ref]$n) -and $n -ge 1 -and $n -le 65535
}
function Test-Name([string]$n) {
    -not [string]::IsNullOrWhiteSpace($n) -and $n -notmatch '[\s=]'
}

function Fetch-LatestTag {
    $resp = Invoke-RestMethod -Uri "$ApiBase/releases/latest" -Headers @{ 'User-Agent' = 'adb_setup.ps1' }
    if (-not $resp.tag_name) {
        throw "could not parse latest release tag from GitHub."
    }
    return $resp.tag_name
}

function Download-And-Install {
    $archive = 'adb-proxy-windows-x86_64.tar.gz'
    $tag     = Fetch-LatestTag
    $url     = "$ReleaseBase/$tag/$archive"

    Write-Host "Installing adb-hub + adb-proxy $tag"
    Write-Host "  archive: $archive"
    Write-Host "  from:    $url"
    Write-Host "  into:    $InstallDir"

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP "adb_setup_$(Get-Random)")
    try {
        $archivePath = Join-Path $tmp $archive
        Invoke-WebRequest -Uri $url -OutFile $archivePath -UseBasicParsing

        $staging = Join-Path $tmp 'extract'
        New-Item -ItemType Directory -Path $staging | Out-Null
        # Windows tar reads .tar.gz natively
        tar -xzf $archivePath -C $staging
        if ($LASTEXITCODE -ne 0) { throw "tar extraction failed (exit $LASTEXITCODE). Is tar available?" }

        foreach ($bin in 'adb-hub', 'adb-proxy') {
            $src = Join-Path $staging "$bin.exe"
            $dst = Join-Path $InstallDir "$bin.exe"
            if (-not (Test-Path $src)) {
                Write-Error "archive missing $bin.exe"
                Get-ChildItem $staging | Format-Table
                throw "missing $bin.exe"
            }
            Copy-Item $src $dst -Force
            Write-Host "Installed $dst"
        }

        Ensure-PathHint
    }
    finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

function Ensure-PathHint {
    $pathUser = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($pathUser -and ($pathUser.Split(';') -contains $InstallDir)) { return }

    Write-Host ""
    Write-Host "NOTE: $InstallDir is not in your user PATH."
    if (-not $Host.UI.RawUI.WindowTitle -or -not [Environment]::UserInteractive) {
        Write-Host "Add this manually:"
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"$InstallDir;`$([Environment]::GetEnvironmentVariable('Path','User'))`", 'User')"
        return
    }

    $ans = Read-Host "Append $InstallDir to user PATH? [Y/n]"
    if ($ans -match '^(n|no)$') { return }
    $newPath = if ($pathUser) { "$InstallDir;$pathUser" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "Appended to user PATH — open a new terminal for it to take effect."
}

function Write-TomlConfig([string]$Name, [string]$Host_, [string]$Port) {
    New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
    @"
listen = "127.0.0.1:5037"
poll_interval_ms = 1000
include_local = true
local_adb_port = 5039

[[backend]]
name = "$Name"
addr = "${Host_}:$Port"
"@ | Set-Content -Path $ConfigFile -Encoding UTF8
    Write-Host "Wrote $ConfigFile"
}

function Prompt-And-Save {
    $defaultName  = 'remote'
    $defaultHost  = ''
    $defaultPort  = '5038'

    if (Test-Path $LegacyConfig) {
        foreach ($line in Get-Content $LegacyConfig) {
            if ($line -match '^\s*host\s*=\s*(.+?)\s*$') { $defaultHost = $Matches[1].Trim() }
            elseif ($line -match '^\s*port\s*=\s*(.+?)\s*$') { $defaultPort = $Matches[1].Trim() }
        }
    }

    $name = $null
    while ($true) {
        $prompt = if ($defaultName) { "Backend name [$defaultName]" } else { "Backend name" }
        $name = (Read-Host $prompt)
        if (-not $name) { $name = $defaultName }
        if (Test-Name $name) { break }
        Write-Host "Error: name must be non-empty and must not contain spaces or '='." -ForegroundColor Red
    }

    $host_ = $null
    while ($true) {
        if ($defaultHost) {
            $host_ = Read-Host "Remote adb-proxy host [$defaultHost]"
            if (-not $host_) { $host_ = $defaultHost }
        } else {
            $host_ = Read-Host "Remote adb-proxy host"
        }
        if (Test-Host $host_) { break }
        Write-Host "Error: host is required." -ForegroundColor Red
        $defaultHost = ''
    }

    $port = $null
    while ($true) {
        $port = Read-Host "Remote adb-proxy port [$defaultPort]"
        if (-not $port) { $port = $defaultPort }
        if (Test-Port $port) { break }
        Write-Host "Error: port must be 1-65535." -ForegroundColor Red
    }

    Write-TomlConfig $name $host_ $port
}

function Print-NextSteps {
    $hub   = Join-Path $InstallDir 'adb-hub.exe'
    $proxy = Join-Path $InstallDir 'adb-proxy.exe'
    Write-Host @"

Done.

Client (this machine):
  adb kill-server
  $hub --config $ConfigFile
  adb devices

Device host (USB machine):
  adb start-server
  $proxy --listen 0.0.0.0:5038 --target 127.0.0.1:5037

Re-run install only:   .\adb_setup.ps1 -Install
Config only:           .\adb_setup.ps1 -Config
"@
}

if ($Help) { Show-Help; return }

if ($Install) {
    Download-And-Install
}
elseif ($Config) {
    Prompt-And-Save
    Print-NextSteps
}
else {
    if ($env:ADB_SETUP_SKIP_DOWNLOAD -ne '1') {
        Download-And-Install
        Write-Host ""
    }
    Prompt-And-Save
    Print-NextSteps
}
