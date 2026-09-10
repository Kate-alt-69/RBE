#!/bin/sh
set -u

has_secret_service() {
    if command -v gdbus >/dev/null 2>&1; then
        gdbus call --session \
            --dest org.freedesktop.secrets \
            --object-path /org/freedesktop/secrets \
            --method org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1 && return 0
    fi
    if command -v dbus-send >/dev/null 2>&1; then
        dbus-send --session --dest=org.freedesktop.DBus --type=method_call --print-reply \
            /org/freedesktop/DBus org.freedesktop.DBus.ListNames 2>/dev/null \
            | grep -q 'org.freedesktop.secrets' && return 0
    fi
    return 1
}

if [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] && has_secret_service; then
    echo 'RBE_RESULT=READY'
    exit 0
fi

if ! command -v dbus-daemon >/dev/null 2>&1; then
    echo 'RBE_RESULT=MISSING'
    echo 'RBE_MISSING=dbus-daemon'
    exit 10
fi

if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    address="$(dbus-daemon --session --fork --print-address 2>/dev/null | head -n 1)"
    if [ -z "$address" ]; then
        echo 'RBE_RESULT=FAILED'
        echo 'RBE_STAGE=session-dbus'
        exit 11
    fi
    DBUS_SESSION_BUS_ADDRESS="$address"
    export DBUS_SESSION_BUS_ADDRESS
    printf 'RBE_EXPORT_DBUS_SESSION_BUS_ADDRESS=%s\n' "$DBUS_SESSION_BUS_ADDRESS"
fi

if has_secret_service; then
    echo 'RBE_RESULT=READY'
    exit 0
fi

if ! command -v gnome-keyring-daemon >/dev/null 2>&1; then
    echo 'RBE_RESULT=MISSING'
    echo 'RBE_MISSING=secret-service-provider'
    exit 10
fi

keyring_output="$(gnome-keyring-daemon --start --components=secrets 2>/dev/null || true)"
printf '%s\n' "$keyring_output" | while IFS= read -r line; do
    case "$line" in
        GNOME_KEYRING_CONTROL=*) printf 'RBE_EXPORT_%s\n' "$line" ;;
        GNOME_KEYRING_PID=*) printf 'RBE_EXPORT_%s\n' "$line" ;;
    esac
done

# gnome-keyring may need a moment to claim org.freedesktop.secrets.
i=0
while [ "$i" -lt 20 ]; do
    if has_secret_service; then
        echo 'RBE_RESULT=READY'
        exit 0
    fi
    i=$((i + 1))
    sleep 0.05
 done

echo 'RBE_RESULT=FAILED'
echo 'RBE_STAGE=secret-service-start'
exit 11
