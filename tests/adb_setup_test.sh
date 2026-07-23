#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

assert_contains() {
    local haystack="$1" needle="$2"
    [[ "$haystack" == *"$needle"* ]] || fail "expected to contain '$needle', got: $haystack"
}

assert_eq() {
    local expected="$1" actual="$2"
    [[ "$expected" == "$actual" ]] || fail "expected '$expected', got '$actual'"
}

expected_archive_for_host() {
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64|Darwin-aarch64) printf '%s\n' "adb-proxy-macos-aarch64.tar.gz" ;;
        Darwin-x86_64) printf '%s\n' "adb-proxy-macos-x86_64.tar.gz" ;;
        Linux-x86_64|Linux-amd64) printf '%s\n' "adb-proxy-linux-x86_64-musl.tar.gz" ;;
        *) fail "unsupported host for test: $(uname -s)-$(uname -m)" ;;
    esac
}

test_setup_writes_toml() {
    local tmp home cfg body
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    home="$tmp/home"
    mkdir -p "$home"

    printf 'office\n10.0.0.8\n5038\n' |
        HOME="$home" ADB_SETUP_SKIP_DOWNLOAD=1 bash "$repo_root/adb_setup.sh"

    cfg="$home/.config/adb-hub/config.toml"
    [[ -f "$cfg" ]] || fail "missing $cfg"
    body="$(cat "$cfg")"
    assert_contains "$body" 'listen = "127.0.0.1:5037"'
    assert_contains "$body" 'include_local = true'
    assert_contains "$body" 'name = "office"'
    assert_contains "$body" 'addr = "10.0.0.8:5038"'
}

test_setup_from_legacy_defaults() {
    local tmp home cfg body
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    home="$tmp/home"
    mkdir -p "$home"
    cat > "$home/.adbproxy" <<'EOF'
host=192.168.1.9
port=5038
EOF

    printf '\n\n\n' |
        HOME="$home" ADB_SETUP_SKIP_DOWNLOAD=1 bash "$repo_root/adb_setup.sh" --config

    cfg="$home/.config/adb-hub/config.toml"
    body="$(cat "$cfg")"
    assert_contains "$body" 'name = "remote"'
    assert_contains "$body" 'addr = "192.168.1.9:5038"'
}

test_install_from_mock_archive() {
    local tmp home install archive
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    home="$tmp/home"
    install="$tmp/bin"
    mkdir -p "$home" "$install" "$tmp/staging" "$tmp/path"

    printf '#!/bin/sh\necho hub\n' > "$tmp/staging/adb-hub"
    printf '#!/bin/sh\necho proxy\n' > "$tmp/staging/adb-proxy"
    chmod +x "$tmp/staging/adb-hub" "$tmp/staging/adb-proxy"

    archive="$(expected_archive_for_host)"
    tar -C "$tmp/staging" -czf "$tmp/$archive" adb-hub adb-proxy

    cat > "$tmp/path/curl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
out=""
url=""
args=("\$@")
i=0
while [[ \$i -lt \${#args[@]} ]]; do
  case "\${args[\$i]}" in
    -o)
      i=\$((i+1))
      out="\${args[\$i]}"
      ;;
    -o*)
      out="\${args[\$i]#-o}"
      ;;
    http*|file*)
      url="\${args[\$i]}"
      ;;
  esac
  i=\$((i+1))
done
if [[ "\$url" == *"/releases/latest"* ]]; then
  printf '%s\n' '{"tag_name":"v9.9.9"}'
  exit 0
fi
if [[ -n "\$out" ]]; then
  cp "$tmp/$archive" "\$out"
  exit 0
fi
echo "unexpected curl: \$*" >&2
exit 1
EOF
    chmod +x "$tmp/path/curl"

    # Keep install dir on PATH so ensure_path_hint stays quiet / non-interactive.
    PATH="$install:$tmp/path:/usr/bin:/bin" \
    HOME="$home" \
    ADB_PROXY_INSTALL_DIR="$install" \
    bash "$repo_root/adb_setup.sh" --install

    [[ -x "$install/adb-hub" ]] || fail "adb-hub not installed"
    [[ -x "$install/adb-proxy" ]] || fail "adb-proxy not installed"
    assert_eq "hub" "$("$install/adb-hub")"
    assert_eq "proxy" "$("$install/adb-proxy")"
}

test_setup_writes_toml
test_setup_from_legacy_defaults
test_install_from_mock_archive
echo "adb_setup_test.sh: ok"
