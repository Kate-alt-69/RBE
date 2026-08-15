#!/bin/bash
#
# Build script for RBE (engine + container-runtime) for one or more target
# platforms/architectures. By default, for each target it builds
# container-bin FIRST and embeds its bytes into the engine's
# backend executable.
#
# Platform flags (pick one or more):
#   --build-win10 / --build-win11 / --build-windows   (all three are
#       the SAME Rust target, x86_64/aarch64-pc-windows-msvc)
#   --build-linux
#   --build-macos
#   --build-all        builds a reasonable "ship everywhere" default
#                       set: windows-x64, linux-x64-gnu, linux-x64-musl,
#                       macos-x64, macos-arm64
#
# Architecture flags (pick one or more; default is the host's own
# architecture if none given):
#   --arch-x64          (aliases: --architect-x64, --achitect-x64)
#   --arch-x86          (aliases: --architect-x86, --achitext-x86)
#   --arch-arm64        (aliases: --arch-arm)
#   --arch-armv7
#
# Other flags:
#   --musl              Linux only — build against musl libc instead of
#                        glibc
#   --no-embed          build backend and container-bin separately,
#                        don't embed one into the other
#   --dev-content       copy this repo's actual dev api/ and module/
#                        content into dist/<target>/
#   --debug             debug profile instead of release (default)
#   --target=<triple>   bypass the OS/arch flags entirely and build
#                        for an exact Rust target triple
#   --distro=<name>     map a Linux distro name to its targets
#   --cache-remove      remove all cached builds
#   --cache-remove-win-cache   remove Windows cache only
#   --cache-remove-linux-cache remove Linux cache only
#   --cache-remove-all-cache   remove all cached builds
#   --help
#

set -e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Argument defaults
BUILD_WIN=false
BUILD_LINUX=false
BUILD_MACOS=false
BUILD_ALL=false
MUSL=false
NO_EMBED=false
DEV_CONTENT=false
RELEASE=true
ARCH_X64=false
ARCH_X86=false
ARCH_ARM64=false
ARCH_ARMV7=false
CUSTOM_TARGET=""
SHOW_HELP=false
CACHE_REMOVE=false
CACHE_REMOVE_WIN=false
CACHE_REMOVE_LINUX=false
CACHE_REMOVE_ALL=false
DISTRO=""

# Parse arguments
for arg in "$@"; do
    case "$arg" in
        --build-win|--build-win10|--build-win11|--build-windows)
            BUILD_WIN=true
            ;;
        --build-linux)
            BUILD_LINUX=true
            ;;
        --build-macos)
            BUILD_MACOS=true
            ;;
        --build-all)
            BUILD_ALL=true
            ;;
        --musl)
            MUSL=true
            ;;
        --no-embed)
            NO_EMBED=true
            ;;
        --dev-content)
            DEV_CONTENT=true
            ;;
        --debug)
            RELEASE=false
            ;;
        --arch-x64|--architect-x64|--achitect-x64)
            ARCH_X64=true
            ;;
        --arch-x86|--architect-x86|--achitext-x86)
            ARCH_X86=true
            ;;
        --arch-arm|--arch-arm64)
            ARCH_ARM64=true
            ;;
        --arch-armv7)
            ARCH_ARMV7=true
            ;;
        --target=*)
            CUSTOM_TARGET="${arg#--target=}"
            ;;
        --distro=*)
            DISTRO="${arg#--distro=}"
            ;;
        --cache-remove)
            CACHE_REMOVE=true
            ;;
        --cache-remove-win-cache)
            CACHE_REMOVE_WIN=true
            ;;
        --cache-remove-linux-cache)
            CACHE_REMOVE_LINUX=true
            ;;
        --cache-remove-all-cache)
            CACHE_REMOVE_ALL=true
            ;;
        --help|-h|-\?)
            SHOW_HELP=true
            ;;
        *)
            echo "WARNING: build.sh: unrecognized argument '$arg' — ignoring (run with --help for usage)" >&2
            ;;
    esac
done

if [ "$SHOW_HELP" = true ]; then
    grep "^#" "$0" | sed 's/^# //' | head -60
    exit 0
fi

