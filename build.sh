#!/bin/bash
# RBE build entrypoint.
# build-core.sh performs the complete secure build/package flow:
#   container-bin -> SHA-256/build-id/target + Ed25519 binding in backend.exe
#   -> dist/<target>/dep/container(.exe)
# The backend refuses to start without the exact verified dependency.
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

exec "$REPO_ROOT/build-core.sh" "$@"
