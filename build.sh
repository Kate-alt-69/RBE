#!/bin/bash
# RBE build entrypoint.
# build-core.sh performs the complete secure build/package flow.
set -e

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ensure_admin_verifier() {
    local rounds=120000
    local have_salt=false have_verifier=false have_rounds=false
    [ -n "${RBE_ADMIN_AUTH_SALT_HEX:-}" ] && have_salt=true
    [ -n "${RBE_ADMIN_AUTH_VERIFIER_HEX:-}" ] && have_verifier=true
    [ -n "${RBE_ADMIN_AUTH_ROUNDS:-}" ] && have_rounds=true

    if $have_salt || $have_verifier || $have_rounds; then
        if ! $have_salt || ! $have_verifier || ! $have_rounds; then
            echo "ERROR: RBE admin verifier variables must be supplied together." >&2
            exit 1
        fi
        [[ "$RBE_ADMIN_AUTH_SALT_HEX" =~ ^[0-9a-fA-F]{16}$ ]] || { echo "ERROR: RBE_ADMIN_AUTH_SALT_HEX must contain exactly 16 hexadecimal characters." >&2; exit 1; }
        [[ "$RBE_ADMIN_AUTH_VERIFIER_HEX" =~ ^[0-9a-fA-F]{64}$ ]] || { echo "ERROR: RBE_ADMIN_AUTH_VERIFIER_HEX must contain exactly 64 hexadecimal characters." >&2; exit 1; }
        [[ "$RBE_ADMIN_AUTH_ROUNDS" =~ ^[0-9]+$ ]] && [ "$RBE_ADMIN_AUTH_ROUNDS" -ge 10000 ] || { echo "ERROR: RBE_ADMIN_AUTH_ROUNDS must be an integer >= 10000." >&2; exit 1; }
        echo "Using externally supplied Control Room password verifier for this build." >&2
        return 0
    fi

    command -v openssl >/dev/null 2>&1 || { echo "ERROR: OpenSSL is required to derive the Control Room password verifier." >&2; exit 1; }

    local password="${RBE_ADMIN_PASSWORD:-}"
    unset RBE_ADMIN_PASSWORD || true
    if [ -z "$password" ]; then
        if [ ! -t 0 ]; then
            echo "ERROR: RBE_ADMIN_PASSWORD or a complete RBE_ADMIN_AUTH_* verifier is required for non-interactive packaged builds." >&2
            exit 1
        fi
        local first second
        printf '\nRBE Control Room authentication\n' >&2
        printf 'Set the password that will unlock this exact backend build.\n' >&2
        read -r -s -p "Admin password (12+ characters): " first
        printf '\n' >&2
        read -r -s -p "Confirm admin password: " second
        printf '\n' >&2
        if [ "$first" != "$second" ]; then
            echo "ERROR: admin passwords did not match." >&2
            unset first second
            exit 1
        fi
        password="$first"
        unset first second
    else
        echo "Consuming externally supplied Control Room admin password for this build." >&2
    fi

    if [ "${#password}" -lt 12 ]; then
        echo "ERROR: admin password must contain at least 12 characters." >&2
        unset password
        exit 1
    fi
    if [ "${#password}" -gt 1024 ]; then
        echo "ERROR: admin password is unreasonably large." >&2
        unset password
        exit 1
    fi

    local salt output verifier
    salt="$(openssl rand -hex 8)"
    output="$(printf '%s\n' "$password" | openssl enc -aes-256-cbc -pbkdf2 -iter "$rounds" -md sha256 -S "$salt" -pass stdin -P 2>/dev/null)"
    verifier="$(printf '%s\n' "$output" | awk -F= '$1 == "key" {print tolower($2); exit}')"
    unset password output
    [[ "$salt" =~ ^[0-9a-fA-F]{16}$ ]] || { echo "ERROR: failed to generate Control Room verifier salt." >&2; exit 1; }
    [[ "$verifier" =~ ^[0-9a-f]{64}$ ]] || { echo "ERROR: failed to derive Control Room password verifier." >&2; exit 1; }

    export RBE_ADMIN_AUTH_ROUNDS="$rounds"
    export RBE_ADMIN_AUTH_SALT_HEX="$(printf '%s' "$salt" | tr '[:upper:]' '[:lower:]')"
    export RBE_ADMIN_AUTH_VERIFIER_HEX="$verifier"
    unset salt verifier
    echo "Admin password accepted. Only a salted PBKDF2 verifier is passed into Cargo." >&2
}

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
    unset key
    echo "Generated ephemeral local RBE container signing key (not written to disk)." >&2
}

case " $* " in
    *" --help "*|*" -h "*|*" -? "*) ;;
    *) ensure_admin_verifier ;;
esac
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
