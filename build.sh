#!/bin/bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELP_PATH="$REPO_ROOT/build-help.txt"

configure_cargo_build_cache() {
    local cache_root="" cache_source=""

    if [ -n "${CARGO_TARGET_DIR:-}" ]; then
        case "$CARGO_TARGET_DIR" in
            /*) ;;
            *) CARGO_TARGET_DIR="$REPO_ROOT/$CARGO_TARGET_DIR" ;;
        esac
        cache_source="CARGO_TARGET_DIR override"
    else
        if [ -n "${RBE_BUILD_CACHE_DIR:-}" ]; then
            cache_root="$RBE_BUILD_CACHE_DIR"
            cache_source="RBE_BUILD_CACHE_DIR"
        elif [ -n "${XDG_CACHE_HOME:-}" ]; then
            cache_root="$XDG_CACHE_HOME/rbe-build"
            cache_source="XDG_CACHE_HOME"
        else
            cache_root="$REPO_ROOT/.cache/rbe-build"
            cache_source="RBE local cache"
        fi

        case "$cache_root" in
            /*) ;;
            *) cache_root="$REPO_ROOT/$cache_root" ;;
        esac
        export RBE_BUILD_CACHE_DIR="$cache_root"
        CARGO_TARGET_DIR="$cache_root/cargo-target"
    fi

    mkdir -p "$CARGO_TARGET_DIR"
    export CARGO_TARGET_DIR

    echo "RBE shared Cargo artifact cache:" >&2
    echo "  target-dir: $CARGO_TARGET_DIR" >&2
    echo "  source: $cache_source" >&2
    if [ -n "${RENDER:-}" ] && [ -n "${XDG_CACHE_HOME:-}" ] && [[ "$CARGO_TARGET_DIR" == "$XDG_CACHE_HOME/"* ]]; then
        echo "  persistence: Render build cache" >&2
    fi
}

normalize_only_component() {
    local value
    value="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
    case "$value" in
        backend) echo backend ;;
        service) echo service ;;
        cloud-node|cloud_node) echo cloud-node ;;
        container|container-bin|container_bin) echo container ;;
        rpx) echo rpx ;;
        sdk-backend|sdk_backend) echo sdk-backend ;;
        *)
            echo "ERROR: unknown --only component '$1'. Run ./build.sh -h for the supported component list." >&2
            return 2
            ;;
    esac
}

linux_build_tool_error() {
    echo "ERROR: $1" >&2
    echo "Debian/Ubuntu build hosts normally need: build-essential git openssl rustup." >&2
    echo "Install the missing host tool, then rerun ./build.sh --check-tools --build-linux --arch-x64." >&2
    exit 1
}

rust_toolchain_healthy() {
    local toolchain="$1"
    RUSTUP_TOOLCHAIN="$toolchain" rustc --version >/dev/null 2>&1 \
        && RUSTUP_TOOLCHAIN="$toolchain" cargo --version >/dev/null 2>&1 \
        && RUSTUP_TOOLCHAIN="$toolchain" rustup target list --installed >/dev/null 2>&1
}

repair_rust_toolchain() {
    local toolchain="$1"
    local rustup_home toolchain_name=""

    echo "WARNING: Rust toolchain '$toolchain' is installed or selected but cannot execute correctly." >&2
    echo "         This usually means the cached rustup toolchain is incomplete/corrupt." >&2

    case "$(printf '%s' "${RBE_BUILD_AUTO_REPAIR_RUST:-1}" | tr '[:upper:]' '[:lower:]')" in
        0|false|no|off)
            echo "ERROR: automatic Rust repair is disabled by RBE_BUILD_AUTO_REPAIR_RUST." >&2
            exit 1
            ;;
    esac

    echo "Repairing Rust toolchain '$toolchain' before the RBE build..." >&2
    toolchain_name="$(rustup toolchain list 2>/dev/null | awk '{print $1}' | awk -v requested="$toolchain" '$0 == requested || index($0, requested "-") == 1 { print; exit }')"

    if [ -n "$toolchain_name" ]; then
        if ! rustup toolchain uninstall "$toolchain_name" >/dev/null 2>&1; then
            rustup_home="${RUSTUP_HOME:-$(rustup show home 2>/dev/null || true)}"
            if [ -n "$rustup_home" ] \
                && [[ "$toolchain_name" =~ ^[A-Za-z0-9._-]+$ ]] \
                && [ -d "$rustup_home/toolchains/$toolchain_name" ]; then
                echo "rustup could not uninstall the damaged toolchain cleanly; removing only its broken toolchain directory." >&2
                rm -rf -- "$rustup_home/toolchains/$toolchain_name"
            fi
        fi
    fi

    rustup toolchain install "$toolchain" --profile minimal
    hash -r

    if ! rust_toolchain_healthy "$toolchain"; then
        echo "ERROR: Rust toolchain '$toolchain' is still unhealthy after automatic repair." >&2
        echo "Try clearing the host rustup cache or reinstalling rustup before rebuilding RBE." >&2
        exit 1
    fi
}

ensure_linux_build_tools() {
    local mode="$1"
    local toolchain="${RBE_RUST_TOOLCHAIN:-stable}"

    echo "Checking Linux RBE build prerequisites..." >&2

    command -v rustup >/dev/null 2>&1 || linux_build_tool_error "rustup is required to build RBE."

    if ! rust_toolchain_healthy "$toolchain"; then
        repair_rust_toolchain "$toolchain"
    fi

    export RUSTUP_TOOLCHAIN="$toolchain"
    hash -r

    command -v rustc >/dev/null 2>&1 || linux_build_tool_error "rustc is missing after Rust toolchain setup."
    command -v cargo >/dev/null 2>&1 || linux_build_tool_error "cargo is missing after Rust toolchain setup."
    command -v cc >/dev/null 2>&1 || linux_build_tool_error "a C compiler/linker driver (cc) is required."

    if [ "$mode" = release ]; then
        command -v git >/dev/null 2>&1 || linux_build_tool_error "git is required for reproducible release build identity."
        command -v openssl >/dev/null 2>&1 || linux_build_tool_error "OpenSSL is required for release build credentials/signing material."
    fi

    echo "Rust toolchain: $toolchain" >&2
    echo "  rustc: $(rustc --version)" >&2
    echo "  cargo: $(cargo --version)" >&2
    echo "  rustup: $(rustup --version 2>/dev/null | head -n 1)" >&2
    echo "  cc: $(cc --version 2>/dev/null | head -n 1)" >&2
    if [ "$mode" = release ]; then
        echo "  git: $(git --version)" >&2
        echo "  openssl: $(openssl version)" >&2
    fi
    echo "Linux RBE build prerequisite check passed." >&2
}

normalized=()
only_seen=false
build_sdk=false
help_requested=false
check_tools=false

for raw in "$@"; do
    arg="$raw"
    case "$arg" in
        -build-sdk) arg=--build-sdk ;;
        -only-*) arg="--only-${arg#-only-}" ;;
        --cloud-node-only|--build-cloud-node|-CloudNodeOnly) arg=--only-cloud-node ;;
    esac

    case "$arg" in
        --only-*)
            if $only_seen; then
                echo 'ERROR: only one --only-<component> selector may be used.' >&2
                exit 2
            fi
            only_seen=true
            component="$(normalize_only_component "${arg#--only-}")"
            normalized+=("--only-$component")
            ;;
        --build-sdk)
            build_sdk=true
            normalized+=("$arg")
            ;;
        --check-tools)
            check_tools=true
            ;;
        --help|-help|-h|-\?)
            help_requested=true
            normalized+=("$arg")
            ;;
        *) normalized+=("$arg") ;;
    esac
done

if $help_requested; then
    if [ -f "$HELP_PATH" ]; then
        cat "$HELP_PATH"
    else
        echo 'RBE build help is missing. Expected build-help.txt beside build.sh.' >&2
    fi
    exit 0
fi

if $only_seen && ! $build_sdk; then
    for arg in "${normalized[@]}"; do
        if [ "$arg" = '--only-sdk-backend' ]; then
            echo 'ERROR: --only-sdk-backend is valid only together with --build-sdk.' >&2
            exit 2
        fi
    done
fi

if [[ "$(uname -s)" == Linux* ]]; then
    preflight_mode=release
    if $build_sdk || $only_seen; then
        preflight_mode=selective
    fi
    ensure_linux_build_tools "$preflight_mode"
elif $check_tools; then
    echo "INFO: --check-tools currently performs the strict host prerequisite check on Linux builds." >&2
fi

configure_cargo_build_cache

if $check_tools; then
    exit 0
fi

if $build_sdk || $only_seen; then
    exec "$REPO_ROOT/build-select.sh" "${normalized[@]}"
fi

exec "$REPO_ROOT/build-release.sh" "${normalized[@]}"