if [ "$BUILD_WIN" = false ] && [ "$BUILD_LINUX" = false ] && [ "$BUILD_MACOS" = false ] && [ "$BUILD_ALL" = false ] && [ -z "$CUSTOM_TARGET" ]; then
    echo "No platform flag given — building for the host platform only. Use --help to see all options." >&2
fi

# ---------------------------------------------------------------------------
# Detect host OS and architecture
# ---------------------------------------------------------------------------

detect_host_os() {
    case "$(uname -s)" in
        Linux*)  echo "linux" ;;
        Darwin*) echo "macos" ;;
        MINGW*|MSYS*) echo "windows" ;;
        *) echo "unknown" ;;
    esac
}

detect_host_arch() {
    local arch=$(uname -m)
    case "$arch" in
        x86_64|amd64)  echo "x64" ;;
        i386|i686)     echo "x86" ;;
        aarch64|arm64) echo "arm64" ;;
        armv7*|armv7l) echo "armv7" ;;
        *) echo "$arch" ;;
    esac
}

HOST_OS=$(detect_host_os)
HOST_ARCH=$(detect_host_arch)

# ---------------------------------------------------------------------------
# Get requested architectures
# ---------------------------------------------------------------------------

get_requested_arches() {
    local arches=()
    if [ "$ARCH_X64" = true ]; then arches+=("x64"); fi
    if [ "$ARCH_X86" = true ]; then arches+=("x86"); fi
    if [ "$ARCH_ARM64" = true ]; then arches+=("arm64"); fi
    if [ "$ARCH_ARMV7" = true ]; then arches+=("armv7"); fi
    
    if [ ${#arches[@]} -eq 0 ]; then
        arches=("$HOST_ARCH")
    fi
    printf '%s\n' "${arches[@]}"
}

# ---------------------------------------------------------------------------
# Resolve target triple from OS and architecture
# ---------------------------------------------------------------------------

resolve_target() {
    local os="$1" arch="$2" use_musl="$3"
    
    case "$os" in
        windows)
            case "$arch" in
                x64) echo "x86_64-pc-windows-msvc" ;;
                x86) echo "i686-pc-windows-msvc" ;;
                arm64) echo "aarch64-pc-windows-msvc" ;;
                armv7) echo "thumbv7-pc-windows-msvc" ;;
                *) echo "x86_64-pc-windows-msvc" ;;
            esac
            ;;
        linux)
            case "$arch" in
                x64)
                    if [ "$use_musl" = true ]; then echo "x86_64-unknown-linux-musl"; else echo "x86_64-unknown-linux-gnu"; fi
                    ;;
                x86)
                    if [ "$use_musl" = true ]; then echo "i686-unknown-linux-musl"; else echo "i686-unknown-linux-gnu"; fi
                    ;;
                arm64)
                    if [ "$use_musl" = true ]; then echo "aarch64-unknown-linux-musl"; else echo "aarch64-unknown-linux-gnu"; fi
                    ;;
                armv7)
                    if [ "$use_musl" = true ]; then echo "armv7-unknown-linux-musleabihf"; else echo "armv7-unknown-linux-gnueabihf"; fi
                    ;;
                *) echo "x86_64-unknown-linux-gnu" ;;
            esac
            ;;
        macos)
            case "$arch" in
                x64) echo "x86_64-apple-darwin" ;;
                x86) echo "i686-apple-darwin" ;;
                arm64) echo "aarch64-apple-darwin" ;;
                armv7) echo "armv7-apple-darwin" ;;
                *) echo "x86_64-apple-darwin" ;;
            esac
            ;;
        *) echo "$os-$arch" ;;
    esac
}

# ---------------------------------------------------------------------------
# Get targets for a Linux distribution
# ---------------------------------------------------------------------------

