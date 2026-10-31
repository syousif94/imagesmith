#!/usr/bin/env bash
# Interactive login shell chrooted into a project's stage, with /proc, /sys
# and /dev mounted for the session.
#
#   shell.sh <project-id>
set -uo pipefail

id=${1:-}
[[ $id =~ ^[A-Za-z0-9._-]+$ && $id != . && $id != .. ]] || { echo "invalid project id" >&2; exit 2; }
STAGE=${IMAGESMITH_STORE:-/store}/projects/$id/stage
[[ -d $STAGE ]] || { echo "no stage for project $id: build or extract first" >&2; exit 1; }

mounted=()
cleanup() {
    local i
    for ((i = ${#mounted[@]} - 1; i >= 0; i--)); do umount -l "${mounted[i]}" 2>/dev/null; done
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 143' TERM

mnt() { # dir mount-args...
    local dir=$STAGE/$1; shift
    mountpoint -q "$dir" && return 0
    mkdir -p "$dir"
    mount "$@" "$dir" 2>/dev/null && mounted+=("$dir") || echo "warning: could not mount /${dir#"$STAGE"/}" >&2
}
mnt proc -t proc proc
mnt sys -t sysfs sysfs
mnt dev --bind /dev
mnt dev/pts --bind /dev/pts

chroot "$STAGE" /bin/bash -l
