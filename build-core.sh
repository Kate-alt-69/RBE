#!/bin/bash
# RBE secure release builder.
# Builds container-bin first, binds its exact bytes to backend.exe at build time,
# and packages the same artifact as dist/<target>/dep/container(.exe).
# Packaged builds require RBE_CONTAINER_SIGNING_PRIVATE_KEY in the environment.
# --no-embed is retained only for CLI compatibility; production still requires
# the standalone signed dep/container artifact.
set -e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_WIN=false; BUILD_LINUX=false; BUILD_MACOS=false; BUILD_ALL=false; MUSL=false
NO_EMBED=false; DEV_CONTENT=false; RELEASE=true; ARCH_X64=false; ARCH_X86=false
ARCH_ARM64=false; ARCH_ARMV7=false; CUSTOM_TARGET=""; SHOW_HELP=false
CACHE_REMOVE=false; CACHE_REMOVE_WIN=false; CACHE_REMOVE_LINUX=false; CACHE_REMOVE_ALL=false; DISTRO=""

for arg in "$@"; do
    case "$arg" in
        --build-win|--build-win10|--build-win11|--build-windows) BUILD_WIN=true ;;
        --build-linux) BUILD_LINUX=true ;;
        --build-macos) BUILD_MACOS=true ;;
        --build-all) BUILD_ALL=true ;;
        --musl) MUSL=true ;;
        --no-embed) NO_EMBED=true ;;
        --dev-content) DEV_CONTENT=true ;;
        --debug) RELEASE=false ;;
        --arch-x64|--architect-x64|--achitect-x64) ARCH_X64=true ;;
        --arch-x86|--architect-x86|--achitext-x86) ARCH_X86=true ;;
        --arch-arm|--arch-arm64) ARCH_ARM64=true ;;
        --arch-armv7) ARCH_ARMV7=true ;;
        --target=*) CUSTOM_TARGET="${arg#--target=}" ;;
        --distro=*) DISTRO="${arg#--distro=}" ;;
        --cache-remove) CACHE_REMOVE=true ;;
        --cache-remove-win-cache) CACHE_REMOVE_WIN=true ;;
        --cache-remove-linux-cache) CACHE_REMOVE_LINUX=true ;;
        --cache-remove-all-cache) CACHE_REMOVE_ALL=true ;;
        --help|-h|-\?) SHOW_HELP=true ;;
        *) echo "WARNING: unrecognized argument '$arg' — ignoring" >&2 ;;
    esac
done

if [ "$SHOW_HELP" = true ]; then
    sed -n '1,70p' "$REPO_ROOT/build.sh"
    exit 0
fi

HOST_OS=$(case "$(uname -s)" in Linux*) echo linux;; Darwin*) echo macos;; MINGW*|MSYS*) echo windows;; *) echo unknown;; esac)
HOST_ARCH=$(case "$(uname -m)" in x86_64|amd64) echo x64;; i386|i686) echo x86;; aarch64|arm64) echo arm64;; armv7*|armv7l) echo armv7;; *) echo x64;; esac)

resolve_target() {
    local os="$1" arch="$2" musl="$3"
    case "$os" in
        windows) case "$arch" in x64) echo x86_64-pc-windows-msvc;; x86) echo i686-pc-windows-msvc;; arm64) echo aarch64-pc-windows-msvc;; armv7) echo thumbv7-pc-windows-msvc;; esac;;
        linux) case "$arch" in x64) [ "$musl" = true ] && echo x86_64-unknown-linux-musl || echo x86_64-unknown-linux-gnu;; x86) [ "$musl" = true ] && echo i686-unknown-linux-musl || echo i686-unknown-linux-gnu;; arm64) [ "$musl" = true ] && echo aarch64-unknown-linux-musl || echo aarch64-unknown-linux-gnu;; armv7) [ "$musl" = true ] && echo armv7-unknown-linux-musleabihf || echo armv7-unknown-linux-gnueabihf;; esac;;
        macos) case "$arch" in x64) echo x86_64-apple-darwin;; arm64) echo aarch64-apple-darwin;; esac;;
    esac
}
get_target_os() { case "$1" in *windows*) echo windows;; *darwin*) echo macos;; *) echo linux;; esac; }