get_targets_for_distro() {
    local distro="$1"
    [ -z "$distro" ] && return
    
    distro=$(echo "$distro" | tr '[:upper:]' '[:lower:]' | xargs)
    
    # Parse optional arch suffix: e.g. ubuntu-x64, alpine-arm64
    local arch=""
    if [[ "$distro" =~ ^(.*)[_-](x64|x86|arm64|armv7)$ ]]; then
        distro="${BASH_REMATCH[1]}"
        arch="${BASH_REMATCH[2]}"
    fi
    
    case "$distro" in
        all-linux|common)
            echo "x86_64-unknown-linux-gnu"
            echo "x86_64-unknown-linux-musl"
            echo "i686-unknown-linux-gnu"
            echo "i686-unknown-linux-musl"
            echo "aarch64-unknown-linux-gnu"
            echo "aarch64-unknown-linux-musl"
            echo "armv7-unknown-linux-gnueabihf"
            echo "armv7-unknown-linux-musleabihf"
            ;;
        ubuntu|debian|linuxmint|kali|pop|elementary|zorin|deepin)
            case "$arch" in
                x64) echo "x86_64-unknown-linux-gnu" ;;
                x86) echo "i686-unknown-linux-gnu" ;;
                arm64) echo "aarch64-unknown-linux-gnu" ;;
                armv7) echo "armv7-unknown-linux-gnueabihf" ;;
                *)
                    echo "x86_64-unknown-linux-gnu"
                    echo "i686-unknown-linux-gnu"
                    echo "aarch64-unknown-linux-gnu"
                    echo "armv7-unknown-linux-gnueabihf"
                    ;;
            esac
            ;;
        fedora|centos|rhel|amazonlinux|oraclelinux|almalinux|rocky|rocky-linux)
            case "$arch" in
                x64) echo "x86_64-unknown-linux-gnu" ;;
                x86) echo "i686-unknown-linux-gnu" ;;
                arm64) echo "aarch64-unknown-linux-gnu" ;;
                armv7) echo "armv7-unknown-linux-gnueabihf" ;;
                *)
                    echo "x86_64-unknown-linux-gnu"
                    echo "aarch64-unknown-linux-gnu"
                    ;;
            esac
            ;;
        arch|manjaro|artix)
            case "$arch" in
                x86) echo "i686-unknown-linux-gnu" ;;
                arm64) echo "aarch64-unknown-linux-gnu" ;;
                *) echo "x86_64-unknown-linux-gnu" ;;
            esac
            ;;
        opensuse|suse|tumbleweed|sles)
            echo "x86_64-unknown-linux-gnu"
            echo "aarch64-unknown-linux-gnu"
            ;;
        alpine|musl|void-musl)
            case "$arch" in
                x64) echo "x86_64-unknown-linux-musl" ;;
                x86) echo "i686-unknown-linux-musl" ;;
                arm64) echo "aarch64-unknown-linux-musl" ;;
                armv7) echo "armv7-unknown-linux-musleabihf" ;;
                *)
                    echo "x86_64-unknown-linux-musl"
                    echo "aarch64-unknown-linux-musl"
                    echo "i686-unknown-linux-musl"
                    echo "armv7-unknown-linux-musleabihf"
                    ;;
            esac
            ;;
        gentoo|slackware|void|nixos|guix)
            echo "x86_64-unknown-linux-gnu"
            echo "aarch64-unknown-linux-gnu"
            ;;
        raspbian|raspios|pi|raspberry)
            case "$arch" in
                arm64) echo "aarch64-unknown-linux-gnu" ;;
                *) echo "armv7-unknown-linux-gnueabihf" ;;
            esac
            ;;
        clear|clearlinux)
            echo "x86_64-unknown-linux-gnu"
            ;;
        wsl|windows-subsystem-for-linux|chromeos)
            case "$arch" in
                arm64) echo "aarch64-unknown-linux-gnu" ;;
                x86) echo "i686-unknown-linux-gnu" ;;
                *) echo "x86_64-unknown-linux-gnu" ;;
            esac
            ;;
        openwrt)
            echo "WARNING: OpenWRT builds are specialized; prefer --target=... or use a dedicated toolchain" >&2
            ;;
        android|termux)
            echo "WARNING: Android/Termux targets require the Android NDK; returning Android triples but builds may fail without proper toolchain" >&2
            case "$arch" in
                arm64) echo "aarch64-linux-android" ;;
                x86) echo "i686-linux-android" ;;
                x64) echo "x86_64-linux-android" ;;
                *)
                    echo "x86_64-linux-android"
                    echo "aarch64-linux-android"
                    echo "i686-linux-android"
                    echo "armv7-linux-androideabi"
                    ;;
            esac
            ;;
        *)
            if [[ "$distro" =~ ^[a-z0-9_-]+-[a-z0-9_-]+-[a-z0-9_-]+$ ]]; then
                echo "$distro"
            else
                echo "WARNING: Unknown distro alias '$distro' — ignoring" >&2
            fi
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Build targets array
# ---------------------------------------------------------------------------

