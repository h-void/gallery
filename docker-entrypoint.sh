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
    for d in /media /media2 /media3 /media4 /media5 /media6 /media7 /media8 /media9; do
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
