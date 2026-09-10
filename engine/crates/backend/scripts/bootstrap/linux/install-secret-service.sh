#!/bin/sh
set -u

if [ "$(id -u)" -eq 0 ]; then
    ELEVATE='direct'
elif command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1; then
    ELEVATE='sudo'
else
    echo 'RBE_RESULT=NO_PRIVILEGE'
    exit 13
fi

as_root() {
    if [ "$ELEVATE" = 'sudo' ]; then
        sudo -n "$@"
    else
        "$@"
    fi
}

if command -v apt-get >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=apt'
    as_root env DEBIAN_FRONTEND=noninteractive apt-get update -qq || exit 20
    as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq dbus-daemon gnome-keyring || exit 20
elif command -v dnf >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=dnf'
    as_root dnf install -y dbus-daemon gnome-keyring || exit 20
elif command -v yum >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=yum'
    as_root yum install -y dbus-daemon gnome-keyring || exit 20
elif command -v pacman >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=pacman'
    as_root pacman -Sy --noconfirm dbus gnome-keyring || exit 20
elif command -v zypper >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=zypper'
    as_root zypper --non-interactive install dbus-1 gnome-keyring || exit 20
elif command -v apk >/dev/null 2>&1; then
    echo 'RBE_PACKAGE_MANAGER=apk'
    as_root apk add --no-cache dbus gnome-keyring || exit 20
else
    echo 'RBE_RESULT=NO_PACKAGE_MANAGER'
    exit 12
fi

echo 'RBE_RESULT=INSTALLED'
exit 0
