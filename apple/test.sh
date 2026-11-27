#!/bin/bash
# apple/test.sh — the package's checks, on a Mac.
#
#   ./test.sh          the tests on an iPhone simulator: (i) the bindings, (ii) the codec path, the
#                      audio pieces; the peer test skips itself
#   ./test.sh peer     (iii) a call with tools/peer on 127.0.0.1 (ports 18480 http, 18481 udp): the
#                      peer is built and started here, the test gets its URL, the peer's final JSON
#                      line is printed and checked
#   ./test.sh build    the library in Debug and Release, for devices and the simulator
#
# TINYRTC_SIM=<udid> picks the simulator (default: an iPhone 18 Pro, else the newest iPhone).
# Outputs (git-ignored): apple/build/DerivedData, apple/build/logs.

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here"
source "$HOME/.cargo/env" 2>/dev/null || true
[ -d TinyRTCCore.xcframework ] || ./build-core.sh

derived="$here/build/DerivedData"
logs="$here/build/logs"
mkdir -p "$logs"

sim="${TINYRTC_SIM:-}"
if [ -z "$sim" ]; then
  list=$(xcrun simctl list devices available)
  sim=$(echo "$list" | grep -m1 -E '^ +iPhone 18 Pro \(' | grep -oE '[0-9A-F-]{36}' || true)
  if [ -z "$sim" ]; then
    sim=$(echo "$list" | awk '/^-- iOS /{first=""} /^ +iPhone/ && first=="" {match($0, /[0-9A-F-]{36}/); first=substr($0, RSTART, RLENGTH)} END{print first}')
  fi
fi
destination="id=$sim"

summary() { # the lines a reader wants out of an xcodebuild log
  grep -E '^Test Case .*(passed|failed|skipped)|error:|warning: -|Executed|\*\* (TEST|BUILD)|^(encoder|decoder|sps|stats|echoed|channel|voice-processing|frame rate|remote media|after stop|b after|a requested|CryptoKit|openCamera)' "$1" || true
}

case "${1:-unit}" in
build)
  for configuration in Debug Release; do
    for platform in "iOS" "iOS Simulator"; do
      log="$logs/build-$configuration-${platform// /}.log"
      xcodebuild build -scheme TinyRTC -configuration "$configuration" -destination "generic/platform=$platform" \
        -derivedDataPath "$derived" CODE_SIGNING_ALLOWED=NO > "$log" 2>&1 || true
      echo "$configuration / $platform: $(grep -c ': warning:' "$log") warnings, $(grep -c ': error:' "$log") errors, $(grep -E '\*\* BUILD' "$log")"
    done
  done
  # The demo app, unsigned: for the simulator (arm64: the core has no x86_64 slice) and a device.
  for target in "Debug|id=$sim" "Release|generic/platform=iOS"; do
    configuration=${target%%|*}
    log="$logs/demo-$configuration.log"
    (cd Demo/TinyRTCDemo.swiftpm && xcodebuild build -scheme TinyRTCDemo -configuration "$configuration" \
      -destination "${target#*|}" -derivedDataPath "$derived" CODE_SIGNING_ALLOWED=NO > "$log" 2>&1) || true
    echo "demo $configuration / ${target#*|}: $(grep -c ': warning:' "$log") warnings, $(grep -c ': error:' "$log") errors, $(grep -E '\*\* BUILD' "$log")"
  done
  ;;
unit)
  xcodebuild test -scheme TinyRTC -destination "$destination" -derivedDataPath "$derived" > "$logs/test.log" 2>&1 || true
  summary "$logs/test.log"
  ;;
peer)
  peer="$here/../tools/peer"
  (cd "$peer" && cargo build --release -q)
  xcodebuild build-for-testing -scheme TinyRTC -destination "$destination" -derivedDataPath "$derived" > "$logs/peer-build.log" 2>&1
  xcrun simctl boot "$sim" 2>/dev/null || true
  xcrun simctl bootstatus "$sim" -b > /dev/null

  # The peer runs on its own clock: long enough for the test's launch, its call and slack.
  "$peer/target/release/tinyrtc-peer" --host 127.0.0.1 --http-port 18480 --udp-port 18481 --seconds 60 \
    > "$logs/peer.jsonl" 2> "$logs/peer.log" &
  pid=$!
  trap 'kill $pid 2>/dev/null || true' EXIT
  for _ in $(seq 50); do grep -q listening "$logs/peer.jsonl" 2>/dev/null && break; sleep 0.1; done
  head -1 "$logs/peer.jsonl"

  TEST_RUNNER_TINYRTC_PEER=http://127.0.0.1:18480 xcodebuild test-without-building -scheme TinyRTC \
    -destination "$destination" -derivedDataPath "$derived" -only-testing:TinyRTCTests/PeerInteropTests \
    > "$logs/peer-test.log" 2>&1 || true
  summary "$logs/peer-test.log"

  echo "waiting for the peer's final line"
  wait $pid || true
  trap - EXIT
  final=$(grep '"type":"final"' "$logs/peer.jsonl" || true)
  echo "$final"
  value() { echo "$final" | grep -oE "\"$1\":[^,}]*" | cut -d: -f2- | tr -d '"'; }
  ok=1
  [ "$(value connected)" = true ] || { echo "peer: not connected"; ok=0; }
  [ "$(value video_frames)" -gt 150 ] 2>/dev/null || { echo "peer: video_frames $(value video_frames)"; ok=0; }
  [ "$(value video_echoed)" -gt 150 ] 2>/dev/null || { echo "peer: video_echoed $(value video_echoed)"; ok=0; }
  [ "$(value audio_frames)" -gt 300 ] 2>/dev/null || { echo "peer: audio_frames $(value audio_frames)"; ok=0; }
  [ "$(value channel_messages)" -gt 30 ] 2>/dev/null || { echo "peer: channel_messages $(value channel_messages)"; ok=0; }
  [ "$(value echo_errors)" = 0 ] || { echo "peer: echo_errors $(value echo_errors)"; ok=0; }
  [ $ok = 1 ] && echo "peer counters: ok" || { echo "peer counters: FAILED"; exit 1; }
  ;;
*)
  echo "usage: $0 [unit|peer|build]" >&2
  exit 2
  ;;
esac
