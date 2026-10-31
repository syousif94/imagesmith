#!/bin/sh
# Installed by ImageSmith: grows the root partition and filesystem to fill
# the disk on first boot, then disables itself.
set -eu
log() { echo "imagesmith-expand-rootfs: $*"; }

ROOT=$(findmnt -no SOURCE /)
NAME=$(basename "$ROOT")
PART=$(cat "/sys/class/block/$NAME/partition")
DISK=/dev/$(basename "$(readlink -f "/sys/class/block/$NAME/..")")
log "root $ROOT is partition $PART of $DISK"

if command -v growpart >/dev/null 2>&1; then
    growpart "$DISK" "$PART" || log "growpart: nothing to do"
elif command -v sfdisk >/dev/null 2>&1; then
    sfdisk --relocate gpt-bak-std "$DISK" 2>/dev/null || true
    echo ", +" | sfdisk --force --no-reread -N "$PART" "$DISK"
    partx -u --nr "$PART" "$DISK" 2>/dev/null || true
else
    log "neither growpart nor sfdisk is installed; not expanding"
    exit 0
fi
resize2fs "$ROOT"

systemctl disable imagesmith-expand-rootfs.service || true
rm -f /etc/systemd/system/imagesmith-expand-rootfs.service /usr/local/sbin/imagesmith-expand-rootfs
