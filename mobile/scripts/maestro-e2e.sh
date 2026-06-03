#!/usr/bin/env bash
#
# Run the Maestro e2e flows against one or more simulators/emulators in
# parallel, backed by a fully-offline mock lair (no API spend, no network).
#
# It will:
#   1. boot the mock lair (the `maestro_serve` ignored test) and wait until it
#      is serving on :9000 with the dev keypair;
#   2. discover booted iOS Simulators and/or running Android emulators;
#   3. run `mobile/.maestro/smoke.yaml` against each device concurrently, with
#      HOST=127.0.0.1 for iOS and HOST=10.0.2.2 for Android;
#   4. tear the mock lair down on exit.
#
# Prerequisites (this script does NOT build/install the app):
#   - `maestro` on PATH                 (https://maestro.mobile.dev)
#   - the okto app already built+installed on each target device
#       iOS:     npm run -w mobile ios       (or build in Xcode onto a sim)
#       Android: npm run -w mobile android    (onto a running emulator)
#   - at least one booted iOS Simulator and/or running Android emulator
#
# Usage:
#   mobile/scripts/maestro-e2e.sh                # all booted iOS + Android devices
#   mobile/scripts/maestro-e2e.sh --ios          # iOS Simulators only
#   mobile/scripts/maestro-e2e.sh --android      # Android emulators only
#   FLOW=mobile/.maestro/connect.yaml mobile/scripts/maestro-e2e.sh   # a specific flow
#
set -uo pipefail

# ── locate the repo root (this script lives in mobile/scripts/) ────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FLOW="${FLOW:-$REPO_ROOT/mobile/.maestro/smoke.yaml}"

WANT_IOS=1
WANT_ANDROID=1
case "${1:-}" in
  --ios)     WANT_ANDROID=0 ;;
  --android) WANT_IOS=0 ;;
  "" )       ;;
  *) echo "unknown arg: $1 (use --ios | --android)"; exit 2 ;;
esac

command -v maestro >/dev/null 2>&1 || { echo "error: 'maestro' not on PATH — see https://maestro.mobile.dev"; exit 1; }

# ── 1. boot the mock lair ──────────────────────────────────────────────────────
HARNESS_LOG="$(mktemp -t maestro-lair.XXXXXX.log)"
echo "▶ starting mock lair (offline) … log: $HARNESS_LOG"
(
  cd "$REPO_ROOT" &&
  exec cargo test -p okto-tests --test maestro_serve serve -- --ignored --nocapture
) >"$HARNESS_LOG" 2>&1 &
HARNESS_PID=$!

cleanup() {
  echo "▶ stopping mock lair (pid $HARNESS_PID)"
  # kill the whole process group so the spawned `lair` child dies too
  kill "$HARNESS_PID" 2>/dev/null
  pkill -P "$HARNESS_PID" 2>/dev/null
  pkill -f "deps/maestro_serve" 2>/dev/null
  pkill -f "target/.*/lair --role lair" 2>/dev/null
  rm -f "$HARNESS_LOG"
}
trap cleanup EXIT INT TERM

echo "▶ waiting for lair to become ready (builds lair on first run) …"
for _ in $(seq 1 150); do
  if grep -q "is READY and serving" "$HARNESS_LOG" 2>/dev/null; then break; fi
  if ! kill -0 "$HARNESS_PID" 2>/dev/null; then
    echo "error: mock lair exited early:"; tail -30 "$HARNESS_LOG"; exit 1
  fi
  sleep 2
done
grep -q "is READY and serving" "$HARNESS_LOG" || { echo "error: lair not ready in time:"; tail -30 "$HARNESS_LOG"; exit 1; }
echo "✓ mock lair ready on :9000"

# ── 2. discover devices ────────────────────────────────────────────────────────
declare -a RUN_LABELS=()   # "ios:<udid>" / "android:<serial>"
if [ "$WANT_IOS" = 1 ] && command -v xcrun >/dev/null 2>&1; then
  while IFS= read -r udid; do
    [ -n "$udid" ] && RUN_LABELS+=("ios:$udid")
  done < <(xcrun simctl list devices booted 2>/dev/null | grep -oE '\(([0-9A-F-]{36})\)' | tr -d '()')
fi
if [ "$WANT_ANDROID" = 1 ] && command -v adb >/dev/null 2>&1; then
  while IFS= read -r serial; do
    [ -n "$serial" ] && RUN_LABELS+=("android:$serial")
  done < <(adb devices 2>/dev/null | awk '/^emulator-|\tdevice$/{print $1}' | grep -v '^$')
fi

if [ "${#RUN_LABELS[@]}" = 0 ]; then
  echo "error: no booted iOS Simulators or running Android emulators found."
  echo "  iOS:     xcrun simctl boot <name>   then  npm run -w mobile ios"
  echo "  Android: start an emulator          then  npm run -w mobile android"
  exit 1
fi

echo "▶ running '$FLOW' on ${#RUN_LABELS[@]} device(s): ${RUN_LABELS[*]}"

# ── 3. fan out maestro runs in parallel ────────────────────────────────────────
declare -a PIDS=() NAMES=()
for label in "${RUN_LABELS[@]}"; do
  platform="${label%%:*}"
  device="${label#*:}"
  if [ "$platform" = ios ]; then host=127.0.0.1; else host=10.0.2.2; fi
  out="$(mktemp -t maestro-$platform.XXXXXX.log)"
  echo "  → $platform $device (HOST=$host) … log: $out"
  ( maestro --device "$device" test "$FLOW" --env HOST="$host" ) >"$out" 2>&1 &
  PIDS+=("$!"); NAMES+=("$platform:$device|$out")
done

# ── 4. collect results ─────────────────────────────────────────────────────────
fail=0
for i in "${!PIDS[@]}"; do
  if wait "${PIDS[$i]}"; then status=PASS; else status=FAIL; fail=1; fi
  name="${NAMES[$i]%%|*}"; log="${NAMES[$i]#*|}"
  echo "──────── $status  $name ────────"
  tail -8 "$log"
  rm -f "$log"
done

[ "$fail" = 0 ] && echo "✓ all device runs passed" || echo "✗ one or more device runs failed"
exit "$fail"
