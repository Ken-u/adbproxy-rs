# adb-proxy / adb-hub

This repository provides two binaries:

1. **`adb-proxy`** — TCP proxy on the machine that has USB devices (pair-code auth required)
2. **`adb-hub`** — local ADB-protocol server on the client machine (`127.0.0.1:5037`) that aggregates multiple remote `adb-proxy` backends

```text
Client PC                         Device hosts
---------                         ------------
official adb
    |
    v
adb-hub :5037  ----TCP---->  adb-proxy :5038  -->  adb server :5037  --> USB
               ----TCP---->  adb-proxy :5038  -->  adb server :5037  --> USB
```

`adb-hub` speaks just enough of the ADB host protocol to merge device lists. Other host services (`features`, `version`, `tport`, `transport`, …) are forwarded as-is to the owning backend (or the default/local adb server).

Remote `adb-proxy` instances require an 8-character pair code (`A-Z0-9`) on every connection. The port may be open on the LAN, but ADB traffic is rejected until the hub authenticates with the matching code.

## Device host (USB machine)

```bash
adb start-server
adb devices
# pair code is printed at startup (or set ADB_PROXY_PAIR_CODE / --pair-code)
cargo run --bin adb-proxy -- --listen 0.0.0.0:5038 --target 127.0.0.1:5037
```

Or with a release binary:

```bash
adb-proxy --listen 0.0.0.0:5038 --target 127.0.0.1:5037
# optional fixed code for reconnects:
# ADB_PROXY_PAIR_CODE=ABCD1234 adb-proxy ...
```

## Client (your laptop)

Stop any local adb server that already owns `:5037`, then pair and start the hub:

```bash
adb kill-server

# Pair with a remote proxy (writes ~/.config/adb-hub/config.toml)
adb-hub pair 192.168.1.10:5038 ABCD1234 --name office
adb-hub pair 192.168.1.11:5038 EFGH5678 --name lab

# Start hub (loads paired backends from config)
adb-hub
```

You can still pass backends on the CLI (without a pair code they cannot talk to an auth-gated proxy):

```bash
adb-hub --backend office=192.168.1.10:5038
adb-hub --config ~/.config/adb-hub/config.toml
```

Example config after pairing:

```toml
listen = "127.0.0.1:5037"
poll_interval_ms = 1000
include_local = true
local_adb_port = 5039

[[backend]]
name = "office"
addr = "192.168.1.10:5038"
pair_code = "ABCD1234"

[[backend]]
name = "lab"
addr = "192.168.1.11:5038"
pair_code = "EFGH5678"
```

By default `adb-hub` also starts a real local `adb` server on `local_adb_port` (5039), frees `:5037` for itself, and aggregates local USB devices as backend `local` together with remotes. Use `--no-local` to disable. The local backend does not use pair-code auth.

Then use the original `adb` as usual (no `-H` / `-P`, no PATH wrapper):

```bash
adb devices
adb -s <serial> shell
adb install app.apk
```

If two backends expose the same serial, the hub rewrites them to `name:serial` (for example `office:ABC123`).

Legacy `~/.adbproxy` (`host=` / `port=`) is still loaded when the TOML config is missing.

## Setup helper

[`adb_setup.sh`](adb_setup.sh) (Linux / macOS) and [`adb_setup.ps1`](adb_setup.ps1) (Windows) download the latest GitHub Release for your OS/arch, install `adb-hub` and `adb-proxy` into `~/.local/bin`, then write the client TOML config. They do not replace the official `adb` binary.

### Linux / macOS

```bash
curl -fsSL https://raw.githubusercontent.com/Ken-u/adbproxy-rs/main/adb_setup.sh | bash
```

Useful flags:

```bash
bash adb_setup.sh --install              # download + install only
bash adb_setup.sh --config               # config only
bash adb_setup.sh --uninstall-wrapper    # remove legacy PATH wrapper
```

Install directory override: `ADB_PROXY_INSTALL_DIR=~/bin`.

