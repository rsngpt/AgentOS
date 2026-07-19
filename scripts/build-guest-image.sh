#!/bin/sh
# Build the Agent OS guest image for aarch64 macOS hosts:
#   ~/.agentos/images/kernel            - Alpine linux-virt kernel (uncompressed Image)
#   ~/.agentos/images/initramfs.cpio.gz - Alpine minirootfs + agentos-guest-agent as /init
#
# The virt kernel ships vsock as modules, so the needed .ko files are staged
# into the initramfs at /lib/modules/agentos and loaded by the guest agent.
#
# Requires: curl, bsdcpio (macOS built-in), unsquashfs (brew install squashfs),
# and a prior `cargo build -p agentos-guest-agent --release --target aarch64-unknown-linux-musl`.
set -eu
cd "$(dirname "$0")/.."

MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine/latest-stable}"
ARCH=aarch64
IMAGES="$HOME/.agentos/images"
CACHE="$HOME/.agentos/cache"
GUEST_AGENT=target/aarch64-unknown-linux-musl/release/agentos-guest-agent

[ -f "$GUEST_AGENT" ] || { echo "guest agent not built: $GUEST_AGENT" >&2; exit 1; }
mkdir -p "$IMAGES" "$CACHE"

fetch() { # fetch <url> <dest>
    [ -f "$2" ] && return 0
    echo "fetching $1"
    curl -fsSL -o "$2.tmp" "$1" && mv "$2.tmp" "$2"
}

VER=$(curl -fsSL "$MIRROR/releases/$ARCH/latest-releases.yaml" | awk '/^  version:/{print $2; exit}')
echo "alpine version: $VER"

fetch "$MIRROR/releases/$ARCH/alpine-minirootfs-$VER-$ARCH.tar.gz" "$CACHE/minirootfs-$VER.tar.gz"
fetch "$MIRROR/releases/$ARCH/netboot/vmlinuz-virt" "$CACHE/vmlinuz-virt-$VER"
fetch "$MIRROR/releases/$ARCH/netboot/modloop-virt" "$CACHE/modloop-virt-$VER"

# Kernel: Virtualization.framework needs an uncompressed ARM64 Image.
# Alpine ships vmlinuz-virt either gzipped or as an EFI zboot PE ("MZ..zimg")
# whose gzip payload we must unwrap (header: payload offset @8, size @12, LE).
KSRC="$CACHE/vmlinuz-virt-$VER"
if [ "$(dd if="$KSRC" bs=1 skip=4 count=4 2>/dev/null)" = "zimg" ]; then
    python3 - "$KSRC" "$IMAGES/kernel.tmp" <<'EOF'
import gzip, struct, sys
data = open(sys.argv[1], "rb").read()
off, size = struct.unpack_from("<II", data, 8)
open(sys.argv[2], "wb").write(gzip.decompress(data[off:off + size]))
EOF
elif file "$KSRC" | grep -q gzip; then
    gunzip -c "$KSRC" > "$IMAGES/kernel.tmp"
else
    cp "$KSRC" "$IMAGES/kernel.tmp"
fi
# Sanity: ARM64 Image magic "ARM\x64" at offset 0x38.
magic=$(dd if="$IMAGES/kernel.tmp" bs=1 skip=56 count=4 2>/dev/null)
[ "$magic" = "ARMd" ] || { echo "extracted kernel lacks ARM64 Image magic" >&2; exit 1; }
mv "$IMAGES/kernel.tmp" "$IMAGES/kernel"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# vsock modules out of the modloop squashfs (they are gzipped .ko.gz).
unsquashfs -q -n -d "$WORK/modloop" "$CACHE/modloop-virt-$VER" \
    'modules/*/kernel/net/vmw_vsock/*' > /dev/null
MODDIR=$(echo "$WORK"/modloop/modules/*/kernel/net/vmw_vsock)
[ -d "$MODDIR" ] || { echo "vsock modules not found in modloop" >&2; exit 1; }

# Rootfs: Alpine minirootfs + our init + staged modules.
ROOT="$WORK/rootfs"
mkdir -p "$ROOT"
tar -xzf "$CACHE/minirootfs-$VER.tar.gz" -C "$ROOT"
cp "$GUEST_AGENT" "$ROOT/init"
chmod 755 "$ROOT/init"

mkdir -p "$ROOT/lib/modules/agentos"
for m in vsock vmw_vsock_virtio_transport_common vmw_vsock_virtio_transport; do
    if [ -f "$MODDIR/$m.ko.gz" ]; then
        gunzip -c "$MODDIR/$m.ko.gz" > "$ROOT/lib/modules/agentos/$m.ko"
    elif [ -f "$MODDIR/$m.ko" ]; then
        cp "$MODDIR/$m.ko" "$ROOT/lib/modules/agentos/$m.ko"
    else
        echo "missing module $m in $MODDIR" >&2; exit 1
    fi
    echo "$m.ko"
done > "$ROOT/lib/modules/agentos/order"

# Pack as newc cpio, everything owned by root.
(cd "$ROOT" && find . | cpio -o --format newc -R 0:0 --quiet | gzip -1) \
    > "$IMAGES/initramfs.cpio.gz.tmp"
mv "$IMAGES/initramfs.cpio.gz.tmp" "$IMAGES/initramfs.cpio.gz"

echo "kernel:    $(ls -lh "$IMAGES/kernel" | awk '{print $5}')  $IMAGES/kernel"
echo "initramfs: $(ls -lh "$IMAGES/initramfs.cpio.gz" | awk '{print $5}')  $IMAGES/initramfs.cpio.gz"
