#!/usr/bin/env bash
# macOS hardened-runtime check: proves the runtime's JIT code runs under the
# hardened runtime with ONLY `com.apple.security.cs.allow-jit`, the entitlement
# the shipped HelloHQ app carries (hellohq macos/Runner/Release.Production.entitlements).
#
# Builds examples/hardened_runtime_check.rs (Cranelift core module + the real
# hello-world plugin, compiled and precompiled, through the app's C ABI), then
# codesigns copies of it with `--options runtime` and runs:
#
#   control    not hardened                   -> must PASS (the binary works)
#   allow-jit  hardened + allow-jit only      -> must PASS (code is in MAP_JIT memory)
#   no-jit     hardened, no entitlements      -> must NOT pass (MAP_JIT is refused,
#                                                or the kernel kills the process)
#
# Before the MAP_JIT fix the allow-jit case was killed with
# `SIGKILL (Code Signature Invalid)`, exactly like the app (G0.4, 2026-10-17).
#
# Usage: scripts/macos-hardened-runtime-check.sh [--release]
# Env:   CODESIGN_IDENTITY  signing identity (default "-", ad-hoc)
#        CARGO_TARGET_DIR   honoured as usual
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macos-hardened-runtime-check: macOS only" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE_FLAG=()
PROFILE_DIR=debug
if [[ "${1:-}" == "--release" ]]; then
  PROFILE_FLAG=(--release)
  PROFILE_DIR=release
fi
IDENTITY="${CODESIGN_IDENTITY:--}"

# hello-world v1.0.1 (HelloHQ/plugin-sdk release hello-world-v1.0.1).
FIXTURE=tests/fixtures/hello_world_plugin.component.wasm
FIXTURE_SHA256=eaf371ccba5fa6203464b7d2f663a95bc7d94e5d7af6b51a191f6ec8ac8c39ff
actual="$(shasum -a 256 "$FIXTURE" | awk '{print $1}')"
if [[ "$actual" != "$FIXTURE_SHA256" ]]; then
  echo "FAIL: $FIXTURE sha256 $actual, expected $FIXTURE_SHA256" >&2
  exit 1
fi

cargo build --example hardened_runtime_check --features typed-hosts --locked ${PROFILE_FLAG[@]+"${PROFILE_FLAG[@]}"}
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET_DIR/$PROFILE_DIR/examples/hardened_runtime_check"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/hwr-hardened.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

cat >"$WORK/allow-jit.entitlements" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.cs.allow-jit</key>
  <true/>
</dict>
</plist>
PLIST

# sign <name> [entitlements-file]: a hardened-runtime copy of the binary.
sign() {
  local name="$1" ents="${2:-}"
  cp "$BIN" "$WORK/$name"
  if [[ -n "$ents" ]]; then
    codesign --force --sign "$IDENTITY" --options runtime --entitlements "$ents" "$WORK/$name"
  else
    codesign --force --sign "$IDENTITY" --options runtime "$WORK/$name"
  fi
  # Capture first: with pipefail, `grep -q` exiting early would SIGPIPE codesign.
  local info
  info="$(codesign -dv "$WORK/$name" 2>&1)"
  if [[ "$info" != *"flags="*"runtime"* ]]; then
    echo "FAIL: $name is not signed with the hardened runtime" >&2
    exit 1
  fi
}

# run <name>: runs it, sets RC and OUT (stdout+stderr).
run() {
  set +e
  OUT="$("$WORK/$1" 2>&1)"
  RC=$?
  set -e
}

failures=0

cp "$BIN" "$WORK/control"
run control
if [[ $RC -eq 0 && "$OUT" == *"hardened-runtime-check: PASS"* ]]; then
  echo "PASS  control (not hardened): runs"
else
  echo "FAIL  control (not hardened): exit $RC"; echo "$OUT"; failures=$((failures + 1))
fi

sign allow-jit "$WORK/allow-jit.entitlements"
ents="$(codesign -d --entitlements - "$WORK/allow-jit" 2>&1)"
if [[ "$ents" != *"com.apple.security.cs.allow-jit"* ]]; then
  echo "FAIL: allow-jit copy lacks the allow-jit entitlement" >&2
  exit 1
fi
if [[ "$ents" == *"allow-unsigned-executable-memory"* ]]; then
  echo "FAIL: allow-jit copy must not carry allow-unsigned-executable-memory" >&2
  exit 1
fi
run allow-jit
if [[ $RC -eq 0 && "$OUT" == *"hardened-runtime-check: PASS"* ]]; then
  echo "PASS  hardened + allow-jit only: guest code runs"
  echo "$OUT" | sed 's/^/        /'
else
  how="exit $RC"; [[ $RC -eq 137 ]] && how="killed (SIGKILL, Code Signature Invalid)"
  echo "FAIL  hardened + allow-jit only: $how"; echo "$OUT"; failures=$((failures + 1))
fi

sign no-jit
run no-jit
if [[ $RC -eq 0 ]]; then
  echo "FAIL  hardened, no entitlements: guest code ran without allow-jit"
  failures=$((failures + 1))
elif [[ $RC -eq 137 ]]; then
  echo "PASS  hardened, no entitlements: killed by the kernel (SIGKILL)"
else
  echo "PASS  hardened, no entitlements: runtime refused (exit $RC): $(echo "$OUT" | tail -1)"
fi

if [[ $failures -ne 0 ]]; then
  echo "macos-hardened-runtime-check: $failures case(s) FAILED" >&2
  exit 1
fi
echo "macos-hardened-runtime-check: all cases passed"
