#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

assert_file_equals() {
    local expected="$1" actual_file="$2"
    local actual
    actual="$(cat "$actual_file")"
    [[ "$actual" == "$expected" ]] || fail "expected '$expected', got '$actual'"
}

test_bound_sn_is_used_by_default() {
    local tmp bin home args
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    bin="$tmp/bin"
    home="$tmp/home"
    args="$tmp/args"
    mkdir -p "$bin" "$home"

    cat > "$bin/adb" <<'FAKEADB'
#!/usr/bin/env bash
printf '%s\n' "$*" > "$ADB_TEST_ARGS_FILE"
FAKEADB
    chmod +x "$bin/adb"

    PATH="$bin:$PATH" HOME="$home" ADB_TEST_ARGS_FILE="$args" bash "$repo_root/adb_setup.sh"

    printf '10.0.0.8\n5038\nDEVICE123\n' |
        PATH="$bin:$PATH" HOME="$home" ADB_TEST_ARGS_FILE="$args" "$bin/adb" --adbproxy-setup

    assert_file_equals $'host=10.0.0.8\nport=5038\nsn=DEVICE123' "$home/.adbproxy"

    PATH="$bin:$PATH" HOME="$home" ADB_TEST_ARGS_FILE="$args" "$bin/adb" shell id
    assert_file_equals '-H 10.0.0.8 -P 5038 -s DEVICE123 shell id' "$args"

    PATH="$bin:$PATH" HOME="$home" ADB_TEST_ARGS_FILE="$args" "$bin/adb" -s OTHER shell id
    assert_file_equals '-H 10.0.0.8 -P 5038 -s OTHER shell id' "$args"
}

test_bound_sn_is_used_by_default
