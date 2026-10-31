#!/usr/bin/env bash
# ImageSmith builder. Runs inside the imagesmith-store container.
#
#   build.sh --project-id <id> --build-id <id> [--extract-only]
#       stdin:  "IMAGESMITH-INPUT 1 <len>\n", <len> bytes of tar
#               (recipe.json, inputs/files/N, overlay/), then one JSON line
#               {"passwords":{...}}
#       stdout: log text plus "@@{json}" events (see imagesmith-core events.rs)
#   build.sh --probe <sha256>
#       prints one JSON object describing the base image
#
# Nothing is mounted from images: the rootfs is extracted with debugfs and
# rebuilt with mke2fs -d. Every Linux file stays on the store volume.

set -Eeuo pipefail
shopt -s nullglob

STORE=${IMAGESMITH_STORE:-/store}
LIB=${IMAGESMITH_LIB:-/usr/local/lib/imagesmith}
export LC_ALL=C.UTF-8

# Events go to fd 3 (the original stdout), so helpers running inside
# pipelines never mix them into data streams.
exec 3>&1

# Own process group so Cancel can signal every child: kill -TERM -<pid>.
if [[ -z ${IMAGESMITH_SID:-} ]]; then
    export IMAGESMITH_SID=1
    exec setsid --wait "$0" "$@"
fi

emit() { printf '@@%s\n' "$1" >&3; }
now_ms() { date +%s%3N; }
log() { printf '%s\n' "$*"; }

CUR_STAGE=prepare
ERRORED=0
CANCELLED=0

ev_stage() { # id title index [parent]
    CUR_STAGE=$1
    emit "$(jq -cn --arg id "$1" --arg t "$2" --argjson i "$3" --arg p "${4:-}" \
        '{type:"stage",id:$id,title:$t,index:$i,total:10} + (if $p == "" then {} else {parent:$p} end)')"
}
ev_progress() { # done total unit
    emit "$(jq -cn --arg s "$CUR_STAGE" --argjson d "$1" --argjson t "${2:-null}" --arg u "$3" \
        '{type:"progress",stage:$s,done:$d,total:$t,unit:$u}')"
}
warn() { # message [hint]
    emit "$(jq -cn --arg m "$1" --arg h "${2:-}" '{type:"warn",message:$m,hint:(if $h == "" then null else $h end)}')"
}
fail() { # message [hint]
    ERRORED=1
    emit "$(jq -cn --arg s "$CUR_STAGE" --arg m "$1" --arg h "${2:-}" \
        '{type:"error",stage:$s,message:$m,hint:(if $h == "" then null else $h end)}')"
    exit 1
}
artifact() { # kind file
    emit "$(jq -cn --arg k "$1" --arg p "$(basename "$2")" --argjson b "$(stat -c %s "$2")" \
        '{type:"artifact",kind:$k,path:$p,bytes:$b}')"
}
on_err() { # rc command line
    # ERR is inherited by command substitutions (set -E); only the main shell reports.
    [[ $BASHPID == "$$" ]] || return 0
    [[ $ERRORED == 1 || $CANCELLED == 1 ]] && return
    ERRORED=1
    emit "$(jq -cn --arg s "$CUR_STAGE" --arg m "command failed (exit $1): $2" --arg l "$3" \
        '{type:"error",stage:$s,message:$m,hint:("build.sh line " + $l)}')"
}

safe_id() { [[ $1 =~ ^[A-Za-z0-9._-]+$ && $1 != . && $1 != .. ]]; }

# dd with byte progress events: ddp <total-bytes> <dd args...>
ddp() {
    local total=$1; shift
    dd bs=4M status=progress "$@" 2> >(progress_filter "$total")
}
progress_filter() {
    stdbuf -o0 tr '\r' '\n' | while IFS= read -r l; do
        if [[ $l =~ ^([0-9]+)\ bytes ]]; then
            ev_progress "${BASH_REMATCH[1]}" "$1" bytes
        elif [[ -n $l && ! $l =~ records\ (in|out)$ ]]; then
            printf '%s\n' "$l" >&2
        fi
    done
}

# ---------------------------------------------------------------- partitions

