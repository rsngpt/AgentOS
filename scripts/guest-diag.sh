#!/bin/sh
# Diagnostic: print the guest's block devices, loaded filesystems, root mount,
# and writability to stdout (captured by the CLI), to debug overlay setup.
set -u
AGENTOS="${1:-target/debug/agentos}"
"$AGENTOS" run -- sh -c '
  echo "FS=$(grep -oE "ext4|overlay|squashfs" /proc/filesystems | tr "\n" ",")"
  echo "DEVS=$(ls -l /dev/vda /dev/vdb 2>&1 | tr "\n" "|")"
  echo "ROOT=$(awk "\$2==\"/\"{print \$1\" \"\$3}" /proc/mounts)"
  echo "MOUNTS:"; grep -E "overlay|ext4|squashfs|vd[ab]" /proc/mounts || true
  echo "WRITE=$(echo x > /wtest 2>&1 && echo rw || echo ro)"
  echo "DMESG:"; dmesg 2>/dev/null | grep -iE "ext4|overlay|squashfs|virtio_blk|vdb" | tail -20 || true
'

