#!/usr/bin/env bash
# Configure adb-hub on the client machine (writes TOML config).
# Does not replace the official adb binary.
set -euo pipefail

CONFIG_DIR="${HOME}/.config/adb-hub"
CONFIG_FILE="${CONFIG_DIR}/config.toml"
LEGACY_CONFIG="${HOME}/.adbproxy"
WRAPPER_MARKER="# adb-wrapper"

resolve_executable() {
    local target="${1:-}"
    [[ -z "$target" ]] && return 1
    if [[ -L "$target" ]]; then
        target="$(readlink -f "$target" 2>/dev/null || realpath "$target" 2>/dev/null)"
        [[ -z "$target" ]] && return 1
    fi
    [[ -f "$target" && -x "$target" ]] || return 1
    printf '%s\n' "$target"
}

is_wrapper_adb() {
    local target
    target="$(resolve_executable "${1:-}")" || return 1
    head -n 5 "$target" | grep -qF "$WRAPPER_MARKER"
}

validate_host() {
    local host="$1"
    [[ -n "$host" && ! "$host" =~ ^[[:space:]]*$ ]]
}

validate_port() {
    local port="$1"
    [[ "$port" =~ ^[0-9]+$ ]] || return 1
    (( port >= 1 && port <= 65535 ))
}

validate_name() {
    local name="$1"
    [[ -n "$name" && ! "$name" =~ [[:space:]=] ]]
}

write_toml_config() {
    local name="$1" host="$2" port="$3"
    mkdir -p "$CONFIG_DIR"
    cat > "$CONFIG_FILE" <<EOF
listen = "127.0.0.1:5037"
poll_interval_ms = 1000

[[backend]]
name = "${name}"
addr = "${host}:${port}"
EOF
    echo "Wrote $CONFIG_FILE"
}

prompt_and_save() {
    local name host port
    local default_host="" default_port="5038" default_name="remote"

    if [[ -f "$LEGACY_CONFIG" ]]; then
        while IFS='=' read -r key value; do
            key="${key#"${key%%[![:space:]]*}"}"
            key="${key%"${key##*[![:space:]]}"}"
            value="${value#"${value%%[![:space:]]*}"}"
            value="${value%"${value##*[![:space:]]}"}"
            case "$key" in
                host) default_host="$value" ;;
                port) default_port="$value" ;;
            esac
        done < "$LEGACY_CONFIG"
    fi

    while true; do
        read -rp "Backend name [$default_name]: " name || exit 1
        name="${name:-$default_name}"
        validate_name "$name" && break
        echo "Error: name must be non-empty and must not contain spaces or '='." >&2
    done

    while true; do
        if [[ -n "$default_host" ]]; then
            read -rp "Remote adb-proxy host [$default_host]: " host || exit 1
            host="${host:-$default_host}"
        else
            read -rp "Remote adb-proxy host: " host || exit 1
        fi
        validate_host "$host" && break
        echo "Error: host is required." >&2
        default_host=""
    done

    while true; do
        read -rp "Remote adb-proxy port [$default_port]: " port || exit 1
        port="${port:-$default_port}"
        validate_port "$port" && break
        echo "Error: port must be 1-65535." >&2
    done

    write_toml_config "$name" "$host" "$port"
}

uninstall_old_wrapper() {
    local adb_path
    if ! adb_path="$(command -v adb 2>/dev/null)"; then
        echo "adb not found in PATH; nothing to uninstall."
        return 0
    fi
    local resolved
    if ! resolved="$(resolve_executable "$adb_path")"; then
        echo "Error: cannot resolve adb at $adb_path" >&2
        exit 1
    fi
    if ! is_wrapper_adb "$resolved"; then
        echo "Current adb is not an adb-wrapper; nothing to uninstall."
        return 0
    fi

    local adb_dir wrapped
    adb_dir="$(dirname "$resolved")"
    wrapped="$adb_dir/wrapper/adb"
    if [[ ! -x "$wrapped" ]]; then
        echo "Error: original adb not found at $wrapped" >&2
        exit 1
    fi
    if head -n 5 "$wrapped" | grep -qF "$WRAPPER_MARKER"; then
        echo "Error: $wrapped is also a wrapper; refusing to uninstall." >&2
        exit 1
    fi
    mv "$wrapped" "$resolved"
    rmdir "$adb_dir/wrapper" 2>/dev/null || true
    echo "Restored original adb to $resolved"
}

print_next_steps() {
    cat <<EOF

Next steps:
  1. Install / build adb-hub (cargo build --release --bin adb-hub)
  2. Stop any local adb server:  adb kill-server
  3. Start the hub:              adb-hub --config $CONFIG_FILE
  4. Use the official adb:       adb devices

To remove a legacy PATH wrapper:  $0 --uninstall-wrapper
EOF
}

show_help() {
    cat <<'EOF'
adb-hub setup

Writes ~/.config/adb-hub/config.toml for the client-side adb-hub aggregator.
Does not replace the official adb binary.

Usage:
  adb_setup.sh                 Interactive config writer
  adb_setup.sh --uninstall-wrapper
  adb_setup.sh --help
EOF
}

main() {
    case "${1:-}" in
        --help|-h)
            show_help
            ;;
        --uninstall-wrapper)
            uninstall_old_wrapper
            ;;
        "")
            prompt_and_save
            print_next_steps
            ;;
        *)
            echo "Unknown option: $1" >&2
            show_help >&2
            exit 1
            ;;
    esac
}

main "$@"
