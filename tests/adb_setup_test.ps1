# End-to-end tests for adb_setup.ps1 (mirrors tests/adb_setup_test.sh).
# Expects to be run on a Windows runner with PowerShell 5.1+.
#   PS> .\tests\adb_setup_test.ps1
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$script   = Join-Path $repoRoot 'adb_setup.ps1'
$passed   = 0

function Fail([string]$msg) {
    Write-Host "FAIL: $msg" -ForegroundColor Red
    exit 1
}

function Assert-Contains([string]$haystack, [string]$needle) {
    if ($haystack -notlike "*$needle*") {
        Fail "expected to contain '$needle', got:`n$haystack"
    }
}

function Assert-Equals($expected, $actual) {
    if ($expected -cne $actual) {
        Fail "expected '$expected', got '$actual'"
    }
}

# ---------------------------------------------------------------------------
# test 1: -Config writes a valid TOML with the entered backend
# ---------------------------------------------------------------------------
function Test-SetupWritesToml {
    $tmp   = Join-Path $env:TEMP "adb_test_$(Get-Random)"
    $home_ = Join-Path $tmp 'home'
    New-Item -ItemType Directory -Path $home_ -Force | Out-Null

    try {
        # Simulate user input: name, host, port
        'office', '10.0.0.8', '5038' | & powershell -NoProfile -Command "
            `$env:HOME         = '$home_'
            `$env:USERPROFILE  = '$home_'
            `$env:APPDATA      = Join-Path `$env:HOME 'AppData' 'Roaming'
            `$env:ADB_SETUP_SKIP_DOWNLOAD = '1'
            & '$script' -Config
        "

        $cfg = Join-Path $home_ 'AppData\Roaming\adb-hub\config.toml'
        if (-not (Test-Path $cfg)) { Fail "missing $cfg" }
        $body = Get-Content $cfg -Raw

        Assert-Contains $body 'listen = "127.0.0.1:5037"'
        Assert-Contains $body 'include_local = true'
        Assert-Contains $body 'name = "office"'
        Assert-Contains $body 'addr = "10.0.0.8:5038"'
    }
    finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
    Write-Host 'Test-SetupWritesToml: ok' -ForegroundColor Green
    $script:passed++
}

# ---------------------------------------------------------------------------
# test 2: legacy ~/.adbproxy seeds default values
# ---------------------------------------------------------------------------
function Test-SetupFromLegacyDefaults {
    $tmp   = Join-Path $env:TEMP "adb_test_$(Get-Random)"
    $home_ = Join-Path $tmp 'home'
    New-Item -ItemType Directory -Path $home_ -Force | Out-Null

    @"
host=192.168.1.9
port=5038
"@ | Set-Content (Join-Path $home_ '.adbproxy')

    try {
        # Accept all defaults (three empty lines)
        "`n", "`n", "`n" | & powershell -NoProfile -Command "
            `$env:HOME         = '$home_'
            `$env:USERPROFILE  = '$home_'
            `$env:APPDATA      = Join-Path `$env:HOME 'AppData' 'Roaming'
            `$env:ADB_SETUP_SKIP_DOWNLOAD = '1'
            & '$script' -Config
        "

        $cfg = Join-Path $home_ 'AppData\Roaming\adb-hub\config.toml'
        $body = Get-Content $cfg -Raw

        Assert-Contains $body 'name = "remote"'
        Assert-Contains $body 'addr = "192.168.1.9:5038"'
    }
    finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
    Write-Host 'Test-SetupFromLegacyDefaults: ok' -ForegroundColor Green
    $script:passed++
}

# ---------------------------------------------------------------------------
# test 3: -Install extracts binaries from a mock release archive
# ---------------------------------------------------------------------------
function Test-InstallFromMockArchive {
    $tmp    = Join-Path $env:TEMP "adb_test_$(Get-Random)"
    $home_  = Join-Path $tmp 'home'
    $binDir = Join-Path $tmp 'bin'
    $stage  = Join-Path $tmp 'staging'
    $pathDir = Join-Path $tmp 'path'
    New-Item -ItemType Directory -Path $home_, $binDir, $stage, $pathDir -Force | Out-Null

    # Create fake executables
    Set-Content (Join-Path $stage 'adb-hub.exe')  '@hub'
    Set-Content (Join-Path $stage 'adb-proxy.exe') '@proxy'

    $archive = 'adb-proxy-windows-x86_64.tar.gz'
    Push-Location $stage
    try {
        & tar -czf (Join-Path $tmp $archive) adb-hub.exe adb-proxy.exe
        if ($LASTEXITCODE -ne 0) { Fail 'mock archive creation failed' }
    }
    finally { Pop-Location }

    # Fake curl: respond to /releases/latest with a tag, then serve the archive
    $curlMock = Join-Path $pathDir 'curl.cmd'
    @"
@echo off
setlocal
set "ARGS=%*"
echo %ARGS% | findstr /C:"releases/latest" >nul && (
    echo {"tag_name":"v9.9.9"}
    exit /b 0
)
echo %ARGS% | findstr /C:"-o" >nul && (
    for /f "tokens=2 delims= " %%%%a in ("echo %ARGS% -o") do set "OUT=%%a"
    copy "$tmp\$archive" "%%OUT%%" >nul
    exit /b 0
)
exit /b 1
"@ | Set-Content $curlMock

    try {
        # Put fake curl + install dir first on PATH so the script finds them.
        $env:PATH     = "$pathDir;$binDir;$env:PATH"
        $env:HOME     = $home_
        $env:USERPROFILE = $home_
        $env:APPDATA  = Join-Path $home_ 'AppData\Roaming'
        $env:ADB_PROXY_INSTALL_DIR = $binDir

        & powershell -NoProfile -Command "& '$script' -Install"

        $hub = Join-Path $binDir 'adb-hub.exe'
        $prx = Join-Path $binDir 'adb-proxy.exe'
        if (-not (Test-Path $hub)) { Fail 'adb-hub.exe not installed' }
        if (-not (Test-Path $prx)) { Fail 'adb-proxy.exe not installed' }
        Assert-Equals '@hub'   (Get-Content $hub -Raw).Trim()
        Assert-Equals '@proxy' (Get-Content $prx -Raw).Trim()
    }
    finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
    Write-Host 'Test-InstallFromMockArchive: ok' -ForegroundColor Green
    $script:passed++
}

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------
Test-SetupWritesToml
Test-SetupFromLegacyDefaults
Test-InstallFromMockArchive

Write-Host "`nadb_setup_test.ps1: ok ($passed tests)" -ForegroundColor Green
