#!/usr/bin/env bash
# Kill the gateway test fixtures that `cargo test --workspace` can leak:
# codex app-server stand-ins that spin in `while :; do :; done` after the
# test that spawned them is gone. Safe to run anytime; matches only those.
set -euo pipefail
n=0
for p in $(ps -eo pid=,comm= | awk '$2=="sh"{print $1}'); do
  if tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | grep -q 'synthetic fixture'; then
    kill -9 "$p" 2>/dev/null && n=$((n + 1))
  fi
done
echo "reaped $n leaked test fixture(s)"