# Sets LABEL_TYPE SECTOR PARTS_JSON for an image file.
read_table() {
    local img=$1
    PARTS_JSON=$(sfdisk --json "$img" | jq -c --arg base "$img" '.partitiontable
        | {label, sectorsize: (.sectorsize // 512),
           parts: [.partitions[] | {number: (.node | ltrimstr($base) | tonumber), start, size, type}]}')
    LABEL_TYPE=$(jq -r .label <<<"$PARTS_JSON")
    SECTOR=$(jq -r .sectorsize <<<"$PARTS_JSON")
}
part_field() { jq -r --argjson n "$1" ".parts[] | select(.number == \$n) | .$2" <<<"$PARTS_JSON"; }
last_part() { jq -r '.parts | max_by(.start) | .number' <<<"$PARTS_JSON"; }

# Filesystem facts at a byte offset of an image, without copying it.
blkid_at() { blkid -p -O "$2" -o export "$1" 2>/dev/null || true; }

# e2fsprogs read-only access to a filesystem inside an image at an offset.
dbg() { debugfs -R "$2" "$1" 2>/dev/null; }

# ---------------------------------------------------------------- probe

CAP_CANDIDATES=(
    /usr/bin/ping /usr/bin/ping4 /usr/bin/ping6 /usr/bin/arping /usr/bin/clockdiff
    /usr/bin/traceroute6.iputils /usr/bin/mtr-packet /usr/bin/fping /usr/bin/newuidmap
    /usr/bin/newgidmap /usr/bin/dumpcap /usr/sbin/arping /usr/bin/gnome-keyring-daemon
    /usr/lib/aarch64-linux-gnu/gstreamer1.0/gstreamer-1.0/gst-ptp-helper
    /usr/lib/x86_64-linux-gnu/gstreamer1.0/gstreamer-1.0/gst-ptp-helper
    /usr/lib/arm-linux-gnueabihf/gstreamer1.0/gstreamer-1.0/gst-ptp-helper
)

# vfs_cap_data (security.capability) -> "cap_net_raw+ep"
decode_caps() {
    local hex; hex=$(od -An -tx1 -v "$1" | tr -d ' \n')
    [[ ${#hex} -ge 24 ]] || return 0
    le32() { local h=${hex:$1:8}; echo $((16#${h:6:2}${h:4:2}${h:2:2}${h:0:2})); }
    local magic perm inh
    magic=$(le32 0); perm=$(le32 8); inh=$(le32 16)
    local perm_hi=0 inh_hi=0
    if [[ ${#hex} -ge 40 ]]; then perm_hi=$(le32 24); inh_hi=$(le32 32); fi
    local mask=$(( (perm_hi << 32) | perm | (inh_hi << 32) | inh ))
    local names; names=$(capsh --decode="$(printf '0x%016x' "$mask")" | cut -d= -f2)
    local flags=""
    (( magic & 1 )) && flags+="e"
    (( perm | perm_hi )) && flags+="p"
    (( inh | inh_hi )) && flags+="i"
    echo "${names}+${flags}"
}

# Prints JSON about a rootfs inside an image (read-only, via debugfs offset).
probe_rootfs() { # img offset partnum
    local fs="$1?offset=$2" tmp; tmp=$(mktemp -d)
    local arch="" distro="" distro_id="" codename="" used=0 resize="" caps="[]"

    local f
    for f in /usr/bin/ls /bin/ls /usr/bin/busybox /bin/busybox; do
        if dbg "$fs" "dump -p $f $tmp/elf" && [[ -s $tmp/elf ]]; then
            case $(od -An -tx1 -j18 -N2 "$tmp/elf" | tr -d ' ') in
                b700) arch=arm64 ;; 3e00) arch=amd64 ;; 2800) arch=armhf ;; f300) arch=riscv64 ;; 0300) arch=i386 ;;
            esac
            break
        fi
    done
    local osr=""
    for f in /usr/lib/os-release /etc/os-release; do
        osr=$(dbg "$fs" "cat $f" || true)
        [[ $osr == *ID=* ]] && break
    done
    osr_get() { sed -n "s/^$1=//p" <<<"$osr" | tr -d '"' | head -1; }
    distro=$(osr_get PRETTY_NAME); distro_id=$(osr_get ID); codename=$(osr_get VERSION_CODENAME)
    [[ $(osr_get NAME) == Debian* && -n $(osr_get VERSION_ID) ]] && distro="Debian $(osr_get VERSION_ID)"

    local hdr; hdr=$(dbg "$fs" "stats -h" || true)
    local bc fb bs
    bc=$(sed -n 's/^Block count: *//p' <<<"$hdr"); fb=$(sed -n 's/^Free blocks: *//p' <<<"$hdr")
    bs=$(sed -n 's/^Block size: *//p' <<<"$hdr")
    [[ -n $bc && -n $fb && -n $bs ]] && used=$(( (bc - fb) * bs ))

    local units=""
    for d in /usr/lib/systemd/system /etc/systemd/system /etc/systemd/system/multi-user.target.wants; do
        units+=$(dbg "$fs" "ls $d" | tr -s ' ' '\n' | grep -E '\.service$' || true)$'\n'
    done
    resize=$(grep -E 'resize|growfs|growpart|expand' <<<"$units" | grep -vE '^systemd-growfs(@|-root)' | head -1 || true)
    if [[ -z $resize ]] && dbg "$fs" "cat /etc/fstab" | grep -q 'x-systemd.growfs'; then
        resize="x-systemd.growfs (fstab)"
    fi

    local list=()
    for f in "${CAP_CANDIDATES[@]}"; do
        rm -f "$tmp/cap"
        dbg "$fs" "ea_get -f $tmp/cap $f security.capability" || true
        if [[ -s $tmp/cap ]]; then
            list+=("$(jq -cn --arg p "$f" --arg c "$(decode_caps "$tmp/cap")" '{path:$p,caps:$c}')")
        fi
    done
    ((${#list[@]})) && caps=$(printf '%s\n' "${list[@]}" | jq -cs .)
    rm -rf "$tmp"

    jq -cn --argjson n "$3" --arg arch "$arch" --arg distro "$distro" --arg id "$distro_id" \
        --arg cn "$codename" --argjson used "$used" --arg resize "$resize" --argjson caps "$caps" \
        'def nz: if . == "" then null else . end;
         {partition:$n, arch:($arch|nz), distro:($distro|nz), distro_id:($id|nz), codename:($cn|nz),
          used_bytes:$used, resize_service:($resize|nz), caps:$caps}'
}

probe() {
    local sha=$1
    [[ $sha =~ ^[0-9a-f]{64}$ ]] || { echo "bad sha" >&2; exit 2; }
    local img=$STORE/bases/$sha.img
    [[ -f $img ]] || { echo "base $sha is not imported" >&2; exit 1; }
    read_table "$img"
    local parts=() n start size info type uuid label rootfs_n=""
    for n in $(jq -r '.parts[].number' <<<"$PARTS_JSON"); do
        start=$(( $(part_field "$n" start) * SECTOR )); size=$(( $(part_field "$n" size) * SECTOR ))
        info=$(blkid_at "$img" "$start")
        type=$(sed -n 's/^TYPE=//p' <<<"$info"); uuid=$(sed -n 's/^UUID=//p' <<<"$info")
        label=$(sed -n 's/^LABEL=//p' <<<"$info")
        [[ $type == ext4 ]] && rootfs_n=$n
        parts+=("$(jq -cn --argjson n "$n" --argjson s "$start" --argjson z "$size" --arg t "$type" --arg u "$uuid" --arg l "$label" \
            'def nz: if . == "" then null else . end;
             {number:$n,start_bytes:$s,size_bytes:$z,fstype:($t|nz),uuid:($u|nz),label:($l|nz)}')")
    done
    # "auto" = last ext4 partition by start offset
    rootfs_n=$(printf '%s\n' "${parts[@]}" | jq -s -r '[.[] | select(.fstype=="ext4")] | max_by(.start_bytes) | .number // empty')
    local rootfs=null
    if [[ -n $rootfs_n ]]; then
        rootfs=$(probe_rootfs "$img" "$(( $(part_field "$rootfs_n" start) * SECTOR ))" "$rootfs_n")
    fi
    printf '%s\n' "${parts[@]}" | jq -cs --arg sha "$sha" --arg label "$LABEL_TYPE" --argjson ss "$SECTOR" \
        --argjson bytes "$(stat -c %s "$img")" --argjson rootfs "$rootfs" \
        '{sha256:$sha, table:$label, sector_size:$ss, image_bytes:$bytes, partitions:., rootfs:$rootfs}'
}

# ---------------------------------------------------------------- arguments

PROJECT_ID="" BUILD_ID="" EXTRACT_ONLY=0
while (($#)); do
    case $1 in
        --probe) probe "$2"; exit 0 ;;
        --project-id) PROJECT_ID=$2; shift 2 ;;
        --build-id) BUILD_ID=$2; shift 2 ;;
        --extract-only) EXTRACT_ONLY=1; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
safe_id "$PROJECT_ID" && safe_id "$BUILD_ID" || { echo "missing or invalid --project-id/--build-id" >&2; exit 2; }

TMP=$STORE/tmp/$BUILD_ID
PROJ=$STORE/projects/$PROJECT_ID
STAGE=$PROJ/stage
OUT=$TMP/out
START_MS=$(now_ms)
STAGE_TOUCHED=0
APT_CACHE="" CHROOT_ACTIVE=0
CHROOT_MOUNTS=()

mkdir -p "$TMP" "$PROJ" "$STORE/bases" "$STORE/rootfs" "$STORE/apt"
echo $$ > "$TMP/pid"

# ---------------------------------------------------------------- chroot

unmount_under() { # path: lazily unmount anything mounted below it
    local m
    for m in $(findmnt -rn -o TARGET | grep -F "$1/" | sort -r || true); do umount -l "$m" 2>/dev/null || true; done
}

enter_chroot() {
    ((CHROOT_ACTIVE)) && return
    CHROOT_ACTIVE=1
    local m failed=()
    for m in proc sys dev dev/pts; do
        mkdir -p "$STAGE/$m"
        case $m in
            proc) mount -t proc proc "$STAGE/proc" ;;
            sys) mount -t sysfs sysfs "$STAGE/sys" ;;
            dev) mount --bind /dev "$STAGE/dev" ;;
            dev/pts) mount --bind /dev/pts "$STAGE/dev/pts" ;;
        esac 2>/dev/null && CHROOT_MOUNTS+=("$STAGE/$m") || failed+=("/$m")
    done
    ((${#failed[@]})) && warn "could not mount ${failed[*]} in the chroot" \
        "Some package scripts may fail; the container runtime may not allow mounts"

    if [[ -e $STAGE/etc/resolv.conf || -L $STAGE/etc/resolv.conf ]]; then
        mv "$STAGE/etc/resolv.conf" "$STAGE/etc/.resolv.conf.imagesmith"
    fi
    cat /etc/resolv.conf > "$STAGE/etc/resolv.conf"
    if [[ ! -e $STAGE/usr/sbin/policy-rc.d ]]; then
        printf '#!/bin/sh\nexit 101\n' > "$STAGE/usr/sbin/policy-rc.d"
        chmod 755 "$STAGE/usr/sbin/policy-rc.d"
        touch "$STAGE/usr/sbin/.policy-rc.d.imagesmith"
    fi
    if ! chroot "$STAGE" /bin/true 2>/dev/null; then
        fail "cannot run the image's binaries in the chroot (arch $(jq -r .arch "$R"))" \
            "amd64 images need Rosetta enabled for the store container"
    fi
}

leave_chroot() {
    ((CHROOT_ACTIVE)) || return 0
    CHROOT_ACTIVE=0
    local i
    for ((i = ${#CHROOT_MOUNTS[@]} - 1; i >= 0; i--)); do umount -l "${CHROOT_MOUNTS[i]}" 2>/dev/null || true; done
    CHROOT_MOUNTS=()
    unmount_under "$STAGE"
    rm -f "$STAGE/etc/resolv.conf"
    if [[ -e $STAGE/etc/.resolv.conf.imagesmith || -L $STAGE/etc/.resolv.conf.imagesmith ]]; then
        mv "$STAGE/etc/.resolv.conf.imagesmith" "$STAGE/etc/resolv.conf"
    fi
    if [[ -e $STAGE/usr/sbin/.policy-rc.d.imagesmith ]]; then
        rm -f "$STAGE/usr/sbin/policy-rc.d" "$STAGE/usr/sbin/.policy-rc.d.imagesmith"
    fi
}

in_chroot() { chroot "$STAGE" /usr/bin/env -i HOME=/root TERM=dumb LANG=C.UTF-8 \
    PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin DEBIAN_FRONTEND=noninteractive "$@"; }

restore_apt_cache() {
    [[ -n $APT_CACHE ]] || return 0
    local arch=$STAGE/var/cache/apt/archives
    if [[ -d $arch ]]; then
        rm -rf "$APT_CACHE"
        mv "$arch" "$APT_CACHE"
    fi
    [[ -d $arch.imagesmith-orig ]] && mv "$arch.imagesmith-orig" "$arch"
    APT_CACHE=""
}

# ---------------------------------------------------------------- traps

on_term() { CANCELLED=1; log "cancelled"; exit 143; }

on_exit() {
    local rc=$?
    trap - ERR
    set +e
    restore_apt_cache
    leave_chroot
    unset PASSWORDS_JSON
    if ((rc != 0)); then
        if ((CANCELLED == 0 && ERRORED == 0)); then
            emit "$(jq -cn --arg s "$CUR_STAGE" --arg m "build.sh exited with $rc" '{type:"error",stage:$s,message:$m,hint:null}')"
        fi
        ((STAGE_TOUCHED)) && touch "$PROJ/stage.incomplete"
        rm -rf "$TMP"
    else
        # Keep only out/ for export; the host deletes it afterwards.
        find "$TMP" -mindepth 1 -maxdepth 1 ! -name out -exec rm -rf {} +
    fi
    emit "$(jq -cn --argjson ok "$([[ $rc == 0 ]] && echo true || echo false)" --argjson d "$(( $(now_ms) - START_MS ))" \
        '{type:"done",ok:$ok,duration_ms:$d}')"
}

trap 'on_err $? "$BASH_COMMAND" $LINENO' ERR
trap on_term TERM INT
trap on_exit EXIT

# Only one build at a time: rootfs and apt caches are shared.
exec 9>"$STORE/.build.lock"
flock -n 9 || fail "another build is already running" "Wait for it to finish or cancel it"

# ================================================================ 1. Prepare
ev_stage prepare "Prepare" 1

IFS= read -r header || fail "no input on stdin"
[[ $header =~ ^IMAGESMITH-INPUT\ 1\ ([0-9]+)$ ]] || fail "bad input header"
mkdir -p "$TMP/input"
head -c "${BASH_REMATCH[1]}" | tar -x -C "$TMP/input" --no-same-owner
IFS= read -r PASSWORDS_JSON || PASSWORDS_JSON='{"passwords":{}}'

R=$TMP/input/recipe.json
r() { jq -r "$1" "$R"; }
BASE_SHA=$(r .base_sha256)
BASE=$STORE/bases/$BASE_SHA.img
[[ -f $BASE ]] || fail "base image $BASE_SHA is not in the store" "Import it with fsctl import-base"
touch "$BASE.used"
PART=$(r .rootfs_partition)
ARCH=$(r .arch)
CACHE_KEY=$(r .cache_key)
safe_id "$CACHE_KEY" || fail "bad cache key"
STEM=$(r .output_stem)
safe_id "$STEM" || fail "bad output name"

read_table "$BASE"
PSTART=$(( $(part_field "$PART" start) * SECTOR ))
PSIZE=$(( $(part_field "$PART" size) * SECTOR ))
((PSIZE > 0)) || fail "partition $PART not found in base image"
IS_LAST=0; [[ $(last_part) == "$PART" ]] && IS_LAST=1
FSINFO=$(blkid_at "$BASE" "$PSTART")
[[ $(sed -n 's/^TYPE=//p' <<<"$FSINFO") == ext4 ]] || fail "partition $PART is not ext4"
FS_UUID=$(sed -n 's/^UUID=//p' <<<"$FSINFO")
FS_LABEL=$(sed -n 's/^LABEL=//p' <<<"$FSINFO")
log "base $BASE_SHA: $LABEL_TYPE table, rootfs partition $PART at $PSTART (+$PSIZE bytes)"
log "rootfs UUID=$FS_UUID LABEL=$FS_LABEL"

# ================================================================ 2. Extract
ev_stage extract "Extract" 2
CACHE=$STORE/rootfs/$CACHE_KEY
META=$STORE/rootfs/.meta-$CACHE_KEY
if [[ -d $CACHE && -f $META ]]; then
    emit "$(jq -cn --arg k "$CACHE_KEY" '{type:"cache",hit:true,key:$k}')"
    log "rootfs cache hit: $CACHE_KEY"
    touch "$CACHE.used"
else
    emit "$(jq -cn --arg k "$CACHE_KEY" '{type:"cache",hit:false,key:$k}')"
    PIMG=$TMP/rootfs.ext4
    ddp "$PSIZE" if="$BASE" of="$PIMG" iflag=skip_bytes,count_bytes skip="$PSTART" count="$PSIZE" conv=sparse
    rc=0
    e2fsck -fy "$PIMG" || rc=$?
    ((rc < 4)) || fail "base filesystem has errors (e2fsck exit $rc)" "Repair the base image or use a different base"

    dumpe2fs -h "$PIMG" 2>/dev/null > "$TMP/fs.hdr"
    WORK=$STORE/rootfs/.tmp-$CACHE_KEY-$$
    rm -rf "$WORK"; mkdir -p "$WORK"
    total=$(dbg "$PIMG" "ls -p /" | awk -F/ 'NF>6 && $6!="." && $6!=".." {c++} END {print c+0}')
    n=0
    while IFS= read -r name; do
        n=$((n + 1))
        log "rdump /$name"
        debugfs -R "rdump \"/$name\" \"$WORK\"" "$PIMG" 2>&1 | grep -v '^debugfs ' || true
        ev_progress "$n" "$total" entries
    done < <(dbg "$PIMG" "ls -p /" | awk -F/ 'NF>6 && $6!="." && $6!=".." {print $6}')
    # Root directory mode and owner come from the filesystem's root inode.
    rootmode=$(dbg "$PIMG" "ls -p /" | awk -F/ '$6=="." {print $3; exit}')
    chmod "${rootmode: -4}" "$WORK"
    chown 0:0 "$WORK"
    # rdump drops xattrs; put file capabilities back.
    for f in "${CAP_CANDIDATES[@]}"; do
        rm -f "$TMP/cap"
        dbg "$PIMG" "ea_get -f $TMP/cap $f security.capability" || true
        [[ -s $TMP/cap && -f $WORK$f ]] || continue
        caps=$(decode_caps "$TMP/cap")
        setcap "$caps" "$WORK$f" || fail "could not restore file capabilities on $f"
        log "restored file capabilities: $f $caps"
    done
    rm -f "$PIMG"
    jq -cn --arg uuid "$FS_UUID" --arg label "$FS_LABEL" \
        --arg features "$(sed -n 's/^Filesystem features: *//p' "$TMP/fs.hdr")" \
        --arg bs "$(sed -n 's/^Block size: *//p' "$TMP/fs.hdr")" \
        --arg isz "$(sed -n 's/^Inode size: *//p' "$TMP/fs.hdr")" \
        --arg bc "$(sed -n 's/^Block count: *//p' "$TMP/fs.hdr")" \
        --arg rbc "$(sed -n 's/^Reserved block count: *//p' "$TMP/fs.hdr")" \
        '{uuid:$uuid,label:$label,features:$features,block_size:($bs|tonumber),inode_size:($isz|tonumber),
          block_count:($bc|tonumber),reserved_blocks:($rbc|tonumber)}' > "$META.tmp"
    # Temp-then-rename: a cancelled build never leaves a half-written cache.
    rm -rf "$CACHE"
    mv "$WORK" "$CACHE"
    mv "$META.tmp" "$META"
    touch "$CACHE.used"
fi

# ================================================================ 3. Snapshot
ev_stage snapshot "Snapshot" 3
unmount_under "$STAGE"
rm -rf --one-file-system "$STAGE" "$PROJ/stage.incomplete"
STAGE_TOUCHED=1
cp -a --reflink=auto "$CACHE" "$STAGE"
touch "$PROJ/stage.incomplete"
if [[ -f $STORE/rootfs/.manifest-$CACHE_KEY ]]; then
    cp "$STORE/rootfs/.manifest-$CACHE_KEY" "$PROJ/base.manifest"
else
    fsctl manifest "$STAGE" > "$PROJ/base.manifest.tmp"
    cp "$PROJ/base.manifest.tmp" "$STORE/rootfs/.manifest-$CACHE_KEY"
    mv "$PROJ/base.manifest.tmp" "$PROJ/base.manifest"
fi
pkglist() { dpkg-query --admindir="$STAGE/var/lib/dpkg" -W -f '${Package}\t${Version}\t${Architecture}\n' 2>/dev/null | sort || true; }
pkglist > "$PROJ/base.packages"
log "base has $(wc -l < "$PROJ/base.manifest") paths, $(wc -l < "$PROJ/base.packages") packages"
echo "$BUILD_ID" > "$PROJ/stage.build"

if ((EXTRACT_ONLY)); then
    cp "$PROJ/base.manifest" "$PROJ/post.manifest"
    rm -f "$PROJ/stage.incomplete"
    exit 0
fi

run_steps() { # key parent-stage index id-prefix
    local n i name rc
    n=$(r ".$1 | length")
    for ((i = 0; i < n; i++)); do
        name=$(r ".$1[$i].name")
        ev_stage "$4-$((i + 1))" "$name" "$3" "$2"
        rc=0
        in_chroot /bin/sh -exc "$(r ".$1[$i].run")" || rc=$?
        ((rc == 0)) || fail "step \"$name\" exited with $rc" "Open Shell to inspect the stage, or edit the step"
    done
}

# ================================================================ 4. First steps
if (($(r '.first_steps | length'))); then
    ev_stage first "First Steps" 4
    enter_chroot
    run_steps first_steps first 4 first
fi

# ================================================================ 5. Packages
ev_stage packages "Packages" 5
mapfile -t PKG_REMOVE < <(r '.packages.remove[]')
mapfile -t PKG_INSTALL < <(r '.packages.install[]')
APT_UPDATE=$(r .packages.apt_update)
NREPOS=$(r '.repos | length')
if ((${#PKG_REMOVE[@]} + ${#PKG_INSTALL[@]} + NREPOS == 0)); then
    log "no packages to install or remove"
else
    [[ -x $STAGE/usr/bin/apt-get ]] || fail "the image has no apt-get" "Remove [packages] from the recipe"
    enter_chroot
    osr() { sed -n "s/^$1=//p" "$STAGE/etc/os-release" "$STAGE/usr/lib/os-release" 2>/dev/null | tr -d '"' | head -1; }
    codename=$(osr VERSION_CODENAME); [[ -n $codename ]] || codename=$(osr VERSION_ID)
    distro="$(osr ID)-${codename:-unknown}"
    APT_KEY=$(tr -c 'A-Za-z0-9._-' '_' <<<"$distro-$ARCH" | sed 's/_$//')
    APT_DIR=$STORE/apt/$APT_KEY
    mkdir -p "$APT_DIR/partial" "$STAGE/var/cache/apt"
    archives=$STAGE/var/cache/apt/archives
    [[ -d $archives ]] && mv "$archives" "$archives.imagesmith-orig"
    mv "$APT_DIR" "$archives"
    APT_CACHE=$APT_DIR
    log "apt cache: $APT_KEY ($(find "$archives" -name '*.deb' | wc -l) debs)"

    APT_OPTS=(-y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold -o Dpkg::Use-Pty=0)
    run_apt() { # args...; output scanned for known failures
        local out=$TMP/apt.out rc=0
        in_chroot apt-get "${APT_OPTS[@]}" "$@" 2>&1 | tee "$out" | apt_progress || rc=$?
        if ((rc != 0)); then
            local missing
            missing=$(sed -n 's/^E: Unable to locate package //p' "$out" | head -1)
            [[ -n $missing ]] && fail "package not found: $missing" "Check the name in the Packages card"
            grep -qE 'Temporary failure resolving|Could not resolve' "$out" &&
                fail "apt could not reach the network (DNS resolution failed)" "Check the container runtime's network and DNS settings"
            fail "apt-get $1 failed (exit $rc)"
        fi
    }
    apt_progress() {
        local total=0 n=0 l
        while IFS= read -r l; do
            printf '%s\n' "$l"
            if [[ $l =~ ([0-9]+)\ newly\ installed ]]; then
                total=${BASH_REMATCH[1]}
                [[ $l =~ ^([0-9]+)\ upgraded ]] && total=$((total + BASH_REMATCH[1]))
            elif [[ $l == Setting\ up\ * ]] && ((total > 0)); then
                n=$((n + 1)); ev_progress "$n" "$total" pkgs
            fi
        done
    }
    ((${#PKG_REMOVE[@]})) && run_apt purge "${PKG_REMOVE[@]}"
    for ((i = 0; i < NREPOS; i++)); do
        name=$(r ".repos[$i].name"); url=$(r ".repos[$i].url")
        safe_id "$name" || fail "bad repository name: $name"
        suite=$(r ".repos[$i].suite // empty"); [[ -n $suite ]] || suite=$codename
        [[ -n $suite ]] || fail "repository $name has no suite and the image's release is unknown" "Set the suite for $name"
        comps=$(r ".repos[$i].components | join(\" \")")
        opts="arch=$ARCH"
        keyfile=$(r ".repos[$i].key_file // empty"); keyurl=$(r ".repos[$i].key_url // empty")
        if [[ -n $keyfile || -n $keyurl ]]; then
            k=$TMP/repo-$i.key
            if [[ -n $keyfile ]]; then
                cp "$TMP/input/$keyfile" "$k"
            else
                curl -fsSL --max-time 60 -o "$k" "$keyurl" || fail "could not download the signing key for $name" "Check the key URL: $keyurl"
            fi
            ext=gpg; grep -q -- '-----BEGIN PGP' "$k" && ext=asc
            mkdir -p "$STAGE/etc/apt/keyrings"
            rm -f "$STAGE/etc/apt/keyrings/imagesmith-$name".{asc,gpg}
            install -m 0644 "$k" "$STAGE/etc/apt/keyrings/imagesmith-$name.$ext"
            opts+=" signed-by=/etc/apt/keyrings/imagesmith-$name.$ext"
        fi
        mkdir -p "$STAGE/etc/apt/sources.list.d"
        printf 'deb [%s] %s %s%s\n' "$opts" "$url" "$suite" "${comps:+ $comps}" > "$STAGE/etc/apt/sources.list.d/imagesmith-$name.list"
        log "repository $name: $url $suite $comps"
    done
    ((NREPOS)) && APT_UPDATE=true
    [[ $APT_UPDATE == true ]] && run_apt update
    ((${#PKG_INSTALL[@]})) && run_apt install "${PKG_INSTALL[@]}"
    in_chroot apt-get -y autoremove --purge >/dev/null 2>&1 || true
    restore_apt_cache
fi

# ================================================================ 6. Configure
ev_stage configure "Configure" 6
enter_chroot

nusers=$(r '.users | length')
for ((i = 0; i < nusers; i++)); do
    u=$(r ".users[$i].name")
    for g in $(r ".users[$i].groups[]"); do
        in_chroot getent group "$g" >/dev/null || { log "creating group $g"; in_chroot groupadd "$g"; }
    done
    groups=$(r ".users[$i].groups | join(\",\")")
    if in_chroot getent passwd "$u" >/dev/null; then
        log "updating user $u"
        [[ -n $groups ]] && in_chroot usermod -aG "$groups" "$u"
    else
        log "creating user $u"
        in_chroot useradd -m -s /bin/bash ${groups:+-G "$groups"} "$u"
    fi
    home=$(in_chroot getent passwd "$u" | cut -d: -f6)
    if [[ $(r ".users[$i].ssh_keys | length") -gt 0 ]]; then
        fsctl put --project-id "$PROJECT_ID" --dir --mode 0700 --owner "$u:$(in_chroot id -gn "$u")" "$home/.ssh" >/dev/null
        r ".users[$i].ssh_keys[]" | fsctl put --project-id "$PROJECT_ID" --mode 0600 \
            --owner "$u:$(in_chroot id -gn "$u")" "$home/.ssh/authorized_keys" >/dev/null
        log "installed $(r ".users[$i].ssh_keys | length") SSH key(s) for $u"
    fi
    case $(r ".users[$i].password") in
        locked) in_chroot passwd -l "$u" >/dev/null ;;
        prompt)
            pw=$(printf '%s' "$PASSWORDS_JSON" | jq -r --arg u "$u" '.passwords[$u] // empty')
            [[ -n $pw ]] || fail "no password provided for $u"
            printf '%s:%s\n' "$u" "$pw" | in_chroot chpasswd
            unset pw
            log "set password for $u"
            ;;
    esac
done
unset PASSWORDS_JSON

hostname=$(r '.system.hostname // empty')
if [[ -n $hostname ]]; then
    echo "$hostname" > "$STAGE/etc/hostname"
    if grep -q '^127\.0\.1\.1' "$STAGE/etc/hosts" 2>/dev/null; then
        sed -i "s/^127\.0\.1\.1.*/127.0.1.1\t$hostname/" "$STAGE/etc/hosts"
    else
        printf '127.0.1.1\t%s\n' "$hostname" >> "$STAGE/etc/hosts"
    fi
    log "hostname: $hostname"
fi

tz=$(r '.system.timezone // empty')
if [[ -n $tz ]]; then
    [[ -e $STAGE/usr/share/zoneinfo/$tz ]] || fail "unknown timezone: $tz" "Install tzdata or pick a valid zone"
    ln -sfn "/usr/share/zoneinfo/$tz" "$STAGE/etc/localtime"
    echo "$tz" > "$STAGE/etc/timezone"
    log "timezone: $tz"
fi

loc=$(r '.system.locale // empty')
if [[ -n $loc ]]; then
    charset=${loc#*.}; [[ $charset == "$loc" ]] && charset=ISO-8859-1
    if [[ -f $STAGE/etc/locale.gen ]]; then
        if grep -qE "^#? *${loc//./\\.} " "$STAGE/etc/locale.gen"; then
            sed -i -E "s/^# *(${loc//./\\.} )/\1/" "$STAGE/etc/locale.gen"
        else
            echo "$loc $charset" >> "$STAGE/etc/locale.gen"
        fi
    fi
    if [[ -x $STAGE/usr/sbin/locale-gen ]]; then
        in_chroot locale-gen
    else
        warn "locale-gen is not installed; $loc may not be available" "Add the locales package"
    fi
    echo "LANG=$loc" > "$STAGE/etc/default/locale"
    log "locale: $loc"
fi

if [[ $(r .system.reset_machine_id) == true ]]; then
    : > "$STAGE/etc/machine-id"
    if [[ -d $STAGE/var/lib/dbus ]]; then
        rm -f "$STAGE/var/lib/dbus/machine-id"
        ln -s /etc/machine-id "$STAGE/var/lib/dbus/machine-id"
    fi
    log "machine-id reset"
fi

if [[ $(r .system.reset_ssh_host_keys) == true ]]; then
    rm -f "$STAGE"/etc/ssh/ssh_host_*
    if [[ -d $STAGE/etc/ssh ]]; then
        install -m 0644 "$LIB/imagesmith-ssh-keygen.service" "$STAGE/etc/systemd/system/imagesmith-ssh-keygen.service"
        mkdir -p "$STAGE/etc/systemd/system/multi-user.target.wants"
        ln -sfn /etc/systemd/system/imagesmith-ssh-keygen.service \
            "$STAGE/etc/systemd/system/multi-user.target.wants/imagesmith-ssh-keygen.service"
    fi
    log "SSH host keys removed; regenerated on first boot"
fi

if [[ $(r .size.expand_on_boot) == true ]]; then
    existing=$(ls "$STAGE"/{usr/lib,lib,etc}/systemd/system/ 2>/dev/null | grep -E 'resize|growpart|expand' | grep -vE '^systemd-growfs|imagesmith' | head -1 || true)
    if [[ -n $existing ]]; then
        log "base already has a resize service ($existing)"
    elif ((IS_LAST)); then
        install -D -m 0755 "$LIB/expand-rootfs.sh" "$STAGE/usr/local/sbin/imagesmith-expand-rootfs"
        install -m 0644 "$LIB/imagesmith-expand-rootfs.service" "$STAGE/etc/systemd/system/imagesmith-expand-rootfs.service"
        mkdir -p "$STAGE/etc/systemd/system/multi-user.target.wants"
        ln -sfn /etc/systemd/system/imagesmith-expand-rootfs.service \
            "$STAGE/etc/systemd/system/multi-user.target.wants/imagesmith-expand-rootfs.service"
        log "installed imagesmith-expand-rootfs.service"
    else
        warn "expand_on_boot ignored: rootfs is not the last partition"
    fi
fi

# ================================================================ 7. Files
ev_stage files "Files" 7
nfiles=$(r '.files | length')
for ((i = 0; i < nfiles; i++)); do
    src=$TMP/input/$(r ".files[$i].src"); dst=$(r ".files[$i].dst")
    # fsctl resolves symlinks inside the rootfs, so nothing lands outside it.
    fsctl put --project-id "$PROJECT_ID" --mode "$(r ".files[$i].mode")" --owner "$(r ".files[$i].owner")" "$dst" < "$src" >/dev/null
    log "$dst ($(r ".files[$i].mode") $(r ".files[$i].owner"))"
done
if [[ -d $TMP/input/overlay ]] && [[ -n $(ls -A "$TMP/input/overlay") ]]; then
    n=$(find "$TMP/input/overlay" -mindepth 1 | wc -l)
    # Extract inside the chroot so absolute symlinks resolve against the image.
    tar -C "$TMP/input/overlay" --owner=0 --group=0 --numeric-owner -cf - . |
        in_chroot tar -xf - -C / --no-overwrite-dir --same-permissions --numeric-owner
    log "overlay: $n entries"
fi
for s in $(r '.services.enable[]'); do
    log "enable $s"
    in_chroot systemctl enable "$s" || warn "could not enable $s" "Check the unit name"
done
for s in $(r '.services.disable[]'); do
    log "disable $s"
    in_chroot systemctl disable "$s" || warn "could not disable $s" "Check the unit name"
done

# ================================================================ 8. Steps
ev_stage steps "Steps" 8
run_steps steps steps 8 step
leave_chroot

# ================================================================ 9. Assemble
ev_stage assemble "Assemble" 9
META_JSON=$(cat "$META")
m() { jq -r "$1" <<<"$META_JSON"; }
BS=$(m .block_size); ISZ=$(m .inode_size)
RESERVED_PCT=$(m '((.reserved_blocks * 100 / .block_count) | floor)')
MiB=$((1 << 20))
align_up() { echo $(( ($1 + $2 - 1) / $2 * $2 )); }

# Keep the base's feature set so older bootloaders and kernels can read it.
features=()
orig=" $(m .features) "
for f in has_journal ext_attr resize_inode dir_index filetype extent 64bit flex_bg sparse_super large_file \
         huge_file dir_nlink extra_isize metadata_csum metadata_csum_seed orphan_file; do
    [[ $orig == *" $f "* ]] && features+=("$f") || features+=("^$f")
done
for f in $(m .features); do
    case " ${features[*]} " in *" $f "*) ;; *) [[ $f == needs_recovery ]] || features+=("$f") ;; esac
done
FEATURES=$(IFS=,; echo "${features[*]}")

used=$(du -s -B1 "$STAGE" | cut -f1)
inodes=$(find "$STAGE" -xdev | wc -l)
mode=$(r .size.mode)
case $mode in
    shrink)
        headroom=$(r .size.headroom_pct)
        content=$(awk -v u="$used" -v i="$inodes" -v isz="$ISZ" -v h="$headroom" \
            'BEGIN { printf "%d", (u + i * isz * 1.5) * (1 + h / 100) }')
        FS_BYTES=$(awk -v c="$content" -v r="$RESERVED_PCT" 'BEGIN { printf "%d", (c + 96 * 1048576) / (1 - r / 100) * 1.04 }')
        ;;
    grow) FS_BYTES=$((PSIZE + $(r .size.grow_bytes))) ;;
    fixed)
        if ((IS_LAST)); then
            tail=0; [[ $LABEL_TYPE == gpt ]] && tail=$MiB
            FS_BYTES=$(( $(r .size.fixed_bytes) - PSTART - tail ))
        else
            FS_BYTES=$PSIZE
            log "rootfs is not the last partition: keeping its size ($PSIZE bytes)"
        fi
        ;;
esac
FS_BYTES=$(align_up "$FS_BYTES" "$((4 * MiB))")
((IS_LAST)) || ((FS_BYTES <= PSIZE)) || FS_BYTES=$PSIZE
((FS_BYTES > 16 * MiB)) || fail "rootfs size too small ($FS_BYTES bytes)" "Increase the size in the Size card"

NEWFS=$TMP/rootfs.new
ninodes=$(awk -v i="$inodes" 'BEGIN { printf "%d", i * 1.5 + 8192 }')
for attempt in 1 2 3 4 5; do
    rm -f "$NEWFS"; truncate -s "$FS_BYTES" "$NEWFS"
    log "mke2fs: $FS_BYTES bytes, block size $BS, $ninodes+ inodes (attempt $attempt)"
    rc=0
    mke2fs -q -F -t ext4 -b "$BS" -I "$ISZ" -N "$ninodes" -m "$RESERVED_PCT" -O "$FEATURES" \
        -U "${FS_UUID:-random}" ${FS_LABEL:+-L "$FS_LABEL"} -E lazy_itable_init=0,lazy_journal_init=0 \
        -d "$STAGE" "$NEWFS" "$((FS_BYTES / BS))" 2>&1 | tee "$TMP/mke2fs.out" || rc=$?
    ((rc == 0)) && break
    if grep -qiE 'no space|could not allocate|not enough space' "$TMP/mke2fs.out"; then
        if [[ $mode == fixed ]] || ! ((IS_LAST)); then
            need=$(awk -v u="$used" 'BEGIN { printf "%.1f", u * 1.15 / 1073741824 }')
            fail "fixed size too small for content" "Content needs at least ${need}G for the root filesystem"
        fi
        FS_BYTES=$(align_up "$((FS_BYTES + FS_BYTES / 10))" "$((4 * MiB))")
        continue
    fi
    fail "mke2fs failed (exit $rc)"
done
((rc == 0)) || fail "could not fit the rootfs after 5 attempts"
e2fsck -fn "$NEWFS" >/dev/null || fail "new filesystem failed e2fsck"

IMG=$TMP/out.img
if ((IS_LAST)); then
    tail=0; [[ $LABEL_TYPE == gpt ]] && tail=$MiB
    TOTAL=$(align_up "$((PSTART + FS_BYTES + tail))" "$MiB")
    rm -f "$IMG"
    # Everything before the rootfs (tables, bootloader sectors, other partitions) verbatim.
    dd if="$BASE" of="$IMG" bs=4M iflag=count_bytes count="$PSTART" conv=sparse status=none
    truncate -s "$TOTAL" "$IMG"
    ddp "$FS_BYTES" if="$NEWFS" of="$IMG" oflag=seek_bytes seek="$PSTART" conv=sparse,notrunc
    sfdisk --dump "$BASE" | sed -E -e '/^last-lba:/d' -e '/^device:/d' \
        -e "\|^$BASE$PART :|s/size= *[0-9]+/size=$((FS_BYTES / SECTOR))/" \
        -e "s|^$BASE|$IMG|" > "$TMP/table"
    sfdisk --no-reread --no-tell-kernel --wipe never --wipe-partitions never -q "$IMG" < "$TMP/table"
else
    cp --sparse=always "$BASE" "$IMG"
    ddp "$FS_BYTES" if="$NEWFS" of="$IMG" oflag=seek_bytes seek="$PSTART" conv=sparse,notrunc
fi
rm -f "$NEWFS"
if ! sfdisk --verify "$IMG" >/dev/null 2>&1; then
    if [[ $LABEL_TYPE == gpt ]]; then
        sfdisk --relocate gpt-bak-std "$IMG" >/dev/null 2>&1 || true
    fi
    sfdisk --verify "$IMG" || warn "partition table verification reported problems" "Check the image with sfdisk --verify"
fi
log "image: $(stat -c %s "$IMG") bytes, rootfs $FS_BYTES bytes"

# ================================================================ 10. Package
ev_stage package "Package" 10
mkdir -p "$OUT"
fsctl manifest "$STAGE" > "$PROJ/post.manifest"
pkglist > "$PROJ/post.packages"
cp "$PROJ/base.packages" "$OUT/$STEM.packages.base.tsv"
cp "$PROJ/post.packages" "$OUT/$STEM.packages.post.tsv"
fsctl diff "$PROJ/base.manifest" "$PROJ/post.manifest" --prefix /etc > "$OUT/$STEM.etc-diff.json"
jq -cn --argjson img "$(stat -c %s "$IMG")" --argjson fs "$FS_BYTES" --argjson used "$used" \
    --argjson pkgs "$(wc -l < "$PROJ/post.packages")" --arg b "$(r .builder_version)" \
    '{image_bytes:$img,rootfs_bytes:$fs,rootfs_used:$used,packages:$pkgs,builder_version:$b}' > "$OUT/$STEM.summary.json"

if [[ $(r .output.bmap) == true ]]; then
    bmaptool create -o "$OUT/$STEM.img.bmap" "$IMG"
fi
level=$(r .output.level)
case $(r .output.compress) in
    zstd) FINAL=$OUT/$STEM.img.zst
          ddp "$(stat -c %s "$IMG")" if="$IMG" | zstd -q -T0 "-$level" -o "$FINAL" ;;
    xz)   FINAL=$OUT/$STEM.img.xz
          ddp "$(stat -c %s "$IMG")" if="$IMG" | xz -T0 "-$level" > "$FINAL" ;;
    *)    FINAL=$OUT/$STEM.img
          mv "$IMG" "$FINAL" ;;
esac
rm -f "$IMG"
artifact image "$FINAL"
[[ -f $OUT/$STEM.img.bmap ]] && artifact bmap "$OUT/$STEM.img.bmap"
if [[ $(r .output.checksum) == true ]]; then
    (cd "$OUT" && sha256sum "$(basename "$FINAL")" > "$FINAL.sha256")
    artifact checksum "$FINAL.sha256"
fi
for f in "$OUT/$STEM".packages.*.tsv "$OUT/$STEM.etc-diff.json" "$OUT/$STEM.summary.json"; do
    artifact manifest "$f"
done
rm -f "$PROJ/stage.incomplete"
log "done: $(basename "$FINAL")"