declare -a targets=()

if [ -n "$CUSTOM_TARGET" ]; then
    targets+=("$CUSTOM_TARGET")
elif [ "$BUILD_ALL" = true ]; then
    targets+=(
        "x86_64-pc-windows-msvc"
        "x86_64-unknown-linux-gnu"
        "x86_64-unknown-linux-musl"
        "i686-unknown-linux-gnu"
        "i686-unknown-linux-musl"
        "aarch64-unknown-linux-gnu"
        "aarch64-unknown-linux-musl"
        "armv7-unknown-linux-gnueabihf"
        "armv7-unknown-linux-musleabihf"
    )
    if [ "$HOST_OS" = "macos" ]; then
        targets+=(
            "x86_64-apple-darwin"
            "aarch64-apple-darwin"
        )
    else
        echo "INFO: host is not macOS — macOS targets removed from --build-all" >&2
    fi
else
    arches=()
    mapfile -t arches < <(get_requested_arches)
    
    if [ "$BUILD_WIN" = true ]; then
        for arch in "${arches[@]}"; do
            targets+=("$(resolve_target "windows" "$arch" false)")
        done
    fi
    if [ "$BUILD_LINUX" = true ]; then
        for arch in "${arches[@]}"; do
            targets+=("$(resolve_target "linux" "$arch" "$MUSL")")
        done
    fi
    if [ "$BUILD_MACOS" = true ]; then
        for arch in "${arches[@]}"; do
            targets+=("$(resolve_target "macos" "$arch" false)")
        done
    fi
    
    if [ "$BUILD_WIN" = false ] && [ "$BUILD_LINUX" = false ] && [ "$BUILD_MACOS" = false ]; then
        for arch in "${arches[@]}"; do
            targets+=("$(resolve_target "$HOST_OS" "$arch" "$MUSL")")
        done
    fi
fi

if [ -n "$DISTRO" ]; then
    mapfile -t mapped_targets < <(get_targets_for_distro "$DISTRO")
    for target in "${mapped_targets[@]}"; do
        found=false
        for t in "${targets[@]}"; do
            if [ "$t" = "$target" ]; then
                found=true
                break
            fi
        done
        if [ "$found" = false ]; then
            targets+=("$target")
        fi
    done
    echo "After distro mapping, targets: ${targets[*]}" >&2
fi

echo "Targets: ${targets[*]}" >&2

# ---------------------------------------------------------------------------
# Cache removal helpers
# ---------------------------------------------------------------------------

get_workspace_dirs() {
    local dirs=()
    local candidates=("engine" "container-runtime" "atomic-io" "vault")
    for d in "${candidates[@]}"; do
        local p="$REPO_ROOT/$d"
        if [ -f "$p/Cargo.toml" ]; then
            dirs+=("$p")
        fi
    done
    printf '%s\n' "${dirs[@]}"
}

