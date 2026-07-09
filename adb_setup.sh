#!/usr/bin/env bash
set -euo pipefail

CONFIG_FILE="${HOME}/.adbproxy"
WRAPPER_MARKER="# adb-wrapper"

# Resolve a path to the actual executable it points to, following symlinks.
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

# Validate host as a plausible IP address or hostname.
validate_host() {
    local host="$1"
    [[ -z "$host" ]] && return 1
    # Allow simple hostnames, IPv4, or bracketed/unbracketed IPv6.
    if [[ "$host" =~ ^[[:space:]]*$ ]]; then
        return 1
    fi
    return 0
}

# Validate port as a number in the valid TCP range.
validate_port() {
    local port="$1"
    [[ "$port" =~ ^[0-9]+$ ]] || return 1
    (( port >= 1 && port <= 65535 )) || return 1
}

validate_sn() {
    local sn="$1"
    [[ "$sn" =~ [[:space:]] ]] && return 1
    return 0
}

# Prompt for host/port/SN and save to config. Accepts a default host (optional).
prompt_and_save_config() {
    local default_host="${1:-}"
    local host port sn

    while true; do
        if [[ -n "$default_host" ]]; then
            read -rp "Remote adb proxy IP [$default_host]: " host
            host="${host:-$default_host}"
        else
            read -rp "Remote adb proxy IP: " host || {
                echo "Error: no input received." >&2
                exit 1
            }
        fi
        validate_host "$host" && break
        echo "Error: IP/hostname is required." >&2
        default_host=""
    done

    while true; do
        read -rp "Remote adb proxy port [5038]: " port || {
            echo "Error: no input received." >&2
            exit 1
        }
        port="${port:-5038}"
        validate_port "$port" && break
        echo "Error: port must be a number between 1 and 65535." >&2
    done

    while true; do
        read -rp "Default adb device SN (optional): " sn || {
            echo "Error: no input received." >&2
            exit 1
        }
        validate_sn "$sn" && break
        echo "Error: SN must not contain whitespace." >&2
    done

    save_config_file "$host" "$port" "$sn"
}

save_config_file() {
    local host="$1" port="$2" sn="${3:-}"
    cat > "$CONFIG_FILE" <<EOF
host=$host
port=$port
EOF
    if [[ -n "$sn" ]]; then
        printf 'sn=%s\n' "$sn" >> "$CONFIG_FILE"
    fi
    echo "Saved remote adb proxy config to $CONFIG_FILE"
}

# Find the original adb executable. Prefer a real file over aliases/functions,
# and resolve symlinks so we always operate on the actual binary.
find_adb() {
    local adb_path
    if ! adb_path="$(command -v adb 2>/dev/null)"; then
        echo "Error: adb not found in PATH." >&2
        exit 1
    fi

    local resolved
    if ! resolved="$(resolve_executable "$adb_path")"; then
        echo "Error: adb path '$adb_path' is not an executable file." >&2
        exit 1
    fi
    printf '%s\n' "$resolved"
}

