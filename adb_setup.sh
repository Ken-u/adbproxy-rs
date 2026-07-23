#!/usr/bin/env bash
# Install latest adb-hub + adb-proxy from GitHub Releases and write client config.
# Does not replace the official adb binary.
set -euo pipefail

REPO="${ADB_PROXY_REPO:-Ken-u/adbproxy-rs}"
INSTALL_DIR="${ADB_PROXY_INSTALL_DIR:-${HOME}/.local/bin}"
CONFIG_DIR="${HOME}/.config/adb-hub"
CONFIG_FILE="${CONFIG_DIR}/config.toml"
LEGACY_CONFIG="${HOME}/.adbproxy"
WRAPPER_MARKER="# adb-wrapper"
API_BASE="https://api.github.com/repos/${REPO}"
RELEASE_BASE="https://github.com/${REPO}/releases/download"

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

# Prints: <archive_basename>  (without .tar.gz path)
detect_archive_name() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) printf '%s\n' "adb-proxy-macos-aarch64.tar.gz" ;;
                x86_64|amd64)  printf '%s\n' "adb-proxy-macos-x86_64.tar.gz" ;;
                *)
                    echo "Error: unsupported macOS arch: $arch" >&2
                    return 1
                    ;;
            esac
            ;;
        Linux)
            case "$arch" in
                x86_64|amd64) printf '%s\n' "adb-proxy-linux-x86_64-musl.tar.gz" ;;
                *)
                    echo "Error: unsupported Linux arch: $arch (need x86_64)" >&2
                    return 1
                    ;;
            esac
            ;;
        MINGW*|MSYS*|CYGWIN*)
            printf '%s\n' "adb-proxy-windows-x86_64.tar.gz"
            ;;
        *)
            echo "Error: unsupported OS: $os" >&2
            return 1
            ;;
    esac
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "Error: '$1' is required but not found in PATH." >&2
        exit 1
    }
}

fetch_latest_tag() {
    need_cmd curl
    local json tag
    json="$(curl -fsSL "${API_BASE}/releases/latest")"
    tag="$(printf '%s\n' "$json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
    if [[ -z "$tag" ]]; then
        echo "Error: could not parse latest release tag from GitHub." >&2
        exit 1
    fi
    printf '%s\n' "$tag"
}

download_and_install() {
    need_cmd curl
    need_cmd tar

    local archive tag url tmp staging
    archive="$(detect_archive_name)"
    tag="$(fetch_latest_tag)"
    url="${RELEASE_BASE}/${tag}/${archive}"

    echo "Installing adb-hub + adb-proxy ${tag}"
    echo "  archive: ${archive}"
    echo "  from:    ${url}"
    echo "  into:    ${INSTALL_DIR}"

    mkdir -p "$INSTALL_DIR"
    tmp="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmp'" RETURN

    curl -fL --progress-bar -o "${tmp}/${archive}" "$url"
    staging="${tmp}/extract"
    mkdir -p "$staging"
    tar -xzf "${tmp}/${archive}" -C "$staging"

    local bin suffix=""
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) suffix=".exe" ;;
    esac

    for bin in adb-hub adb-proxy; do
        if [[ ! -f "${staging}/${bin}${suffix}" ]]; then
            echo "Error: archive missing ${bin}${suffix}" >&2
            ls -la "$staging" >&2 || true
            exit 1
        fi
        install -m 755 "${staging}/${bin}${suffix}" "${INSTALL_DIR}/${bin}${suffix}"
        echo "Installed ${INSTALL_DIR}/${bin}${suffix}"
    done

    ensure_path_hint
}

ensure_path_hint() {
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) return 0 ;;
    esac

    echo
    echo "NOTE: ${INSTALL_DIR} is not in your PATH."
    echo "Add this to your shell profile:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""

    # Only offer to edit rc files when stdin is a TTY (never in curl|bash non-interactive).
    if [[ ! -t 0 ]]; then
        return 0
    fi

    local rc=""
    if [[ -n "${ZSH_VERSION:-}" ]] || [[ "${SHELL:-}" == *zsh ]]; then
        rc="${HOME}/.zshrc"
    elif [[ -n "${BASH_VERSION:-}" ]] || [[ "${SHELL:-}" == *bash ]]; then
        if [[ -f "${HOME}/.bash_profile" ]]; then
            rc="${HOME}/.bash_profile"
        else
            rc="${HOME}/.bashrc"
        fi
    fi
    [[ -n "$rc" ]] || return 0

    local line="export PATH=\"${INSTALL_DIR}:\$PATH\""
    if [[ -f "$rc" ]] && grep -Fqx "$line" "$rc" 2>/dev/null; then
        echo "PATH export already present in $rc (open a new shell)."
        return 0
    fi

    read -rp "Append PATH to $rc? [Y/n] " ans || true
    ans="${ans:-Y}"
    case "$ans" in
        n|N|no|NO)
            ;;
        *)
            printf '\n# adb-proxy\n%s\n' "$line" >> "$rc"
            echo "Appended to $rc — run:  source $rc"
            ;;
    esac
}

write_toml_config() {
    local name="$1" host="$2" port="$3"
    mkdir -p "$CONFIG_DIR"
    cat > "$CONFIG_FILE" <<EOF
listen = "127.0.0.1:5037"
poll_interval_ms = 1000
include_local = true
local_adb_port = 5039

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
    local hub="${INSTALL_DIR}/adb-hub"
    local proxy="${INSTALL_DIR}/adb-proxy"
    cat <<EOF

Done.

Client (this machine):
  adb kill-server
  ${hub} --config ${CONFIG_FILE}
  adb devices

Device host (USB machine):
  adb start-server
  ${proxy} --listen 0.0.0.0:5038 --target 127.0.0.1:5037

Re-run install only:   $0 --install
Config only:           $0 --config
Remove old wrapper:    $0 --uninstall-wrapper
EOF
}

show_help() {
    cat <<EOF
adb-proxy / adb-hub setup

Downloads the latest GitHub release for this OS/arch, installs adb-hub and
adb-proxy into ${INSTALL_DIR}, and optionally writes client config.

Usage:
  adb_setup.sh                 Download+install, then interactive config
  adb_setup.sh --install       Download+install only
  adb_setup.sh --config        Interactive config only
  adb_setup.sh --uninstall-wrapper
  adb_setup.sh --help

Environment:
  ADB_PROXY_INSTALL_DIR   Install directory (default: ~/.local/bin)
  ADB_PROXY_REPO          GitHub repo (default: Ken-u/adbproxy-rs)
  ADB_SETUP_SKIP_DOWNLOAD=1   Skip download (tests / offline config)
EOF
}

run_default() {
    if [[ "${ADB_SETUP_SKIP_DOWNLOAD:-}" != "1" ]]; then
        download_and_install
        echo
    fi
    prompt_and_save
    print_next_steps
}

main() {
    case "${1:-}" in
        --help|-h)
            show_help
            ;;
        --install)
            download_and_install
            ;;
        --config)
            prompt_and_save
            print_next_steps
            ;;
        --uninstall-wrapper)
            uninstall_old_wrapper
            ;;
        "")
            run_default
            ;;
        *)
            echo "Unknown option: $1" >&2
            show_help >&2
            exit 1
            ;;
    esac
}

main "$@"