do_cache_remove() {
    local remove_all="$1"
    shift
    local triples=("$@")
    
    local workspaces
    mapfile -t workspaces < <(get_workspace_dirs)
    
    if [ ${#workspaces[@]} -eq 0 ]; then
        echo "No Rust workspace dirs found to clean" >&2
        return
    fi
    
    for w in "${workspaces[@]}"; do
        echo "Cleaning workspace: $w" >&2
        pushd "$w" > /dev/null
        if [ "$remove_all" = true ]; then
            echo "  cargo clean" >&2
            cargo clean
        else
            for t in "${triples[@]}"; do
                echo "  cargo clean --target $t" >&2
                cargo clean --target "$t"
            done
        fi
        popd > /dev/null
    done
}

# ---------------------------------------------------------------------------
# Cache removal
# ---------------------------------------------------------------------------

if [ "$CACHE_REMOVE" = true ] || [ "$CACHE_REMOVE_WIN" = true ] || [ "$CACHE_REMOVE_LINUX" = true ] || [ "$CACHE_REMOVE_ALL" = true ]; then
    if [ "$CACHE_REMOVE" = true ] || [ "$CACHE_REMOVE_ALL" = true ]; then
        do_cache_remove true
    else
        triples=()
        if [ "$CACHE_REMOVE_WIN" = true ]; then
            triples+=(
                "x86_64-pc-windows-msvc"
                "i686-pc-windows-msvc"
                "aarch64-pc-windows-msvc"
            )
        fi
        if [ "$CACHE_REMOVE_LINUX" = true ]; then
            triples+=(
                "x86_64-unknown-linux-gnu"
                "x86_64-unknown-linux-musl"
                "i686-unknown-linux-gnu"
                "i686-unknown-linux-musl"
                "aarch64-unknown-linux-gnu"
                "aarch64-unknown-linux-musl"
                "armv7-unknown-linux-gnueabihf"
                "armv7-unknown-linux-musleabihf"
            )
        fi
        if [ ${#triples[@]} -gt 0 ]; then
            do_cache_remove false "${triples[@]}"
        fi
    fi
    
    if [ "$BUILD_WIN" = false ] && [ "$BUILD_LINUX" = false ] && [ "$BUILD_MACOS" = false ] && [ "$BUILD_ALL" = false ] && [ -z "$CUSTOM_TARGET" ]; then
        echo "Cache removal complete." >&2
        exit 0
    fi
fi

# ---------------------------------------------------------------------------
# Build helpers
# ---------------------------------------------------------------------------

detect_linux_distro() {
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        echo "${ID:-linux}"
    elif [ -f /etc/lsb-release ]; then
        . /etc/lsb-release
        echo "${DISTRIB_ID:-linux}" | tr '[:upper:]' '[:lower:]'
    elif command -v lsb_release &> /dev/null; then
        lsb_release -si | tr '[:upper:]' '[:lower:]'
    else
        echo "linux"
    fi
}

install_windows_build_tools() {
    if [ "$HOST_OS" != "linux" ]; then
        return 0
    fi
    
    echo "Checking for Windows cross-compilation tools..." >&2
    
    # Prefer cargo-zigbuild with Zig (like PowerShell version)
    if ! command -v cargo-zigbuild &> /dev/null && ! command -v zig &> /dev/null; then
        echo "Installing cargo-zigbuild and Zig for Windows cross-compilation..." >&2
        
        if ! command -v zig &> /dev/null; then
            echo "  Installing Zig..." >&2
            if command -v apt-get &> /dev/null; then
                sudo apt-get update > /dev/null 2>&1 && sudo apt-get install -y zig > /dev/null 2>&1 || {
                    echo "  NOTE: apt-get zig installation failed. Download from: https://ziglang.org/download/" >&2
                }
            elif command -v brew &> /dev/null; then
                brew install zig > /dev/null 2>&1 || {
                    echo "  NOTE: brew zig installation failed. Download from: https://ziglang.org/download/" >&2
                }
            else
                echo "  NOTE: Please install Zig from https://ziglang.org/download/" >&2
            fi
        fi
        
        if ! command -v cargo-zigbuild &> /dev/null; then
            echo "  Installing cargo-zigbuild..." >&2
            cargo install cargo-zigbuild 2>&1 | grep -v "^Updating" | grep -v "^Downloading" | tail -5 || true
        fi
    fi
    
    # Fallback: suggest/install cross (Docker-based)
    if ! command -v cross &> /dev/null && ! command -v cargo-zigbuild &> /dev/null && ! command -v zig &> /dev/null; then
        echo "Installing 'cross' (Docker-based cross-linker) for Windows builds..." >&2
        if command -v apt-get &> /dev/null; then
            sudo apt-get install -y docker.io > /dev/null 2>&1 || {
                echo "  NOTE: Docker installation failed. Install from: https://docs.docker.com/install/" >&2
            }
        fi
        cargo install cross 2>&1 | grep -v "^Updating" | grep -v "^Downloading" | tail -5 || true
    fi
    
    # Fallback: mingw-w64 for traditional GCC-based linking
    if ! command -v cargo-zigbuild &> /dev/null && ! command -v cross &> /dev/null; then
        echo "  Attempting to install mingw-w64 as fallback..." >&2
        distro=$(detect_linux_distro)
        case "$distro" in
            ubuntu|debian)
                echo "    sudo apt-get install -y mingw-w64" >&2
                sudo apt-get update > /dev/null 2>&1 && sudo apt-get install -y mingw-w64 > /dev/null 2>&1 || {
                    echo "    NOTE: mingw-w64 installation failed. Install manually with: sudo apt-get install mingw-w64" >&2
                }
                ;;
            fedora|rhel|centos)
                echo "    sudo dnf install -y mingw64-gcc mingw64-gcc-c++" >&2
                sudo dnf install -y mingw64-gcc mingw64-gcc-c++ > /dev/null 2>&1 || {
                    echo "    NOTE: mingw-w64 installation failed. Install manually with: sudo dnf install mingw64-gcc mingw64-gcc-c++" >&2
                }
                ;;
            arch|manjaro)
                echo "    sudo pacman -S mingw-w64-gcc" >&2
                sudo pacman -S mingw-w64-gcc > /dev/null 2>&1 || {
                    echo "    NOTE: mingw-w64 installation failed. Install manually with: sudo pacman -S mingw-w64-gcc" >&2
                }
                ;;
            alpine)
                echo "    apk add mingw-w64" >&2
                sudo apk add mingw-w64 > /dev/null 2>&1 || {
                    echo "    NOTE: mingw-w64 installation failed. Install manually with: apk add mingw-w64" >&2
                }
                ;;
        esac
    fi
}

