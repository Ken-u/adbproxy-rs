# Tests for adb_setup.ps1 — designed to run in CI (non-interactive pwsh).
#   pwsh> .\tests\adb_setup_test.ps1
#
# Strategy: dot-source adb_setup.ps1 (guard at bottom prevents auto-run),
# then call non-interactive functions directly with overridden script-scope
# variables.
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
    if ("$expected" -cne "$actual") {
        Fail "expected '$expected', got '$actual'"
    }
}

# Dot-source the script so its functions/variables are available.  The guard
# at the bottom of adb_setup.ps1 prevents Invoke-Main from auto-running.
. $script

# ===========================================================================
# Test 1: Write-TomlConfig produces valid TOML
# ===========================================================================
function Test-WriteTomlConfig {
    $tmp = Join-Path $env:TEMP "adb_test_$(Get-Random)"
    $cfgDir = Join-Path $tmp 'config'
    New-Item -ItemType Directory -Path $cfgDir -Force | Out-Null

    # Override the script-scope variables that Write-TomlConfig reads.
    $script:ConfigDir  = $cfgDir
    $script:ConfigFile = Join-Path $cfgDir 'config.toml'

    Write-TomlConfig 'office' '10.0.0.8' '5038'

    $body = Get-Content $script:ConfigFile -Raw
    Assert-Contains $body 'listen = "127.0.0.1:5037"'
    Assert-Contains $body 'include_local = true'
    Assert-Contains $body 'name = "office"'
    Assert-Contains $body 'addr = "10.0.0.8:5038"'

    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    Write-Host 'Test-WriteTomlConfig: ok' -ForegroundColor Green
    $script:passed++
}

# ===========================================================================
# Test 2: Validation helpers (Test-Host, Test-Port, Test-Name)
# ===========================================================================
function Test-ValidationHelpers {
    # Test-Host
    if (-not (Test-Host '192.168.1.1')) { Fail 'Test-Host should accept valid host' }
    if (Test-Host '')                    { Fail 'Test-Host should reject empty' }
    if (Test-Host '   ')                 { Fail 'Test-Host should reject whitespace' }

    # Test-Port
    if (-not (Test-Port '5038')) { Fail 'Test-Port should accept valid port' }
    if (Test-Port '0')           { Fail 'Test-Port should reject 0' }
    if (Test-Port '65536')       { Fail 'Test-Port should reject >65535' }
    if (Test-Port 'abc')         { Fail 'Test-Port should reject non-numeric' }

    # Test-Name
    if (-not (Test-Name 'office'))  { Fail 'Test-Name should accept valid name' }
    if (Test-Name '')               { Fail 'Test-Name should reject empty' }
    if (Test-Name 'my office')      { Fail 'Test-Name should reject spaces' }
    if (Test-Name 'a=b')            { Fail 'Test-Name should reject =' }

    Write-Host 'Test-ValidationHelpers: ok' -ForegroundColor Green
    $script:passed++
}

# ===========================================================================
# Test 3: Legacy config parsing logic (same regex as Prompt-And-Save)
# ===========================================================================
function Test-LegacyParse {
    $tmp = Join-Path $env:TEMP "adb_test_$(Get-Random)"
    $home_ = Join-Path $tmp 'home'
    New-Item -ItemType Directory -Path $home_ -Force | Out-Null
    $legacyFile = Join-Path $home_ '.adbproxy'
    $cfgDir = Join-Path $tmp 'config'
    New-Item -ItemType Directory -Path $cfgDir -Force | Out-Null

    @"
host=192.168.1.9
port=5038
"@ | Set-Content $legacyFile

    # Simulate the parse + write path from Prompt-And-Save
    $defaultHost = ''; $defaultPort = '5038'
    foreach ($line in Get-Content $legacyFile) {
        if ($line -match '^\s*host\s*=\s*(.+?)\s*$') { $defaultHost = $Matches[1].Trim() }
        elseif ($line -match '^\s*port\s*=\s*(.+?)\s*$') { $defaultPort = $Matches[1].Trim() }
    }

    if ($defaultHost -ne '192.168.1.9') { Fail "legacy host parse failed: '$defaultHost'" }
    if ($defaultPort -ne '5038')         { Fail "legacy port parse failed: '$defaultPort'" }

    $script:ConfigDir  = $cfgDir
    $script:ConfigFile = Join-Path $cfgDir 'config.toml'
    Write-TomlConfig 'remote' $defaultHost $defaultPort

    $body = Get-Content $script:ConfigFile -Raw
    Assert-Contains $body 'name = "remote"'
    Assert-Contains $body 'addr = "192.168.1.9:5038"'

    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    Write-Host 'Test-LegacyParse: ok' -ForegroundColor Green
    $script:passed++
}

# ===========================================================================
# Test 4: Install from mock archive (Download-And-Install with mocked I/O)
# ===========================================================================
function Test-InstallFromMockArchive {
    $tmp    = Join-Path $env:TEMP "adb_test_$(Get-Random)"
    $binDir = Join-Path $tmp 'bin'
    $stage  = Join-Path $tmp 'staging'
    New-Item -ItemType Directory -Path $tmp, $binDir, $stage -Force | Out-Null

    # Create fake executables and archive them
    Set-Content (Join-Path $stage 'adb-hub.exe')   '@hub'
    Set-Content (Join-Path $stage 'adb-proxy.exe')  '@proxy'

    $archive = 'adb-proxy-windows-x86_64.tar.gz'
    $archiveFullPath = Join-Path $tmp $archive
    Push-Location $stage
    try {
        & tar -czf $archiveFullPath adb-hub.exe adb-proxy.exe
        if ($LASTEXITCODE -ne 0) { Fail 'mock archive creation failed' }
    }
    finally { Pop-Location }

    # Override script-scope variables
    $script:InstallDir = $binDir

    # Temporarily replace functions that do network I/O / PATH mutation
    function Fetch-LatestTag { 'v9.9.9' }
    function Ensure-PathHint { }
    function Invoke-WebRequest {
        param($Uri, $OutFile, $UseBasicParsing)
        Copy-Item $archiveFullPath $OutFile
    }

    Download-And-Install

    $hub = Join-Path $binDir 'adb-hub.exe'
    $prx = Join-Path $binDir 'adb-proxy.exe'
    if (-not (Test-Path $hub)) { Fail 'adb-hub.exe not installed' }
    if (-not (Test-Path $prx)) { Fail 'adb-proxy.exe not installed' }
    Assert-Equals '@hub'   (Get-Content $hub -Raw).Trim()
    Assert-Equals '@proxy' (Get-Content $prx -Raw).Trim()

    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    Write-Host 'Test-InstallFromMockArchive: ok' -ForegroundColor Green
    $script:passed++
}

# ===========================================================================
# Run
# ===========================================================================
Test-WriteTomlConfig
Test-ValidationHelpers
Test-LegacyParse
Test-InstallFromMockArchive

Write-Host "`nadb_setup_test.ps1: ok ($passed tests)" -ForegroundColor Green
