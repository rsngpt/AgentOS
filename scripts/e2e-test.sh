#!/bin/sh
# End-to-end test suite: boots real microVMs through the full stack
# (CLI -> daemon -> VMM backend -> guest agent) and asserts the security
# policy behavior that ARCHITECTURE.md promises. Backend-agnostic: runs
# against whichever backend agentos-vmm selects on this host.
#
# Prereqs: workspace built, guest image in ~/.agentos/images, and on macOS
# the vmhelper built; on Linux, cloud-hypervisor + virtiofsd on PATH and KVM.
#
# Usage: scripts/e2e-test.sh [path-to-agentos]   (default target/debug/agentos)
set -u
cd "$(dirname "$0")/.."

AGENTOS="${1:-target/debug/agentos}"
[ -x "$AGENTOS" ] || { echo "agentos binary not found: $AGENTOS" >&2; exit 1; }

# Fresh daemon so we test the binaries just built.
pkill -f "$(basename "$AGENTOS")d" 2>/dev/null; sleep 0.5

FAILURES=0
check() { # check <name> <expected> <actual>
    if [ "$2" = "$3" ]; then
        echo "PASS: $1"
    else
        echo "FAIL: $1 — expected [$2], got [$3]" >&2
        FAILURES=$((FAILURES + 1))
    fi
}

echo "== basic exec + stdout =="
out=$("$AGENTOS" run -- echo hi 2>/dev/null)
check "echo through microVM" "hi" "$out"

echo "== exit code + stderr routing =="
"$AGENTOS" run -- sh -c 'echo err >&2; exit 3' >/dev/null 2>/tmp/agentos-e2e-err.txt
check "exit code propagation" "3" "$?"
check "stderr routing" "err" "$(grep -v '^sandbox' /tmp/agentos-e2e-err.txt)"

echo "== mounts =="
M=$(mktemp -d)
mkdir -p "$M/ro" "$M/rw"
echo hello > "$M/ro/f.txt"
out=$("$AGENTOS" run --mount "$M/ro" --mount "$M/rw:rw" -- sh -c \
    'cat /mnt/ro/f.txt; echo guest > /mnt/rw/out.txt 2>/dev/null && echo rw-ok; echo x > /mnt/ro/f2.txt 2>/dev/null && echo RO-LEAK || echo ro-blocked' 2>/tmp/agentos-e2e-mount-err.txt)
check "mount behavior" "hello
rw-ok
ro-blocked" "$out"
[ "$out" = "hello
rw-ok
ro-blocked" ] || { echo "  mount run stderr was:" >&2; sed 's/^/  /' /tmp/agentos-e2e-mount-err.txt >&2; }
check "rw mount round-trips to host" "guest" "$(cat "$M/rw/out.txt" 2>/dev/null)"
rm -rf "$M"

echo "== runtimes: python3, node, git present in the guest rootfs =="
out=$("$AGENTOS" run -- sh -c 'command -v python3 >/dev/null && command -v node >/dev/null && command -v git >/dev/null && echo runtimes-ok || echo missing' 2>/dev/null)
check "python3+node+git available" "runtimes-ok" "$out"

echo "== overlay: the agent root is writable (copy-up over the ro rootfs) =="
out=$("$AGENTOS" run -- sh -c 'echo ok > /usr/agentos-write-test && cat /usr/agentos-write-test' 2>/dev/null)
check "overlay makes the root writable" "ok" "$out"

echo "== network: offline (default) =="
out=$("$AGENTOS" run -- sh -c 'wget -T 5 -q -O- http://example.com >/dev/null 2>&1 && echo LEAK || echo blocked' 2>/dev/null)
check "offline blocks egress" "blocked" "$out"

echo "== network: allowlist =="
out=$("$AGENTOS" run --net allowlist:example.com -- sh -c \
    'wget -T 15 -q -O- http://example.com 2>/dev/null | grep -c "Example Domain"; wget -T 10 -q -O- http://neverssl.com >/dev/null 2>&1 && echo LEAK || echo denied' 2>/dev/null)
check "allowlist allows listed, denies rest" "1
denied" "$out"

echo "== network: full mode still blocks local ranges =="
out=$("$AGENTOS" run --net full -- sh -c \
    'wget -T 5 -q -O- http://192.168.1.1 >/dev/null 2>&1 && echo LAN-LEAK || echo lan-blocked; wget -T 5 -q -O- http://localhost:8080 >/dev/null 2>&1 && echo LO-LEAK || echo lo-blocked' 2>/dev/null)
check "full mode local blocking" "lan-blocked
lo-blocked" "$out"

echo "== auto-kill: runtime =="
"$AGENTOS" run --kill-after-secs 3 -- sleep 60 >/dev/null 2>&1
check "runtime auto-kill exit code" "137" "$?"

echo "== kill switch: save disposition =="
"$AGENTOS" run -- sleep 60 >/dev/null 2>&1 &
runner=$!
sleep 4
id=$("$AGENTOS" ps | awk '$3=="running"{print $1; exit}')
if [ -n "$id" ]; then
    "$AGENTOS" kill --save "$id" >/dev/null 2>&1
    wait "$runner"
    check "killed runner exit code" "137" "$?"
    [ -f "$HOME/.agentos/sandboxes/$id/console.log" ]
    check "save disposition keeps console.log" "0" "$?"
    rm -rf "$HOME/.agentos/sandboxes/$id"
else
    echo "FAIL: kill-save — no running sandbox found" >&2
    kill "$runner" 2>/dev/null
    FAILURES=$((FAILURES + 1))
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "e2e: all tests passed"
else
    echo "e2e: $FAILURES test(s) FAILED" >&2
    exit 1
fi
