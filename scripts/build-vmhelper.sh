#!/bin/sh
# Build and ad-hoc-sign the macOS VM helper (requires Xcode / CLT).
# The com.apple.security.virtualization entitlement is mandatory:
# Virtualization.framework refuses to start VMs without it.
set -eu
cd "$(dirname "$0")/.."

OUT=target/vmhelper
mkdir -p "$OUT"
swiftc -O vmhelper/main.swift -o "$OUT/agentos-vmhelper" -framework Virtualization
codesign --force --sign - --entitlements vmhelper/vmhelper.entitlements "$OUT/agentos-vmhelper"
echo "built $OUT/agentos-vmhelper"
