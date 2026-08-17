#!/bin/bash
# RBE build entrypoint.
# build-core.sh performs the complete secure build/package flow:
#   container-bin -> SHA-256/build-id/target + Ed25519 binding in backend.exe
#   -> dist/<target>/dep/container(.exe)
# The backend refuses to start without the exact verified dependency.
set -e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ensure_container_signing_key() {
    if [ -n "${RBE_CONTAINER_SIGNING_PRIVATE_KEY:-}" ]; then
        return 0
    fi

    local key_dir="${RBE_CONFIG_HOME:-${HOME:-$REPO_ROOT}/.rbe}"
    local key_path="$key_dir/container-signing.key"

    if [ -f "$key_path" ]; then
        local existing
        existing="$(tr -d '[:space:]' < "$key_path")"
        if [[ "$existing" =~ ^[0-9a-fA-F]{64}$ ]]; then
            export RBE_CONTAINER_SIGNING_PRIVATE_KEY="$existing"
            echo "Using existing local RBE container signing key: $key_path" >&2
            return 0
        fi
        echo "WARNING: local RBE container signing key is invalid; regenerating it." >&2
        rm -f "$key_path"
    fi

    if ! command -v openssl >/dev/null 2>&1; then
        echo "ERROR: OpenSSL is required to generate the local RBE container signing key." >&2
        echo "Install OpenSSL or set RBE_CONTAINER_SIGNING_PRIVATE_KEY explicitly." >&2
        exit 1
    fi

    mkdir -p "$key_dir"
    local key
    key="$(openssl rand -hex 32)"
    printf '%s\n' "$key" > "$key_path"
    chmod 600 "$key_path" 2>/dev/null || true
    export RBE_CONTAINER_SIGNING_PRIVATE_KEY="$key"
    echo "Generated and saved local RBE container signing key: $key_path" >&2
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
