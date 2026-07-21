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

echo "== stdin: interactive agents can be driven (PRD Flow 1) =="
check "stdin reaches the command" "hello" "$(printf 'hello\n' | "$AGENTOS" run -- cat 2>/dev/null)"
# `sort` only finishes if EOF propagates; a hang here means stdin never closes.
check "stdin EOF propagates" "a
b" "$(printf 'b\na\n' | "$AGENTOS" run -- sort 2>/dev/null)"

echo "== runtimes: python3, node, git present in the guest rootfs =="
out=$("$AGENTOS" run -- sh -c 'command -v python3 >/dev/null && command -v node >/dev/null && command -v git >/dev/null && echo runtimes-ok || echo missing' 2>/dev/null)
check "python3+node+git available" "runtimes-ok" "$out"

echo "== overlay: the agent root is writable (copy-up over the ro rootfs) =="
out=$("$AGENTOS" run -- sh -c 'echo ok > /usr/agentos-write-test && cat /usr/agentos-write-test' 2>/dev/null)
check "overlay makes the root writable" "ok" "$out"

echo "== git: --repo clones host-side and mounts at /workspace =="
out=$("$AGENTOS" run --repo https://github.com/octocat/Hello-World.git -- sh -c 'pwd; test -f README && echo readme-present' 2>/dev/null)
check "repo cloned and mounted at /workspace" "/workspace
readme-present" "$out"

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

echo "== pause/resume: vCPUs freeze and continue where they left off =="
"$AGENTOS" run -- sh -c 'i=0; while [ $i -lt 60 ]; do echo "t$i"; i=$((i+1)); sleep 1; done' \
    > /tmp/agentos-e2e-ticks.txt 2>&1 &
ticker=$!
sleep 6
id=$("$AGENTOS" ps | awk '$3=="running"{print $1; exit}')
if [ -n "$id" ]; then
    "$AGENTOS" pause "$id" >/dev/null 2>&1
    check "state is paused" "paused" "$("$AGENTOS" ps | awk -v i="$id" '$1==i{print $3}')"
    before=$(wc -l < /tmp/agentos-e2e-ticks.txt)
    sleep 4
    after=$(wc -l < /tmp/agentos-e2e-ticks.txt)
    check "guest is frozen while paused" "$before" "$after"
    "$AGENTOS" resume "$id" >/dev/null 2>&1
    sleep 4
    resumed=$(wc -l < /tmp/agentos-e2e-ticks.txt)
    if [ "$resumed" -gt "$after" ]; then
        echo "PASS: guest advances again after resume"
    else
        echo "FAIL: guest did not advance after resume ($after -> $resumed)" >&2
        FAILURES=$((FAILURES + 1))
    fi
    "$AGENTOS" kill "$id" >/dev/null 2>&1
    wait "$ticker" 2>/dev/null
else
    echo "FAIL: pause/resume — no running sandbox found" >&2
    kill "$ticker" 2>/dev/null
    FAILURES=$((FAILURES + 1))
fi

echo "== snapshot/restore: guest resumes mid-execution =="
"$AGENTOS" run -- sh -c 'i=0; while [ $i -lt 120 ]; do echo "s$i"; i=$((i+1)); sleep 1; done' \
    > /tmp/agentos-e2e-snap.txt 2>&1 &
snapper=$!
sleep 7
id=$("$AGENTOS" ps | awk '$3=="running"{print $1; exit}')
if [ -n "$id" ]; then
    last_before=$(grep -cE '^s[0-9]+$' /tmp/agentos-e2e-snap.txt)
    "$AGENTOS" snapshot "$id" >/dev/null 2>&1
    check "state is snapshotted" "snapshotted" "$("$AGENTOS" ps | awk -v i="$id" '$1==i{print $3}')"
    wait "$snapper" 2>/dev/null
    "$AGENTOS" restore "$id" > /tmp/agentos-e2e-restored.txt 2>&1 &
    restorer=$!
    sleep 10
    # The restored guest must CONTINUE mid-execution: it picks up near where
    # it stopped (a line buffered at snapshot time may be replayed, which is
    # correct — no output is lost) and runs on past that point. What must NOT
    # happen is re-running the command from scratch at s0.
    first_after=$(grep -E '^s[0-9]+$' /tmp/agentos-e2e-restored.txt | head -1 | tr -d 's')
    last_after=$(grep -E '^s[0-9]+$' /tmp/agentos-e2e-restored.txt | tail -1 | tr -d 's')
    if [ -n "$first_after" ] && [ "$first_after" -gt 0 ] \
        && [ -n "$last_after" ] && [ "$last_after" -ge "$last_before" ]; then
        echo "PASS: restored guest resumed at s$first_after and ran to s$last_after (snapshot after s$((last_before - 1)))"
    else
        echo "FAIL: restored guest did not resume mid-execution (first=${first_after:-none}, last=${last_after:-none}, snapshot after s$((last_before - 1)))" >&2
        FAILURES=$((FAILURES + 1))
    fi
    "$AGENTOS" kill "$id" >/dev/null 2>&1
    wait "$restorer" 2>/dev/null
else
    echo "FAIL: snapshot/restore — no running sandbox found" >&2
    kill "$snapper" 2>/dev/null
    FAILURES=$((FAILURES + 1))
fi

echo "== snapshots survive a daemon restart =="
"$AGENTOS" run -- sh -c 'i=0; while [ $i -lt 120 ]; do echo "r$i"; i=$((i+1)); sleep 1; done' \
    > /tmp/agentos-e2e-persist.txt 2>&1 &
persister=$!
sleep 7
id=$("$AGENTOS" ps | awk '$3=="running"{print $1; exit}')
if [ -n "$id" ]; then
    "$AGENTOS" snapshot "$id" >/dev/null 2>&1
    wait "$persister" 2>/dev/null
    # Kill the daemon outright: the spec now lives on disk, not just in RAM.
    pkill -f "$(basename "$AGENTOS")d" 2>/dev/null; sleep 2
    check "snapshot is still listed after the daemon restarts" "snapshotted" \
        "$("$AGENTOS" ps | awk -v i="$id" '$1==i{print $3}')"
    "$AGENTOS" restore "$id" > /tmp/agentos-e2e-persist2.txt 2>&1 &
    restorer2=$!
    sleep 10
    first=$(grep -E '^r[0-9]+$' /tmp/agentos-e2e-persist2.txt | head -1 | tr -d 'r')
    if [ -n "$first" ] && [ "$first" -gt 0 ]; then
        echo "PASS: restored mid-execution (at r$first) after a daemon restart"
    else
        echo "FAIL: could not restore after daemon restart (first=${first:-none})" >&2
        FAILURES=$((FAILURES + 1))
    fi
    "$AGENTOS" kill "$id" >/dev/null 2>&1
    wait "$restorer2" 2>/dev/null
else
    echo "FAIL: snapshot persistence — no running sandbox found" >&2
    kill "$persister" 2>/dev/null
    FAILURES=$((FAILURES + 1))
fi

echo "== fleet policy: enforced in the daemon, not the client =="
POLICY_DIR=$(mktemp -d)
cat > "$POLICY_DIR/policy.json" <<'POLICY'
{"message":"contact IT","max_net":{"mode":"allowlist","hosts":["pypi.org"]},"max_vcpus":1}
POLICY
# Restart the daemon under a policy; the CLI is unchanged, proving the
# enforcement point is the daemon.
pkill -f "$(basename "$AGENTOS")d" 2>/dev/null; sleep 1
AGENTOS_POLICY="$POLICY_DIR/policy.json" "${AGENTOS}d" >/dev/null 2>&1 &
sleep 2
out=$("$AGENTOS" run --net full -- echo x 2>&1 | tail -1)
case "$out" in
    *"fleet policy forbids"*) echo "PASS: policy refuses network beyond its ceiling" ;;
    *) echo "FAIL: policy did not refuse --net full — got [$out]" >&2; FAILURES=$((FAILURES + 1)) ;;