arches=()
[ "$ARCH_X64" = true ] && arches+=(x64); [ "$ARCH_X86" = true ] && arches+=(x86); [ "$ARCH_ARM64" = true ] && arches+=(arm64); [ "$ARCH_ARMV7" = true ] && arches+=(armv7)
[ ${#arches[@]} -eq 0 ] && arches=("$HOST_ARCH")
declare -a targets=()
if [ -n "$CUSTOM_TARGET" ]; then
    targets+=("$CUSTOM_TARGET")
elif [ "$BUILD_ALL" = true ]; then
    targets+=(x86_64-pc-windows-msvc x86_64-unknown-linux-gnu x86_64-unknown-linux-musl i686-unknown-linux-gnu i686-unknown-linux-musl aarch64-unknown-linux-gnu aarch64-unknown-linux-musl armv7-unknown-linux-gnueabihf armv7-unknown-linux-musleabihf)
    [ "$HOST_OS" = macos ] && targets+=(x86_64-apple-darwin aarch64-apple-darwin) || echo "INFO: host is not macOS — macOS targets removed from --build-all" >&2
else
    [ "$BUILD_WIN" = true ] && for a in "${arches[@]}"; do targets+=("$(resolve_target windows "$a" false)"); done
    [ "$BUILD_LINUX" = true ] && for a in "${arches[@]}"; do targets+=("$(resolve_target linux "$a" "$MUSL")"); done
    [ "$BUILD_MACOS" = true ] && for a in "${arches[@]}"; do targets+=("$(resolve_target macos "$a" false)"); done
    if [ "$BUILD_WIN" = false ] && [ "$BUILD_LINUX" = false ] && [ "$BUILD_MACOS" = false ]; then for a in "${arches[@]}"; do targets+=("$(resolve_target "$HOST_OS" "$a" "$MUSL")"); done; fi
fi
echo "Targets: ${targets[*]}" >&2

# Linux -> Windows uses cargo-zigbuild through build.sh's cargo-xwin shim.
CROSS_CMD=""
command -v cross >/dev/null 2>&1 && CROSS_CMD=cross
[ -z "$CROSS_CMD" ] && command -v cargo-zigbuild >/dev/null 2>&1 && CROSS_CMD=cargo-zigbuild
install_target_if_missing() { local t="$1"; rustup target list --installed 2>/dev/null | grep -q "^$t$" || rustup target add "$t"; }
invoke_cargo_build() {
    local package="$1" target="$2" release="$3"; install_target_if_missing "$target"
    local target_os; target_os=$(get_target_os "$target")
    local args=(build -p "$package" --target "$target"); [ "$release" = true ] && args+=(--release)
    local tool=cargo; local tool_args=("${args[@]}")
    if [ "$target_os" != "$HOST_OS" ] && [ "$CROSS_CMD" = cross ]; then tool=cross
    elif [ "$target_os" != "$HOST_OS" ] && [ "$CROSS_CMD" = cargo-zigbuild ]; then tool=cargo; tool_args=(zigbuild "${args[@]:1}"); fi
    echo "  $tool ${tool_args[*]}" >&2; "$tool" "${tool_args[@]}"
}
get_built_binary_path() { local workspace="$1" bin="$2" target="$3" release="$4"; local profile=debug; [ "$release" = true ] && profile=release; local file="$bin"; [ "$(get_target_os "$target")" = windows ] && file="$bin.exe"; [ -n "${CARGO_TARGET_DIR:-}" ] && echo "$CARGO_TARGET_DIR/$target/$profile/$file" || echo "$workspace/target/$target/$profile/$file"; }

# Secure packaged builds always require the signing key.
if [ -z "${RBE_CONTAINER_SIGNING_PRIVATE_KEY:-}" ]; then
    echo "ERROR: RBE_CONTAINER_SIGNING_PRIVATE_KEY is required for packaged builds." >&2
    echo 'For a temporary local key: export RBE_CONTAINER_SIGNING_PRIVATE_KEY="$(openssl rand -hex 32)"' >&2
    exit 1
fi

DIST_ROOT="$REPO_ROOT/dist"; ENGINE_DIR="$REPO_ROOT/engine"; CONTAINER_DIR="$REPO_ROOT/container-runtime"
for target in "${targets[@]}"; do
    echo ""; echo "=== Building for $target ===" >&2
    out_dir="$DIST_ROOT/$target"; dep_dir="$out_dir/dep"; mkdir -p "$dep_dir"

    echo "-- container-bin ($target) --" >&2
    (cd "$CONTAINER_DIR" && invoke_cargo_build container-bin "$target" "$RELEASE")
    container_bin_path=$(get_built_binary_path "$CONTAINER_DIR" container-bin "$target" "$RELEASE")
    [ -f "$container_bin_path" ] || { echo "ERROR: container artifact missing: $container_bin_path" >&2; exit 1; }
    container_dest="$dep_dir/container"; [ "$(get_target_os "$target")" = windows ] && container_dest="$container_dest.exe"
    cp "$container_bin_path" "$container_dest"

    echo "-- backend ($target) --" >&2
    export RBE_CONTAINER_BIN_PATH="$container_bin_path"
    export RBE_BUILD_ID="${RBE_BUILD_ID:-$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown-build)}"
    (cd "$ENGINE_DIR" && invoke_cargo_build backend "$target" "$RELEASE")
    unset RBE_CONTAINER_BIN_PATH
    backend_path=$(get_built_binary_path "$ENGINE_DIR" backend "$target" "$RELEASE")
    [ -f "$backend_path" ] || { echo "ERROR: backend artifact missing: $backend_path" >&2; exit 1; }
    cp "$backend_path" "$out_dir/"

    [ -f "$ENGINE_DIR/settings.json" ] && cp "$ENGINE_DIR/settings.json" "$out_dir/" 2>/dev/null || true
    if [ "$DEV_CONTENT" = true ]; then [ -d "$REPO_ROOT/api" ] && cp -r "$REPO_ROOT/api" "$out_dir/"; [ -d "$REPO_ROOT/module" ] && cp -r "$REPO_ROOT/module" "$out_dir/"; else mkdir -p "$out_dir/api" "$out_dir/module"; fi
    if [ "$(get_target_os "$target")" = linux ]; then printf '%s\n' '#!/bin/sh' 'set -e' 'DIR="$(cd "$(dirname "$0")" && pwd)"' 'exec "$DIR/backend" "$@"' > "$out_dir/launch.sh"; chmod +x "$out_dir/launch.sh"; fi
    echo "  -> $out_dir" >&2
done

echo "Done. Output in $DIST_ROOT" >&2
