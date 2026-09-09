#!/bin/bash
# RBE build entrypoint.
# build-core.sh performs the complete secure build/package flow:
#   container-bin -> SHA-256/build-id/target + Ed25519 binding in backend.exe
#   -> dist/<target>/dep/container(.exe)
# The backend refuses to start without the exact verified dependency.
set -e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

remove_legacy_container_signing_key() {
    local key_dir="${RBE_CONFIG_HOME:-${HOME:-$REPO_ROOT}/.rbe}"
    local key_path="$key_dir/container-signing.key"
    if [ ! -e "$key_path" ]; then
        return 0
    fi

    rm -f -- "$key_path"
    if [ -e "$key_path" ]; then
        echo "ERROR: failed to remove legacy local signing credential: $key_path" >&2
        exit 1
    fi
    echo "Removed legacy local signing credential: $key_path" >&2
}

ensure_container_signing_key() {
    if [ -n "${RBE_CONTAINER_SIGNING_PRIVATE_KEY:-}" ]; then
        if [[ ! "$RBE_CONTAINER_SIGNING_PRIVATE_KEY" =~ ^[0-9a-fA-F]{64}$ ]]; then
            echo "ERROR: RBE_CONTAINER_SIGNING_PRIVATE_KEY must contain exactly 64 hexadecimal characters." >&2
            exit 1
        fi
        remove_legacy_container_signing_key
        echo "Using externally supplied RBE container signing key." >&2
        return 0
    fi

    remove_legacy_container_signing_key

    if ! command -v openssl >/dev/null 2>&1; then
        echo "ERROR: OpenSSL is required to generate the ephemeral local RBE container signing key." >&2
        echo "Install OpenSSL or set RBE_CONTAINER_SIGNING_PRIVATE_KEY explicitly." >&2
        exit 1
    fi

    local key
    key="$(openssl rand -hex 32)"
    if [[ ! "$key" =~ ^[0-9a-fA-F]{64}$ ]]; then
        echo "ERROR: OpenSSL failed to generate a valid 32-byte container signing key." >&2
        exit 1
    fi
    export RBE_CONTAINER_SIGNING_PRIVATE_KEY="$key"
    echo "Generated ephemeral local RBE container signing key (not written to disk)." >&2
}

ensure_container_signing_key

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
