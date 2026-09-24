#!/bin/bash
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENGINE_DIR="$REPO_ROOT/engine"
CONTAINER_DIR="$REPO_ROOT/container-runtime"
DIST_ROOT="$REPO_ROOT/dist"
BUILD_SDK=false; ONLY=""; RELEASE=true; BUILD_WIN=false; BUILD_LINUX=false; BUILD_MACOS=false; BUILD_ALL=false; MUSL=false; CUSTOM_TARGET=""
ARCHES=()
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
EOF
      exit 0 ;;
    *) echo "WARNING: build-select.sh: ignoring unrecognized argument '$arg'" >&2 ;;
  esac
done
$BUILD_SDK || [ -n "$ONLY" ] || { echo 'ERROR: selective builder requires --build-sdk and/or --only-<binary>' >&2; exit 2; }
HOST_OS=$(case "$(uname -s)" in Linux*) echo linux;; Darwin*) echo macos;; MINGW*|MSYS*|CYGWIN*) echo windows;; *) echo linux;; esac)
HOST_ARCH=$(case "$(uname -m)" in x86_64|amd64) echo x64;; i?86) echo x86;; aarch64|arm64) echo arm64;; armv7*) echo armv7;; *) echo x64;; esac)
[ ${#ARCHES[@]} -gt 0 ] || ARCHES=("$HOST_ARCH")
resolve_target() { local os="$1" arch="$2" musl="$3"; case "$os:$arch" in windows:x64) echo x86_64-pc-windows-msvc;; windows:x86) echo i686-pc-windows-msvc;; windows:arm64) echo aarch64-pc-windows-msvc;; linux:x64) [ "$musl" = true ] && echo x86_64-unknown-linux-musl || echo x86_64-unknown-linux-gnu;; linux:x86) [ "$musl" = true ] && echo i686-unknown-linux-musl || echo i686-unknown-linux-gnu;; linux:arm64) [ "$musl" = true ] && echo aarch64-unknown-linux-musl || echo aarch64-unknown-linux-gnu;; linux:armv7) [ "$musl" = true ] && echo armv7-unknown-linux-musleabihf || echo armv7-unknown-linux-gnueabihf;; macos:x64) echo x86_64-apple-darwin;; macos:arm64) echo aarch64-apple-darwin;; *) echo "ERROR: unsupported target $os/$arch" >&2; return 1;; esac; }
target_os() { case "$1" in *windows*) echo windows;; *darwin*) echo macos;; *) echo linux;; esac; }
targets=()
if [ -n "$CUSTOM_TARGET" ]; then targets+=("$CUSTOM_TARGET")
elif $BUILD_ALL; then targets+=(x86_64-pc-windows-msvc x86_64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-gnu aarch64-unknown-linux-musl); [ "$HOST_OS" = macos ] && targets+=(x86_64-apple-darwin aarch64-apple-darwin)
else
  $BUILD_WIN && for a in "${ARCHES[@]}"; do targets+=("$(resolve_target windows "$a" false)"); done
  $BUILD_LINUX && for a in "${ARCHES[@]}"; do targets+=("$(resolve_target linux "$a" "$MUSL")"); done
  $BUILD_MACOS && for a in "${ARCHES[@]}"; do targets+=("$(resolve_target macos "$a" false)"); done
  if ! $BUILD_WIN && ! $BUILD_LINUX && ! $BUILD_MACOS; then for a in "${ARCHES[@]}"; do targets+=("$(resolve_target "$HOST_OS" "$a" "$MUSL")"); done; fi