esac
out=$("$AGENTOS" run --vcpus 8 -- sh -c 'nproc' 2>/dev/null)
check "policy clamps vcpus" "1" "$out"
check "a permitted subset still runs" "ok" "$("$AGENTOS" run --net allowlist:pypi.org -- echo ok 2>/dev/null)"
rm -rf "$POLICY_DIR"
pkill -f "$(basename "$AGENTOS")d" 2>/dev/null; sleep 1

echo "== concurrency: sandboxes are independent =="
for i in 1 2 3; do
    ("$AGENTOS" run -- sh -c "echo box-$i; sleep 4; echo box-$i-done" \
        > "/tmp/agentos-e2e-conc$i.txt" 2>&1 &)
done
sleep 9
conc_ok=yes
for i in 1 2 3; do
    grep -q "box-$i-done" "/tmp/agentos-e2e-conc$i.txt" || conc_ok=no
    # Output must not bleed between sandboxes.
    for j in 1 2 3; do
        [ "$i" = "$j" ] && continue
        grep -q "box-$j" "/tmp/agentos-e2e-conc$i.txt" && conc_ok=no
    done
done
check "three sandboxes run concurrently without interference" "yes" "$conc_ok"

echo "== a VM never outlives its daemon (fail-closed) =="
"$AGENTOS" run -- sleep 120 >/dev/null 2>&1 &
orphan_runner=$!
sleep 6
pkill -f "$(basename "$AGENTOS")d" 2>/dev/null
sleep 3
# kill_on_drop only covers an orderly exit; a SIGKILLed daemon must still take
# its VMs with it, or a sandbox outlives the thing supervising it.
leaked=$(pgrep -f 'vmhelper/agentos-vmhelper|cloud-hypervisor' | wc -l | tr -d ' ')
check "no VM leaks when the daemon dies" "0" "$leaked"
kill "$orphan_runner" 2>/dev/null; wait "$orphan_runner" 2>/dev/null

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

echo "== panic kill switch: --newest, the GUI hotkey's own code path =="
"$AGENTOS" run -- sleep 60 >/dev/null 2>&1 &
panic_runner=$!
sleep 4
out=$("$AGENTOS" kill --newest 2>&1)
wait "$panic_runner"
check "panic kill terminates the newest live sandbox" "137" "$?"
check "panic kill names what it killed" "yes" \
    "$(echo "$out" | grep -q '^killed ' && echo yes || echo no)"
"$AGENTOS" kill --newest >/dev/null 2>&1
check "panic kill with nothing running exits nonzero" "1" "$?"

# Toolchain variant: only if the opt-in image has been built.
if [ -f "$HOME/.agentos/images/rootfs-devops.squashfs" ]; then
    echo "== template devops: preloaded toolchains =="
    out=$("$AGENTOS" run --template devops -- \
        sh -c 'terraform version >/dev/null && aws --version >/dev/null && echo tools-ok' \
        2>/dev/null | tail -1)
    check "devops template ships terraform + aws cli" "tools-ok" "$out"
else
    echo "== template devops: skipped (rootfs-devops.squashfs not built) =="
    out=$("$AGENTOS" run --template devops -- true 2>&1 | tail -1)
    check "missing variant image explains how to build it" "yes" \
        "$(echo "$out" | grep -q 'build-guest-image.sh --variant devops' && echo yes || echo no)"
fi

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "e2e: all tests passed"
else
    echo "e2e: $FAILURES test(s) FAILED" >&2
    exit 1
fi
