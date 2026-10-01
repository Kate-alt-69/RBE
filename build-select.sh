#!/bin/bash
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENGINE_DIR="$REPO_ROOT/engine"
CONTAINER_DIR="$REPO_ROOT/container-runtime"
SDK_SOURCE_DIR="$REPO_ROOT/sdk"
DIST_ROOT="$REPO_ROOT/dist"
BUILD_SDK=false; ONLY=""; RELEASE=true; BUILD_WIN=false; BUILD_LINUX=false; BUILD_MACOS=false; BUILD_ALL=false; MUSL=false; CUSTOM_TARGET=""
ARCHES=()

ensure_shared_cargo_target() {
  local cache_root=""
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    case "$CARGO_TARGET_DIR" in
      /*) ;;
      *) CARGO_TARGET_DIR="$REPO_ROOT/$CARGO_TARGET_DIR" ;;
    esac
  else
    if [ -n "${RBE_BUILD_CACHE_DIR:-}" ]; then
      cache_root="$RBE_BUILD_CACHE_DIR"
    elif [ -n "${XDG_CACHE_HOME:-}" ]; then
      cache_root="$XDG_CACHE_HOME/rbe-build"
    else
      cache_root="$REPO_ROOT/.cache/rbe-build"
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
}

for arg in "$@"; do
  case "$arg" in
    --build-sdk) BUILD_SDK=true ;;
    --only-*) [ -z "$ONLY" ] || { echo 'ERROR: only one --only-<binary> selector is allowed' >&2; exit 2; }; ONLY="${arg#--only-}" ;;
    --build-win|--build-win10|--build-win11|--build-windows) BUILD_WIN=true ;;
    --build-linux) BUILD_LINUX=true ;;
    --build-macos) BUILD_MACOS=true ;;
    --build-all) BUILD_ALL=true ;;
    --musl) MUSL=true ;;
    --debug) RELEASE=false ;;
    --arch-x64|--architect-x64|--achitect-x64) ARCHES+=(x64) ;;
    --arch-x86|--architect-x86|--achitext-x86) ARCHES+=(x86) ;;
    --arch-arm|--arch-arm64) ARCHES+=(arm64) ;;
    --arch-armv7) ARCHES+=(armv7) ;;
    --target=*) CUSTOM_TARGET="${arg#--target=}" ;;
    --no-embed|--dev-content) ;;
    --help|-help|-h|-?) cat <<'EOF'
RBE selective/SDK builder
  ./build.sh --build-sdk [--only-backend|--only-rpx] [platform/arch flags]
  ./build.sh --only-<binary> [platform/arch flags]
Known binaries: backend, service, cloud-node, container, rpx.
A full --build-sdk bundle includes Rust, JavaScript/TypeScript, and Python SDK bindings.
EOF
      exit 0 ;;
    *) echo "WARNING: build-select.sh: ignoring unrecognized argument '$arg'" >&2 ;;
  esac
done
$BUILD_SDK || [ -n "$ONLY" ] || { echo 'ERROR: selective builder requires --build-sdk and/or --only-<binary>' >&2; exit 2; }

ensure_shared_cargo_target

case "$(uname -s)" in
  Linux*) HOST_OS=linux ;;
  Darwin*) HOST_OS=macos ;;
  MINGW*|MSYS*|CYGWIN*) HOST_OS=windows ;;
  *) HOST_OS=linux ;;
esac

case "$(uname -m)" in
  x86_64|amd64) HOST_ARCH=x64 ;;
  i?86) HOST_ARCH=x86 ;;
  aarch64|arm64) HOST_ARCH=arm64 ;;
  armv7*) HOST_ARCH=armv7 ;;
  *) HOST_ARCH=x64 ;;
esac

[ ${#ARCHES[@]} -gt 0 ] || ARCHES=("$HOST_ARCH")

resolve_target() {
  local os="$1"
  local arch="$2"
  local musl="$3"
  case "$os:$arch" in
    windows:x64) echo x86_64-pc-windows-msvc ;;
    windows:x86) echo i686-pc-windows-msvc ;;
    windows:arm64) echo aarch64-pc-windows-msvc ;;
    linux:x64)
      if [ "$musl" = true ]; then echo x86_64-unknown-linux-musl; else echo x86_64-unknown-linux-gnu; fi
      ;;
    linux:x86)
      if [ "$musl" = true ]; then echo i686-unknown-linux-musl; else echo i686-unknown-linux-gnu; fi
      ;;
    linux:arm64)
      if [ "$musl" = true ]; then echo aarch64-unknown-linux-musl; else echo aarch64-unknown-linux-gnu; fi
      ;;
    linux:armv7)
      if [ "$musl" = true ]; then echo armv7-unknown-linux-musleabihf; else echo armv7-unknown-linux-gnueabihf; fi
      ;;
    macos:x64) echo x86_64-apple-darwin ;;
    macos:arm64) echo aarch64-apple-darwin ;;
    *)
      echo "ERROR: unsupported target $os/$arch" >&2
      return 1
      ;;
  esac
}

target_os() {
  case "$1" in
    *windows*) echo windows ;;
    *darwin*) echo macos ;;
    *) echo linux ;;
  esac
}

targets=()
if [ -n "$CUSTOM_TARGET" ]; then
  targets+=("$CUSTOM_TARGET")
elif $BUILD_ALL; then
  targets+=(x86_64-pc-windows-msvc x86_64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-gnu aarch64-unknown-linux-musl)
  [ "$HOST_OS" = macos ] && targets+=(x86_64-apple-darwin aarch64-apple-darwin)
else
  if $BUILD_WIN; then
    for a in "${ARCHES[@]}"; do targets+=("$(resolve_target windows "$a" false)"); done
  fi
  if $BUILD_LINUX; then
    for a in "${ARCHES[@]}"; do targets+=("$(resolve_target linux "$a" "$MUSL")"); done
  fi
  if $BUILD_MACOS; then
    for a in "${ARCHES[@]}"; do targets+=("$(resolve_target macos "$a" "$MUSL")"); done
  fi
  if ! $BUILD_WIN && ! $BUILD_LINUX && ! $BUILD_MACOS; then
    for a in "${ARCHES[@]}"; do targets+=("$(resolve_target "$HOST_OS" "$a" "$MUSL")"); done
  fi
fi

profile=debug
$RELEASE && profile=release

cargo_build() {
  local cwd="$1"
  local target="$2"
  shift 2
  rustup target list --installed | grep -qx "$target" || rustup target add "$target"
  local tool=cargo
  if [ "$(target_os "$target")" != "$HOST_OS" ] && command -v cross >/dev/null 2>&1; then
    tool=cross
  elif [ "$(target_os "$target")" != "$HOST_OS" ]; then
    echo "WARNING: cross-OS target $target requested without cross; linker may fail" >&2
  fi
  (cd "$cwd" && "$tool" "$@")
}

built_binary_path() {
  local workspace="$1"
  local target="$2"
  local profile="$3"
  local binary="$4"
  local root="$workspace/target"
  [ -n "${CARGO_TARGET_DIR:-}" ] && root="$CARGO_TARGET_DIR"
  local path="$root/$target/$profile/$binary"
  [ "$(target_os "$target")" = windows ] && path="$path.exe"
  printf '%s\n' "$path"
}

copy_bin() {
  local source="$1"
  local base="$2"
  local target="$3"
  local dest="$base"
  [ "$(target_os "$target")" = windows ] && dest="$dest.exe"
  cp "$source" "$dest"
  echo "  -> $dest" >&2
}

copy_clean_tree() {
  local source="$1"
  local dest="$2"
  [ -d "$source" ] || { echo "ERROR: SDK binding source is missing: $source" >&2; exit 2; }
  rm -rf "$dest"
  mkdir -p "$dest"
  cp -R "$source"/. "$dest"/
  rm -rf "$dest/target" "$dest/node_modules" "$dest/__pycache__" "$dest/.pytest_cache"
  find "$dest" -type d -name __pycache__ -prune -exec rm -rf {} + 2>/dev/null || true
}

copy_sdk_bindings() {
  local out="$1"
  local bindings="$1/bindings"
  mkdir -p "$bindings"
  copy_clean_tree "$SDK_SOURCE_DIR/rbe-sdk" "$bindings/rust"
  copy_clean_tree "$SDK_SOURCE_DIR/js" "$bindings/javascript"
  copy_clean_tree "$SDK_SOURCE_DIR/js" "$bindings/typescript"
  copy_clean_tree "$SDK_SOURCE_DIR/python" "$bindings/python"
  echo "  -> $bindings" >&2
}

for target in "${targets[@]}"; do
  [[ "$target" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "ERROR: unsafe target '$target'" >&2; exit 2; }
  echo
  echo "=== Selective build for $target ===" >&2
  rel=()
  $RELEASE && rel=(--release)
  if $BUILD_SDK; then
    case "$ONLY" in
      ''|backend|rpx|sdk-backend) ;;
      *) echo "ERROR: --build-sdk only supports --only-backend or --only-rpx" >&2; exit 2 ;;
    esac
    out="$DIST_ROOT/$target/sdk"
    mkdir -p "$out"
    if [ -z "$ONLY" ] || [ "$ONLY" = backend ] || [ "$ONLY" = sdk-backend ]; then
      cargo_build "$REPO_ROOT" "$target" build --manifest-path "$ENGINE_DIR/crates/sdk-backend/Cargo.toml" --bin sdk-backend --target "$target" "${rel[@]}"
      p="$(built_binary_path "$ENGINE_DIR/crates/sdk-backend" "$target" "$profile" sdk-backend)"
      copy_bin "$p" "$out/backend" "$target"
    fi
    if [ -z "$ONLY" ] || [ "$ONLY" = rpx ]; then
      cargo_build "$REPO_ROOT" "$target" build --manifest-path "$ENGINE_DIR/crates/rpx/Cargo.toml" --bin rpx --target "$target" "${rel[@]}"
      p="$(built_binary_path "$ENGINE_DIR/crates/rpx" "$target" "$profile" rpx)"
      copy_bin "$p" "$out/rpx" "$target"
    fi
    if [ -z "$ONLY" ]; then
      echo '-- Language SDK bindings --' >&2
      copy_sdk_bindings "$out"
    fi
    has_backend=false
    has_rpx=false
    has_bindings=false
    if [ -z "$ONLY" ] || [ "$ONLY" = backend ] || [ "$ONLY" = sdk-backend ]; then has_backend=true; fi
    if [ -z "$ONLY" ] || [ "$ONLY" = rpx ]; then has_rpx=true; fi
    if [ -z "$ONLY" ]; then has_bindings=true; fi
    printf '{"format":1,"target":"%s","profile":"%s","sdk_backend":%s,"rpx":%s,"bindings":%s}\n' "$target" "$profile" "$has_backend" "$has_rpx" "$has_bindings" > "$out/sdk-build.json"
    continue
  fi

  out="$DIST_ROOT/$target"
  mkdir -p "$out"
  case "$ONLY" in
    backend|service)
      cargo_build "$ENGINE_DIR" "$target" build -p backend --bin "$ONLY" --target "$target" "${rel[@]}"
      p="$(built_binary_path "$ENGINE_DIR" "$target" "$profile" "$ONLY")"
      copy_bin "$p" "$out/$ONLY" "$target"
      ;;
    cloud-node|cloud_node)
      cargo_build "$ENGINE_DIR" "$target" build -p cloud-node --bin cloud_node --target "$target" "${rel[@]}"
      p="$(built_binary_path "$ENGINE_DIR" "$target" "$profile" cloud_node)"
      copy_bin "$p" "$out/cloud_node" "$target"
      ;;
    container|container-bin|container_bin)
      cargo_build "$CONTAINER_DIR" "$target" build -p container-bin --bin container-bin --target "$target" "${rel[@]}"
      p="$(built_binary_path "$CONTAINER_DIR" "$target" "$profile" container-bin)"
      copy_bin "$p" "$out/container" "$target"
      ;;
    rpx)
      cargo_build "$REPO_ROOT" "$target" build --manifest-path "$ENGINE_DIR/crates/rpx/Cargo.toml" --bin rpx --target "$target" "${rel[@]}"
      p="$(built_binary_path "$ENGINE_DIR/crates/rpx" "$target" "$profile" rpx)"
      copy_bin "$p" "$out/rpx" "$target"
      ;;
    *)
      cargo_build "$ENGINE_DIR" "$target" build -p "$ONLY" --bin "$ONLY" --target "$target" "${rel[@]}"
      p="$(built_binary_path "$ENGINE_DIR" "$target" "$profile" "$ONLY")"
      copy_bin "$p" "$out/$ONLY" "$target"
      ;;
  esac
done

echo "Done. Output in $DIST_ROOT" >&2
