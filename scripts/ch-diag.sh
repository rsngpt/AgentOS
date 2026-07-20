#!/bin/sh
# Diagnostic: start a sandbox that stays alive, then dump the VMM's own stderr
# (helper.log = cloud-hypervisor) and the guest console while the sandbox dir
# still exists (it's wiped on exit). Reveals why overlay writes fail on CH.
set -u
AGENTOS="${1:-target/debug/agentos}"
"$AGENTOS" run -- sh -c 'echo guest-alive; sleep 15' &
sleep 6
for d in "$HOME"/.agentos/sandboxes/*/; do
  [ -d "$d" ] || continue
  echo "=== sandbox $d ==="
  echo "--- helper.log (cloud-hypervisor stderr) ---"
  cat "$d/helper.log" 2>/dev/null | tail -40
  echo "--- console.log (guest) ---"
  cat "$d/console.log" 2>/dev/null | grep -iE 'guest-agent|ext4|overlay|error|vdb' | tail -30
done
wait || true