fi
profile=debug; $RELEASE && profile=release
cargo_build() { local cwd="$1" target="$2"; shift 2; rustup target list --installed | grep -qx "$target" || rustup target add "$target"; local tool=cargo; if [ "$(target_os "$target")" != "$HOST_OS" ] && command -v cross >/dev/null 2>&1; then tool=cross; elif [ "$(target_os "$target")" != "$HOST_OS" ]; then echo "WARNING: cross-OS target $target requested without cross; linker may fail" >&2; fi; (cd "$cwd" && "$tool" "$@"); }
copy_bin() { local source="$1" base="$2" target="$3"; local dest="$base"; [ "$(target_os "$target")" = windows ] && dest="$dest.exe"; cp "$source" "$dest"; echo "  -> $dest" >&2; }
for target in "${targets[@]}"; do
  [[ "$target" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "ERROR: unsafe target '$target'" >&2; exit 2; }
  echo; echo "=== Selective build for $target ===" >&2
  rel=(); $RELEASE && rel=(--release)
  if $BUILD_SDK; then
    case "$ONLY" in ''|backend|rpx|sdk-backend) ;; *) echo "ERROR: --build-sdk only supports --only-backend or --only-rpx" >&2; exit 2;; esac
    out="$DIST_ROOT/$target/sdk"; mkdir -p "$out"
    if [ -z "$ONLY" ] || [ "$ONLY" = backend ] || [ "$ONLY" = sdk-backend ]; then cargo_build "$REPO_ROOT" "$target" build --manifest-path "$ENGINE_DIR/crates/sdk-backend/Cargo.toml" --bin sdk-backend --target "$target" "${rel[@]}"; p="$ENGINE_DIR/crates/sdk-backend/target/$target/$profile/sdk-backend"; [ "$(target_os "$target")" = windows ] && p="$p.exe"; copy_bin "$p" "$out/backend" "$target"; fi
    if [ -z "$ONLY" ] || [ "$ONLY" = rpx ]; then cargo_build "$REPO_ROOT" "$target" build --manifest-path "$ENGINE_DIR/crates/rpx/Cargo.toml" --bin rpx --target "$target" "${rel[@]}"; p="$ENGINE_DIR/crates/rpx/target/$target/$profile/rpx"; [ "$(target_os "$target")" = windows ] && p="$p.exe"; copy_bin "$p" "$out/rpx" "$target"; fi
    printf '{"format":1,"target":"%s","profile":"%s"}\n' "$target" "$profile" > "$out/sdk-build.json"
    continue
  fi
  out="$DIST_ROOT/$target"; mkdir -p "$out"
  case "$ONLY" in
    backend|service) cargo_build "$ENGINE_DIR" "$target" build -p backend --bin "$ONLY" --target "$target" "${rel[@]}"; p="$ENGINE_DIR/target/$target/$profile/$ONLY"; [ "$(target_os "$target")" = windows ] && p="$p.exe"; copy_bin "$p" "$out/$ONLY" "$target" ;;
    cloud-node|cloud_node) cargo_build "$ENGINE_DIR" "$target" build -p cloud-node --bin cloud_node --target "$target" "${rel[@]}"; p="$ENGINE_DIR/target/$target/$profile/cloud_node"; [ "$(target_os "$target")" = windows ] && p="$p.exe"; copy_bin "$p" "$out/cloud_node" "$target" ;;
    container|container-bin|container_bin) cargo_build "$CONTAINER_DIR" "$target" build -p container-bin --bin container-bin --target "$target" "${rel[@]}"; p="$CONTAINER_DIR/target/$target/$profile/container-bin"; [ "$(target_os "$target")" = windows ] && p="$p.exe"; copy_bin "$p" "$out/container" "$target" ;;
    rpx) cargo_build "$REPO_ROOT" "$target" build --manifest-path "$ENGINE_DIR/crates/rpx/Cargo.toml" --bin rpx --target "$target" "${rel[@]}"; p="$ENGINE_DIR/crates/rpx/target/$target/$profile/rpx"; [ "$(target_os "$target")" = windows ] && p="$p.exe"; copy_bin "$p" "$out/rpx" "$target" ;;
    *) cargo_build "$ENGINE_DIR" "$target" build -p "$ONLY" --bin "$ONLY" --target "$target" "${rel[@]}"; p="$ENGINE_DIR/target/$target/$profile/$ONLY"; [ "$(target_os "$target")" = windows ] && p="$p.exe"; copy_bin "$p" "$out/$ONLY" "$target" ;;
  esac
done
echo "Done. Output in $DIST_ROOT" >&2
