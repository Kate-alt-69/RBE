#!/bin/bash
# RBE build entrypoint.
#
# The full build implementation lives in build-core.sh. On Linux, Windows/MSVC
# cross-builds use cargo-xwin instead of cargo-zigbuild so the Windows SDK/CRT
# headers and libraries are available to C dependencies such as zstd-sys.
#
# After the core build succeeds, this wrapper packages the exact container-bin
# artifact that was embedded into backend.exe under dist/<target>/dep/.
set -e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ "$(uname -s)" == Linux* ]]; then
    if [[ " ${*} " == *" --build-win "* || " ${*} " == *" --build-win10 "* || " ${*} " == *" --build-win11 "* || " ${*} " == *" --build-windows "* || " ${*} " == *" --build-all "* || " ${*} " == *"--target=x86_64-pc-windows-msvc"* || " ${*} " == *"--target=i686-pc-windows-msvc"* || " ${*} " == *"--target=aarch64-pc-windows-msvc"* || " ${*} " == *"--target=thumbv7-pc-windows-msvc"* ]]; then
        if ! command -v cargo-xwin >/dev/null 2>&1; then
            echo "Installing cargo-xwin for Linux -> Windows/MSVC builds..." >&2
            cargo install cargo-xwin --locked
        fi

        REAL_CARGO="$(command -v cargo)"
        SHIM_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rbe-build.XXXXXX")"
        cleanup() { rm -rf "$SHIM_DIR"; }
        trap cleanup EXIT INT TERM

        cat > "$SHIM_DIR/cargo" <<EOF
#!/bin/sh
if [ "\$1" = "zigbuild" ]; then
    shift
    exec "$REAL_CARGO" xwin build "\$@"
fi
exec "$REAL_CARGO" "\$@"
EOF
        chmod +x "$SHIM_DIR/cargo"

        cat > "$SHIM_DIR/cargo-zigbuild" <<'EOF'
#!/bin/sh
exit 0
EOF
        chmod +x "$SHIM_DIR/cargo-zigbuild"

        export PATH="$SHIM_DIR:$PATH"
    fi
fi

"$REPO_ROOT/build-core.sh" "$@"

PROFILE="release"
if [[ " ${*} " == *" --debug "* ]]; then
    PROFILE="debug"
fi

CARGO_TARGET_ROOT="${CARGO_TARGET_DIR:-$REPO_ROOT/container-runtime/target}"
PACKAGED_ANY=false

for OUT_DIR in "$REPO_ROOT"/dist/*; do
    [ -d "$OUT_DIR" ] || continue
    TARGET="$(basename "$OUT_DIR")"
    BACKEND="$OUT_DIR/backend"
    [ -f "$OUT_DIR/backend.exe" ] && BACKEND="$OUT_DIR/backend.exe"
    [ -f "$BACKEND" ] || continue

    case "$TARGET" in
        *windows*) CONTAINER_NAME="container.exe" ;;
        *) CONTAINER_NAME="container" ;;
    esac

    CONTAINER_SOURCE="$CARGO_TARGET_ROOT/$TARGET/$PROFILE/container-bin"
    [ "$CONTAINER_NAME" = "container.exe" ] && CONTAINER_SOURCE="$CARGO_TARGET_ROOT/$TARGET/$PROFILE/container-bin.exe"

    if [ ! -f "$CONTAINER_SOURCE" ]; then
        echo "ERROR: packaged backend exists for $TARGET but matching container binary is missing: $CONTAINER_SOURCE" >&2
        exit 1
    fi

    DEP_DIR="$OUT_DIR/dep"
    mkdir -p "$DEP_DIR"
    cp "$CONTAINER_SOURCE" "$DEP_DIR/$CONTAINER_NAME"
    PACKAGED_ANY=true
    echo "  -> $DEP_DIR/$CONTAINER_NAME" >&2
done

if [ "$PACKAGED_ANY" = false ]; then
    echo "ERROR: no backend target was produced; refusing to report a packaged build" >&2
    exit 1
fi

echo "Container dependencies packaged under dist/*/dep/" >&2