install_wrapper() {
    local adb_path
    adb_path="$(find_adb)"

    if is_wrapper_adb "$adb_path"; then
        echo "The current adb is already the wrapper script." >&2
        return 0
    fi

    local adb_dir
    adb_dir="$(dirname "$adb_path")"

    local wrapper_dir="$adb_dir/wrapper"
    local wrapped_adb="$wrapper_dir/adb"

    mkdir -p "$wrapper_dir"

    # Stage the wrapper script to a temp file first so we can atomically swap
    # it in. If anything fails, restore the original adb.
    local tmp_wrapper
    tmp_wrapper="$(mktemp "$adb_dir/.adb-wrapper.XXXXXX")"
    trap 'rm -f "$tmp_wrapper"' EXIT

    cat > "$tmp_wrapper" <<'EOF'
#!/usr/bin/env bash
# adb-wrapper
set -euo pipefail

CONFIG_FILE="${HOME}/.adbproxy"

self_dir() {
    local self="${BASH_SOURCE[0]}"
    if [[ ! "$self" =~ / ]]; then
        self="$(command -v "$self")"
    elif [[ "$self" != /* ]]; then
        self="$PWD/$self"
    fi
    cd "$(dirname "$self")" && pwd
}

validate_sn() {
    local sn="$1"
    [[ "$sn" =~ [[:space:]] ]] && return 1
    return 0
}

save_config() {
    local host="$1" port="$2" sn="${3:-}"
    cat > "$CONFIG_FILE" <<INNEREOF
host=$host
port=$port
INNEREOF
    if [[ -n "$sn" ]]; then
        printf 'sn=%s\n' "$sn" >> "$CONFIG_FILE"
    fi
    echo "Saved remote adb proxy config to $CONFIG_FILE"
}

prompt_and_save_config() {
    local host port sn

    while true; do
        read -rp "Remote adb proxy IP: " host || {
            echo "Error: no input received." >&2
            exit 1
        }
        if [[ -n "$host" && ! "$host" =~ ^[[:space:]]*$ ]]; then
            break
        fi
        echo "Error: IP/hostname is required." >&2
    done

    while true; do
        read -rp "Remote adb proxy port [5038]: " port || {
            echo "Error: no input received." >&2
            exit 1
        }
        port="${port:-5038}"
        if [[ "$port" =~ ^[0-9]+$ ]] && (( port >= 1 && port <= 65535 )); then
            break
        fi
        echo "Error: port must be a number between 1 and 65535." >&2
    done

    while true; do
        read -rp "Default adb device SN (optional): " sn || {
            echo "Error: no input received." >&2
            exit 1
        }
        validate_sn "$sn" && break
        echo "Error: SN must not contain whitespace." >&2
    done

    save_config "$host" "$port" "$sn"
}

run_setup() {
    prompt_and_save_config
}

run_local() {
    local script_dir real_adb
    script_dir="$(self_dir)"
    real_adb="$script_dir/wrapper/adb"

    if [[ ! -x "$real_adb" ]]; then
        echo "Error: original adb not found at $real_adb" >&2
        exit 1
    fi

    save_config "127.0.0.1" "5037"
    # Clear env vars that would override -H/-P for this invocation.
    unset ADB_SERVER_SOCKET ANDROID_ADB_SERVER_ADDRESS ANDROID_ADB_SERVER_PORT 2>/dev/null || true
    exec "$real_adb" start-server
}

run_uninstall() {
    local script_dir real_adb wrapper_dir wrapper_script
    script_dir="$(self_dir)"
    real_adb="$script_dir/wrapper/adb"
    wrapper_script="$script_dir/adb"

    if [[ ! -x "$real_adb" ]]; then
        echo "Error: original adb not found at $real_adb" >&2
        exit 1
    fi

    if head -n 5 "$real_adb" | grep -qF "# adb-wrapper"; then
        echo "Error: the file at $real_adb is also a wrapper; cannot uninstall." >&2
        exit 1
    fi

    wrapper_dir="$(dirname "$real_adb")"
    rm -f "$wrapper_dir"/adb.bak.*
    mv "$real_adb" "$wrapper_script"
    rmdir "$wrapper_dir" 2>/dev/null || true
    echo "Restored original adb to $wrapper_script"
}

show_help() {
    cat <<'INNEREOF'
adb proxy wrapper

This wrapper forwards normal adb commands to the configured remote adb-proxy.
Use one of the following meta-commands instead of a normal adb subcommand:

  --adbproxy-setup      Reconfigure the remote adb-proxy host and port.
                        You can also bind a default device SN for adb commands.
  --adbproxy-local      Switch to local adb server (127.0.0.1:5037) and run
                        'adb start-server' once.
  --adbproxy-uninstall  Restore the original adb executable and remove this
                        wrapper.
  --adbproxy-help       Show this help message.

Configuration is read from: ~/.adbproxy
INNEREOF
}

if [[ "${1:-}" == "--adbproxy-help" ]]; then
    show_help
    exit 0
fi

if [[ "${1:-}" == "--adbproxy-setup" ]]; then
    run_setup
    exit 0
fi

if [[ "${1:-}" == "--adbproxy-local" ]]; then
    run_local
    exit 0
fi

if [[ "$#" -eq 1 && "${1:-}" == "--adbproxy-uninstall" ]]; then
    run_uninstall
    exit 0
fi

if [[ ! -f "$CONFIG_FILE" ]]; then
    echo "Error: $CONFIG_FILE not found. Run 'adb --adbproxy-setup' to configure." >&2
    exit 1
fi

has_explicit_serial() {
    local arg
    for arg in "$@"; do
        case "$arg" in
            -s|--serial|--serial=*|-s?*) return 0 ;;
        esac
    done
    return 1
}

host=""
port=""
sn=""
while IFS='=' read -r key value; do
    [[ -z "$key" ]] && continue

    # Strip leading/trailing whitespace from key and value using Bash only.
    key="${key#"${key%%[![:space:]]*}"}"
    key="${key%"${key##*[![:space:]]}"}"

    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"

    [[ -z "$key" || "${key:0:1}" == "#" ]] && continue

    case "$key" in
        host) host="$value" ;;
        port) port="$value" ;;
        sn) sn="$value" ;;
    esac
done < "$CONFIG_FILE"

if [[ -z "$host" || -z "$port" ]]; then
    echo "Error: host and port must be set in $CONFIG_FILE" >&2
    exit 1
fi

script_dir="$(self_dir)"
real_adb="$script_dir/wrapper/adb"

if [[ ! -x "$real_adb" ]]; then
    echo "Error: original adb not found at $real_adb" >&2
    exit 1
fi

if head -n 5 "$real_adb" | grep -qF "# adb-wrapper"; then
    echo "Error: the file at $real_adb is also a wrapper; possible install loop." >&2
    exit 1
fi

# Clear env vars that would override the wrapper's -H/-P.
unset ADB_SERVER_SOCKET ANDROID_ADB_SERVER_ADDRESS ANDROID_ADB_SERVER_PORT 2>/dev/null || true
if [[ -n "$sn" ]] && ! has_explicit_serial "$@"; then
    exec "$real_adb" -H "$host" -P "$port" -s "$sn" "$@"
fi
exec "$real_adb" -H "$host" -P "$port" "$@"
EOF

    chmod +x "$tmp_wrapper"

    # If a previous wrapped adb exists, back it up before overwriting it with
    # the newly resolved original.
    if [[ -e "$wrapped_adb" ]]; then
        local backup="$wrapped_adb.bak.$(date +%s)"
        mv "$wrapped_adb" "$backup"
        echo "Backed up existing $wrapped_adb to $backup"
    fi

    mv "$adb_path" "$wrapped_adb"
    mv "$tmp_wrapper" "$adb_path"
    trap - EXIT
    echo "Installed wrapper adb to $adb_path"
}

main() {
    if [[ -f "$CONFIG_FILE" ]]; then
        prompt_and_save_config ""
    fi
    install_wrapper
}

main "$@"
