#!/bin/bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELP_PATH="$REPO_ROOT/build-help.txt"

normalize_only_component() {
    local value
    value="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
    case "$value" in
        backend) echo backend ;;
        service) echo service ;;
        cloud-node|cloud_node) echo cloud-node ;;
        container|container-bin|container_bin) echo container ;;
        rpx) echo rpx ;;
        sdk-backend|sdk_backend) echo sdk-backend ;;
        *)
            echo "ERROR: unknown --only component '$1'. Run ./build.sh -h for the supported component list." >&2
            return 2
            ;;
    esac
}

normalized=()
only_seen=false
build_sdk=false
help_requested=false

for raw in "$@"; do
    arg="$raw"
    case "$arg" in
        -build-sdk) arg=--build-sdk ;;
        -only-*) arg="--only-${arg#-only-}" ;;
        --cloud-node-only|--build-cloud-node|-CloudNodeOnly) arg=--only-cloud-node ;;
    esac

    case "$arg" in
        --only-*)
            if $only_seen; then
                echo 'ERROR: only one --only-<component> selector may be used.' >&2
                exit 2
            fi
            only_seen=true
            component="$(normalize_only_component "${arg#--only-}")"
            normalized+=("--only-$component")
            ;;
        --build-sdk)
            build_sdk=true
            normalized+=("$arg")
            ;;
        --help|-help|-h|-\?)
            help_requested=true
            normalized+=("$arg")
            ;;
        *) normalized+=("$arg") ;;
    esac
done

if $help_requested; then
    if [ -f "$HELP_PATH" ]; then
        cat "$HELP_PATH"
    else
        echo 'RBE build help is missing. Expected build-help.txt beside build.sh.' >&2
    fi
    exit 0
fi

if $only_seen && ! $build_sdk; then
    for arg in "${normalized[@]}"; do
        if [ "$arg" = '--only-sdk-backend' ]; then
            echo 'ERROR: --only-sdk-backend is valid only together with --build-sdk.' >&2
            exit 2
        fi
    done
fi

if $build_sdk || $only_seen; then
    exec "$REPO_ROOT/build-select.sh" "${normalized[@]}"
fi

exec "$REPO_ROOT/build-release.sh" "${normalized[@]}"
