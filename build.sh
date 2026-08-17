#!/bin/bash
# RBE build entrypoint.
#
# The full build implementation lives in build-core.sh. On Linux, Windows/MSVC
# cross-builds use cargo-xwin instead of cargo-zigbuild so the Windows SDK/CRT
# headers and libraries are available to C dependencies such as zstd-sys.
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

        # The core script detects cargo-zigbuild by name. Provide a tiny marker
        # so its existing selection logic enters the cargo-zigbuild code path;
        # our cargo shim above transparently converts that subcommand to xwin.
        cat > "$SHIM_DIR/cargo-zigbuild" <<'EOF'
#!/bin/sh
exit 0
EOF
        chmod +x "$SHIM_DIR/cargo-zigbuild"

        export PATH="$SHIM_DIR:$PATH"
    fi
fi

exec "$REPO_ROOT/build-core.sh" "$@"
