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

test_setup_writes_toml() {
    local tmp home cfg
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    home="$tmp/home"
    mkdir -p "$home"

    printf 'office\n10.0.0.8\n5038\n' |
        HOME="$home" bash "$repo_root/adb_setup.sh"

    cfg="$home/.config/adb-hub/config.toml"
    [[ -f "$cfg" ]] || fail "missing $cfg"
    local body
    body="$(cat "$cfg")"
    assert_contains "$body" 'listen = "127.0.0.1:5037"'
    assert_contains "$body" 'name = "office"'
    assert_contains "$body" 'addr = "10.0.0.8:5038"'
}

test_setup_from_legacy_defaults() {
    local tmp home cfg
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    home="$tmp/home"
    mkdir -p "$home"
    cat > "$home/.adbproxy" <<'EOF'
host=192.168.1.9
port=5038
EOF

    # Accept defaults for name, host, port (empty lines / enter)
    printf '\n\n\n' |
        HOME="$home" bash "$repo_root/adb_setup.sh"

    cfg="$home/.config/adb-hub/config.toml"
    body="$(cat "$cfg")"
    assert_contains "$body" 'name = "remote"'
    assert_contains "$body" 'addr = "192.168.1.9:5038"'
}

test_setup_writes_toml
test_setup_from_legacy_defaults
echo "adb_setup_test.sh: ok"
