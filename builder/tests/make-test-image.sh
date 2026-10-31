#!/usr/bin/env bash
# Creates a small GPT test image (boot + ext4 rootfs from the builder's own
# Debian userland) inside the store and writes it, zstd-compressed, to stdout.
#   container exec imagesmith-store bash -s < builder/tests/make-test-image.sh > test.img.zst
set -euo pipefail
W=/store/scratch
rm -rf "$W"; mkdir -p "$W/rootfs"
cd "$W"
tar -C / --exclude=./proc --exclude=./sys --exclude=./dev --exclude=./store --exclude=./tmp -cf - . | tar -C rootfs -xf -
mkdir -p rootfs/proc rootfs/sys rootfs/dev rootfs/tmp
chmod 1777 rootfs/tmp
echo test-base > rootfs/etc/hostname
cp rootfs/usr/bin/true rootfs/usr/bin/ping
setcap cap_net_raw+ep rootfs/usr/bin/ping

ROOT_MB=900
truncate -s $(( (1 + 64 + ROOT_MB + 1) ))M test.img
sfdisk -q test.img >&2 <<EOF
label: gpt
start=2048, size=131072, type=uefi, name=boot
start=133120, size=$((ROOT_MB * 2048)), type=linux, name=rootfs
EOF
mke2fs -q -F -t ext4 -O ^orphan_file -L rootfs -d rootfs part.ext4 ${ROOT_MB}M >&2
dd if=part.ext4 of=test.img bs=4M seek=$((133120 * 512)) oflag=seek_bytes conv=notrunc,sparse status=none
zstd -q -T0 -3 -c test.img
rm -rf "$W"
