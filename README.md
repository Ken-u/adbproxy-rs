# adb-proxy / adb-hub

This repository provides two binaries:

1. **`adb-proxy`** — transparent TCP proxy on the machine that has USB devices
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

`adb-hub` speaks the ADB host protocol: it answers `host:devices` / `track-devices` from a merged registry, and on `host:transport:SERIAL` opens a connection to the owning backend and byte-pipes the rest (shell / sync / install / …).

## Device host (USB machine)

```bash
adb start-server
adb devices
cargo run --bin adb-proxy -- --listen 0.0.0.0:5038 --target 127.0.0.1:5037
```

Or with a release binary:

```bash
adb-proxy --listen 0.0.0.0:5038 --target 127.0.0.1:5037
```

## Client (your laptop)

Stop any local adb server that already owns `:5037`, then start the hub:

```bash
adb kill-server

# CLI backends
adb-hub --backend office=192.168.1.10:5038 --backend lab=192.168.1.11:5038

# or TOML config
adb-hub --config ~/.config/adb-hub/config.toml
```

Example config:

```toml
listen = "127.0.0.1:5037"
poll_interval_ms = 1000
include_local = true
local_adb_port = 5039

[[backend]]
name = "office"
addr = "192.168.1.10:5038"

[[backend]]
name = "lab"
addr = "192.168.1.11:5038"
```

By default `adb-hub` also starts a real local `adb` server on `local_adb_port` (5039), frees `:5037` for itself, and aggregates local USB devices as backend `local` together with remotes. Use `--no-local` to disable.

Then use the original `adb` as usual (no `-H` / `-P`, no PATH wrapper):

```bash
adb devices
adb -s <serial> shell
adb install app.apk
```

If two backends expose the same serial, the hub rewrites them to `name:serial` (for example `office:ABC123`).

Legacy `~/.adbproxy` (`host=` / `port=`) is still loaded when the TOML config is missing.

## Setup helper

[`adb_setup.sh`](adb_setup.sh) downloads the latest GitHub Release for your OS/arch, installs `adb-hub` and `adb-proxy` into `~/.local/bin`, then writes the client TOML config. It does not replace the official `adb` binary.

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

## Binary usage

### adb-proxy (device host)

```bash
adb-proxy \
  --listen 0.0.0.0:5038 \
  --target 127.0.0.1:5037 \
  --log-level info
```

Env: `ADB_PROXY_LISTEN`, `ADB_PROXY_TARGET`, `ADB_PROXY_LOG`.

### adb-hub (client)

```bash
adb-hub \
  --listen 127.0.0.1:5037 \
  --backend office=192.168.1.10:5038 \
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

Releases are driven by `Cargo.toml`:

1. Update `package.version`, for example `0.2.1`.
2. Push to `main`.
3. GitHub Actions builds all platforms.
4. If release `v0.2.1` does not already exist, the workflow creates tag `v0.2.1`, creates a GitHub Release, and uploads the archives.

## Development

```bash
cargo test
cargo build --bins
cargo run --bin adb-proxy -- --listen 0.0.0.0:5038 --target 127.0.0.1:5037
cargo run --bin adb-hub -- --backend mock=127.0.0.1:5038
```

## Scope

Implemented:

- Transparent TCP `adb-proxy` (device host)
- Protocol-aware `adb-hub` on `:5037` (client)
- Auto-starts local `adb` on a side port and aggregates USB devices as `local`
- Multi-backend device list merge + serial conflict rewrite
- `host:version`, `host:devices` / `devices-l`, `host:track-devices`, `host:transport:*`, `host:transport-any`, `host:kill`, `host-serial:*` forwarding
- TOML config + legacy `~/.adbproxy` + CLI `--backend`

Not in this phase:

- LAN auto-discovery / `adb pair`
- Auth / ACL
- Sharing `:5037` with a separately started default adb server (hub relocates local adb to `local_adb_port`)