get_cross_cmd() {
    if command -v cross &> /dev/null; then
        echo "cross"
    elif command -v cargo-zigbuild &> /dev/null; then
        echo "cargo-zigbuild"
    else
        echo ""
    fi
}

# Check if we're building for Windows from Linux and install tools if needed
if [ "$HOST_OS" = "linux" ] && ([ "$BUILD_WIN" = true ] || [ "$BUILD_ALL" = true ]); then
    install_windows_build_tools
fi

# Detect cross-compilation tool after installation attempt
CROSS_CMD=$(get_cross_cmd)

test_target_installed() {
    local target="$1"
    if ! command -v rustup &> /dev/null; then
        echo "ERROR: rustup is not installed or not in PATH" >&2
        return 1
    fi
    if rustup target list --installed 2>/dev/null | grep -q "^$target$"; then
        return 0
    else
        return 1
    fi
}

install_target_if_missing() {
    local target="$1"
    if ! command -v rustup &> /dev/null; then
        echo "ERROR: rustup is not installed. Install Rust from https://rustup.rs/" >&2
        exit 1
    fi
    if ! test_target_installed "$target"; then
        echo "  rustup target '$target' not installed — adding it" >&2
        rustup target add "$target"
        if [ $? -ne 0 ]; then
            echo "ERROR: Failed to add rustup target '$target'" >&2
            exit 1
        fi
    fi
}

get_target_os() {
    local target="$1"
    case "$target" in
        *windows*) echo "windows" ;;
        *darwin*) echo "macos" ;;
        *) echo "linux" ;;
    esac
}

invoke_cargo_build() {
    local package="$1" target="$2" is_release="$3"
    
    install_target_if_missing "$target"
    
    local target_os=$(get_target_os "$target")
    local use_cross=false
    if [ "$target_os" != "$HOST_OS" ] && [ -n "$CROSS_CMD" ]; then
        use_cross=true
    fi
    
    if [ "$target_os" != "$HOST_OS" ] && [ -z "$CROSS_CMD" ]; then
        echo "WARNING: Cross-OS build ($HOST_OS -> $target_os) without 'cross' or 'cargo-zigbuild' installed — this may fail to link." >&2
    fi
    
    local cargo_args=("build" "-p" "$package" "--target" "$target")
    if [ "$is_release" = true ]; then
        cargo_args+=("--release")
    fi
    
    local tool="cargo"
    local tool_args=()
    
    if [ "$use_cross" = true ]; then
        if [ "$CROSS_CMD" = "cross" ]; then
            tool="cross"
            tool_args=("${cargo_args[@]}")
        elif [ "$CROSS_CMD" = "cargo-zigbuild" ]; then
            tool="cargo"
            tool_args=("zigbuild")
            for arg in "${cargo_args[@]:1}"; do
                tool_args+=("$arg")
            done
        fi
    else
        tool_args=("${cargo_args[@]}")
    fi
    
    echo "  $tool ${tool_args[*]}" >&2
    "$tool" "${tool_args[@]}"
}

