#!/bin/sh
# Build the Agent OS guest image (works on macOS and Linux hosts):
#   ~/.agentos/images/kernel            - Alpine linux-virt kernel
#   ~/.agentos/images/initramfs.cpio.gz - Alpine minirootfs + agentos-guest-agent as /init
#
# The virt kernel ships vsock/virtiofs as modules, so the needed .ko files are
# staged into the initramfs at /lib/modules/agentos and loaded by the guest agent.
#
# Requires: curl, cpio (bsdcpio or GNU), unsquashfs (brew install squashfs /
# apt install squashfs-tools), python3, and a prior
# `cargo build -p agentos-guest-agent --release --target <arch>-unknown-linux-musl`.
#
# Guest arch defaults to the host's; override with GUEST_ARCH=aarch64|x86_64.
set -eu
cd "$(dirname "$0")/.."

MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine/latest-stable}"
case "${GUEST_ARCH:-$(uname -m)}" in
    arm64|aarch64) ARCH=aarch64 ;;
    x86_64|amd64)  ARCH=x86_64 ;;
    *) echo "unsupported guest arch: ${GUEST_ARCH:-$(uname -m)}" >&2; exit 1 ;;
esac
IMAGES="$HOME/.agentos/images"
CACHE="$HOME/.agentos/cache"
GUEST_AGENT=target/$ARCH-unknown-linux-musl/release/agentos-guest-agent

[ -f "$GUEST_AGENT" ] || { echo "guest agent not built: $GUEST_AGENT" >&2; exit 1; }
mkdir -p "$IMAGES" "$CACHE"

fetch() { # fetch <url> <dest>
    [ -f "$2" ] && return 0
    echo "fetching $1"
    curl -fsSL -o "$2.tmp" "$1" && mv "$2.tmp" "$2"
}

VER=$(curl -fsSL "$MIRROR/releases/$ARCH/latest-releases.yaml" | awk '/^  version:/{print $2; exit}')
echo "alpine version: $VER"

fetch "$MIRROR/releases/$ARCH/alpine-minirootfs-$VER-$ARCH.tar.gz" "$CACHE/minirootfs-$VER-$ARCH.tar.gz"
fetch "$MIRROR/releases/$ARCH/netboot/vmlinuz-virt" "$CACHE/vmlinuz-virt-$VER-$ARCH"
fetch "$MIRROR/releases/$ARCH/netboot/modloop-virt" "$CACHE/modloop-virt-$VER-$ARCH"

KSRC="$CACHE/vmlinuz-virt-$VER-$ARCH"
if [ "$ARCH" = aarch64 ]; then
    # aarch64 VMMs need an uncompressed ARM64 Image. Alpine ships vmlinuz-virt
    # either gzipped or as an EFI zboot PE ("MZ..zimg") whose gzip payload we
    # must unwrap (header: payload offset @8, size @12, LE).
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
else
    # x86_64: vmlinuz-virt is a bzImage, which Cloud Hypervisor boots directly.
    cp "$KSRC" "$IMAGES/kernel.tmp"
fi
mv "$IMAGES/kernel.tmp" "$IMAGES/kernel"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# vsock + virtiofs modules out of the modloop squashfs (gzipped .ko.gz).
unsquashfs -q -n -d "$WORK/modloop" "$CACHE/modloop-virt-$VER-$ARCH" \
    'modules/*/kernel/net/vmw_vsock/*' 'modules/*/kernel/fs/fuse/*' > /dev/null
VSOCKDIR=$(echo "$WORK"/modloop/modules/*/kernel/net/vmw_vsock)
FUSEDIR=$(echo "$WORK"/modloop/modules/*/kernel/fs/fuse)
[ -d "$VSOCKDIR" ] || { echo "vsock modules not found in modloop" >&2; exit 1; }
[ -d "$FUSEDIR" ] || { echo "fuse modules not found in modloop" >&2; exit 1; }

# Rootfs: Alpine minirootfs + our init + staged modules.
ROOT="$WORK/rootfs"
mkdir -p "$ROOT"
tar -xzf "$CACHE/minirootfs-$VER-$ARCH.tar.gz" -C "$ROOT"
cp "$GUEST_AGENT" "$ROOT/init"
chmod 755 "$ROOT/init"

mkdir -p "$ROOT/lib/modules/agentos"
stage_module() { # stage_module <srcdir> <name>
    if [ -f "$1/$2.ko.gz" ]; then
        gunzip -c "$1/$2.ko.gz" > "$ROOT/lib/modules/agentos/$2.ko"
    elif [ -f "$1/$2.ko" ]; then
        cp "$1/$2.ko" "$ROOT/lib/modules/agentos/$2.ko"
    else
        echo "missing module $2 in $1" >&2; exit 1
    fi
    echo "$2.ko"
}
{
    stage_module "$VSOCKDIR" vsock
    stage_module "$VSOCKDIR" vmw_vsock_virtio_transport_common
    stage_module "$VSOCKDIR" vmw_vsock_virtio_transport
    stage_module "$FUSEDIR" fuse
    stage_module "$FUSEDIR" virtiofs
} > "$ROOT/lib/modules/agentos/order"

# Pack as newc cpio, everything owned by root.
(cd "$ROOT" && find . | cpio -o --format newc -R 0:0 --quiet | gzip -1) \
    > "$IMAGES/initramfs.cpio.gz.tmp"
mv "$IMAGES/initramfs.cpio.gz.tmp" "$IMAGES/initramfs.cpio.gz"

echo "kernel:    $(ls -lh "$IMAGES/kernel" | awk '{print $5}')  $IMAGES/kernel"
echo "initramfs: $(ls -lh "$IMAGES/initramfs.cpio.gz" | awk '{print $5}')  $IMAGES/initramfs.cpio.gz"
