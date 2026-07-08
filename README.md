# adb-proxy

`adb-proxy` is a transparent TCP proxy for exposing a local adb server to remote adb clients.

The first phase intentionally does not parse the adb protocol. It accepts TCP connections from official adb clients and forwards bytes to a local adb server.

```text
adb client
    |
    | TCP
    v
adb-proxy 0.0.0.0:5038
    |
    | TCP
    v
adb server 127.0.0.1:5037
    |
    v
USB Android device
```

## Quick Start

On the machine that has the USB Android device:

```bash
adb start-server
adb devices
cargo run -- --listen 0.0.0.0:5038 --target 127.0.0.1:5037
```

On a remote client machine:

```bash
adb -H <server_ip> -P 5038 devices
adb -H <server_ip> -P 5038 shell
adb -H <server_ip> -P 5038 push local.txt /data/local/tmp/
adb -H <server_ip> -P 5038 pull /sdcard/file.txt .
adb -H <server_ip> -P 5038 install app.apk
adb -H <server_ip> -P 5038 logcat
adb -H <server_ip> -P 5038 reboot
```

For convenience:

```bash
alias radb='adb -H <server_ip> -P 5038'
radb devices
radb shell
```

## Binary Usage

```bash
adb-proxy \
  --listen 0.0.0.0:5038 \
  --target 127.0.0.1:5037 \
  --log-level info
```

Environment variables are also supported:

```bash
ADB_PROXY_LISTEN=0.0.0.0:5038 \
ADB_PROXY_TARGET=127.0.0.1:5037 \
ADB_PROXY_LOG=debug \
adb-proxy
```

## Release Artifacts

GitHub Actions builds downloadable archives:

- `adb-proxy-linux-x86_64-musl.tar.gz`
- `adb-proxy-macos-aarch64.tar.gz`
- `adb-proxy-macos-x86_64.tar.gz`
- `adb-proxy-windows-x86_64.tar.gz`

The Linux artifact targets `x86_64-unknown-linux-musl` so it can run on common Linux hosts without depending on the host glibc.

The Windows artifact contains `adb-proxy.exe` built for `x86_64-pc-windows-msvc`.

## Automatic Releases

Releases are driven by `Cargo.toml`:

1. Update `package.version`, for example `0.1.1`.
2. Push to `main`.
3. GitHub Actions builds all platforms.
4. If release `v0.1.1` does not already exist, the workflow creates tag `v0.1.1`, creates a GitHub Release, and uploads the archives.

Pushing an explicit `v*` tag also builds and publishes that tag.

## Development

```bash
cargo test
cargo build
cargo run -- --listen 0.0.0.0:5038 --target 127.0.0.1:5037
```

## Current Scope

Implemented:

- Transparent TCP proxy
- Tokio async runtime
- One upstream adb server connection per client
- Bidirectional async forwarding
- Multi-client support
- Connection logs with byte counts and duration
- Graceful shutdown hook in the library API

Planned later:

- Config file and hot reload
- Pluggable authentication trait
- Device manager structures
- Session tracking model
- Multi-target routing