get_built_binary_path() {
    local workspace_dir="$1" bin_name="$2" target="$3" is_release="$4"
    
    local profile_dir="debug"
    if [ "$is_release" = true ]; then
        profile_dir="release"
    fi
    
    local target_os=$(get_target_os "$target")
    local file_name="$bin_name"
    if [ "$target_os" = "windows" ]; then
        file_name="$bin_name.exe"
    fi
    
    if [ -n "$CARGO_TARGET_DIR" ]; then
        echo "$CARGO_TARGET_DIR/$target/$profile_dir/$file_name"
    else
        echo "$workspace_dir/target/$target/$profile_dir/$file_name"
    fi
}

# ---------------------------------------------------------------------------
# Build loop
# ---------------------------------------------------------------------------

DIST_ROOT="$REPO_ROOT/dist"
ENGINE_DIR="$REPO_ROOT/engine"
CONTAINER_DIR="$REPO_ROOT/container-runtime"

for target in "${targets[@]}"; do
    echo ""
    echo "=== Building for $target ===" >&2
    
    out_dir="$DIST_ROOT/$target"
    mkdir -p "$out_dir"
    
    container_bin_path=""
    if [ "$NO_EMBED" = false ]; then
        echo "-- container-bin ($target) --" >&2
        pushd "$CONTAINER_DIR" > /dev/null
        invoke_cargo_build "container-bin" "$target" "$RELEASE"
        container_bin_path=$(get_built_binary_path "$CONTAINER_DIR" "container-bin" "$target" "$RELEASE")
        popd > /dev/null
    fi
    
    echo "-- backend ($target) --" >&2
    pushd "$ENGINE_DIR" > /dev/null
    if [ -n "$container_bin_path" ] && [ -f "$container_bin_path" ]; then
        export RBE_CONTAINER_BIN_PATH="$container_bin_path"
    else
        unset RBE_CONTAINER_BIN_PATH
    fi
    invoke_cargo_build "backend" "$target" "$RELEASE"
    backend_path=$(get_built_binary_path "$ENGINE_DIR" "backend" "$target" "$RELEASE")
    unset RBE_CONTAINER_BIN_PATH
    popd > /dev/null
    
    cp "$backend_path" "$out_dir/"
    echo "  -> $out_dir/$(basename "$backend_path")" >&2
    
    if [ "$NO_EMBED" = true ] && [ -n "$container_bin_path" ] && [ -f "$container_bin_path" ]; then
        cp "$container_bin_path" "$out_dir/"
        echo "  -> $out_dir/$(basename "$container_bin_path")" >&2
    fi
    
    # Copy settings.json
    if [ -f "$ENGINE_DIR/settings.json" ]; then
        cp "$ENGINE_DIR/settings.json" "$out_dir/" 2>/dev/null || true
    fi
    
    # Create or copy api/ and module/ directories
    if [ "$DEV_CONTENT" = true ]; then
        if [ -d "$REPO_ROOT/api" ]; then
            cp -r "$REPO_ROOT/api" "$out_dir/" 2>/dev/null || true
        fi
        if [ -d "$REPO_ROOT/module" ]; then
            cp -r "$REPO_ROOT/module" "$out_dir/" 2>/dev/null || true
        fi
    else
        mkdir -p "$out_dir/api"
        mkdir -p "$out_dir/module"
    fi
    
    # Create launch script for Linux targets
    target_os=$(get_target_os "$target")
    if [ "$target_os" = "linux" ]; then
        launch_path="$out_dir/launch.sh"
        cat > "$launch_path" << 'EOF'
#!/bin/sh
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
if [ -f "$DIR/container-bin" ]; then
  export RBE_CONTAINER_BIN_PATH="$DIR/container-bin"
fi
exec "$DIR/backend" "$@"
EOF
        chmod +x "$launch_path"
    fi
done

echo ""
echo "Done. Output in $DIST_ROOT" >&2
