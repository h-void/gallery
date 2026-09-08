#!/bin/sh
# Auto-detect media library roots at container start: every non-empty
# /mediaN directory becomes one library root (label = directory name).
# Empty placeholder mounts from unset GALLERY_MEDIA_DIRN variables are
# skipped. Explicit PICTURES_ROOT / PICTURES_ROOT_LABELS from the
# environment always win (docker-compose.test.yml relies on this).
set -eu

if [ -z "${PICTURES_ROOT:-}" ]; then
    roots=""
    labels=""
    # Discover numeric slots without a fixed upper bound (/media10, /media100...).
    # The glob also matches /media2-backup; accept digits only after /media.
    for d in /media /media[0-9]*; do
        suffix=${d#/media}
        case "$suffix" in
            *[!0-9]*) continue ;;
        esac
        [ -d "$d" ] || continue
        if [ -n "$(ls -A "$d" 2>/dev/null)" ]; then
            label=${d#/}
            if [ -z "$roots" ]; then
                roots="$d"
                labels="$label"
            else
                roots="$roots,$d"
                labels="$labels,$label"
            fi
        fi
    done
    if [ -z "$roots" ]; then
        roots=/media
        labels=media
    fi
    export PICTURES_ROOT="$roots"
    export PICTURES_ROOT_REAL_PATHS="$roots"
    export PICTURES_ROOT_LABELS="$labels"
fi

exec gallery-accel "$@"
