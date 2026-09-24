#!/bin/bash
set -e
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for arg in "$@"; do
    case "$arg" in
        --build-sdk|--only-*) exec "$REPO_ROOT/build-select.sh" "$@" ;;
    esac
done

exec "$REPO_ROOT/build-release.sh" "$@"