### Windows (PowerShell)

```powershell
# Run from a PowerShell terminal (Network access required for the one-time download)
.\adb_setup.ps1
```

Or inline:

```powershell
irm https://raw.githubusercontent.com/Ken-u/adbproxy-rs/main/adb_setup.ps1 | iex
```

Useful flags:

```powershell
.\adb_setup.ps1 -Install             # download + install only
.\adb_setup.ps1 -Config              # interactive config only
.\adb_setup.ps1 -UninstallWrapper    # remove legacy PATH wrapper
```

Install directory override: `$env:ADB_PROXY_INSTALL_DIR = "$HOME\bin"`.

> **Note** — Windows uses `tar` (bundled since Windows 10 1803) to extract the archive, and writes config to `%USERPROFILE%\.config\adb-hub\config.toml`. If the install directory is not on `PATH`, the script offers to append it to the user-level `PATH`.

## Binary usage

### adb-proxy (device host)

```bash
adb-proxy \
  --listen 0.0.0.0:5038 \
  --target 127.0.0.1:5037 \
  --pair-code ABCD1234 \
  --log-level info
```

Env: `ADB_PROXY_LISTEN`, `ADB_PROXY_TARGET`, `ADB_PROXY_PAIR_CODE`, `ADB_PROXY_LOG`.

If `--pair-code` / `ADB_PROXY_PAIR_CODE` is omitted, a random 8-character `A-Z0-9` code is generated and logged at startup.

### adb-hub (client)

```bash
# Pair once
adb-hub pair 192.168.1.10:5038 ABCD1234 --name office

# Run hub
adb-hub \
  --listen 127.0.0.1:5037 \
  --log-level info
```

Env: `ADB_HUB_LISTEN`, `ADB_HUB_CONFIG`, `ADB_HUB_POLL_MS`, `ADB_HUB_LOG`.

## Release Artifacts

GitHub Actions builds downloadable archives:

- `adb-proxy-linux-x86_64-musl.tar.gz`
- `adb-proxy-macos-aarch64.tar.gz`
- `adb-proxy-macos-x86_64.tar.gz`
- `adb-proxy-windows-x86_64.tar.gz`

Each archive includes both `adb-proxy` and `adb-hub` (or `adb-hub.exe` on Windows).

## Automatic Releases

Releases are driven by `Cargo.toml` on pushes to `main`:

1. Update `package.version`, for example `0.2.4`.
2. Push to `main` (do **not** push a `v*` tag yourself).
3. GitHub Actions builds all platforms.
4. If release `v0.2.4` does not already exist, the workflow creates tag `v0.2.4`, creates a GitHub Release, and uploads the archives.

Tag pushes no longer trigger CI (avoids duplicate runs when the workflow creates the tag).

## Development

```bash
cargo test
cargo build --bins
cargo run --bin adb-proxy -- --listen 0.0.0.0:5038 --target 127.0.0.1:5037 --pair-code ABCD1234
cargo run --bin adb-hub -- pair 127.0.0.1:5038 ABCD1234 --name mock
cargo run --bin adb-hub -- --no-local
```

## Scope

Implemented:

- TCP `adb-proxy` with pair-code auth on every connection (device host)
- Protocol-aware `adb-hub` on `:5037` (client)
- `adb-hub pair <host:port> <code> [--name]` persists backends + pair codes
- Auto-starts local `adb` on a side port and aggregates USB devices as `local`
- Multi-backend device list merge + serial conflict rewrite
- Opaque forward of non-list host services (`features`, `tport`, `transport`, …) to the owning/default backend
- `host:version`, `host:devices` / `devices-l`, `host:track-devices`, `host:transport:*`, `host:transport-any`, `host:kill`, `host-serial:*` forwarding
- TOML config + legacy `~/.adbproxy` + CLI `--backend`

Not in this phase:

- LAN auto-discovery
- Sharing `:5037` with a separately started default adb server (hub relocates local adb to `local_adb_port`)
