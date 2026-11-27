#!/usr/bin/env bash
# The demo against tools/peer on the attached phone, the way the phone is to be used: no tapping, ONE
# launch, ONE logcat dump at the end (never a live log). From android/, after
#   ./gradlew :demo:assembleDebug   and   (cd ../tools/peer && cargo build --release):
#
#   demo/run-against-peer.sh <pc-lan-ip> [--reverse] [--no-install]
#
# The phone must reach <pc-lan-ip> over UDP (same LAN): the peer answers with that address, and its
# own checks toward the phone's candidate open the PC firewall's state for the phone's packets.
# --reverse: the signaling (one HTTP POST) goes over `adb reverse` and a loopback forwarder (node)
#   instead, for a PC whose firewall refuses inbound TCP to tinyrtc-peer; the media still crosses
#   the LAN.
# --no-install: the APK is installed already (a Xiaomi phone asks for a tap on every adb install:
#   install it by hand first, `adb install -r demo/build/outputs/apk/debug/demo-debug.apk`).
set -u
host=${1:?usage: run-against-peer.sh <pc-lan-ip> [--reverse] [--no-install]}
shift
reverse=0
install=1
for a in "$@"; do
  case $a in
    --reverse) reverse=1 ;;
    --no-install) install=0 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

here="$(cd "$(dirname "$0")" && pwd)"
out="$here/build/peer-run"
mkdir -p "$out"
# adb and node are Windows programs under Git Bash: hand them Windows paths, and keep MSYS from
# rewriting the phone's paths.
native() { if command -v cygpath > /dev/null; then cygpath -m "$1"; else echo "$1"; fi; }
apk="$(native "$here/build/outputs/apk/debug/demo-debug.apk")"
peer="$here/../../tools/peer/target/release/tinyrtc-peer"
[ -x "$peer" ] || peer="$peer.exe"
http=18795
seconds=20

if [ $install = 1 ]; then
  adb install -r "$apk" || { echo "install failed (a phone that asks for a tap: --no-install)" >&2; exit 1; }
fi
MSYS_NO_PATHCONV=1 adb shell pm list packages dev.tinyrtc.demo | grep -q dev.tinyrtc.demo || { echo "dev.tinyrtc.demo is not installed" >&2; exit 1; }
adb shell pm grant dev.tinyrtc.demo android.permission.CAMERA
adb shell pm grant dev.tinyrtc.demo android.permission.RECORD_AUDIO

"$peer" --host "$host" --http-port $http --seconds 40 > "$out/peer.out" 2> "$out/peer.err" &
peer_pid=$!
url="http://$host:$http"
if [ $reverse = 1 ]; then
  url="http://127.0.0.1:18790"
  adb reverse tcp:18790 tcp:18790
  node -e "
    const net = require('net');
    net.createServer((c) => { const u = net.connect($http, '$host'); c.pipe(u); u.pipe(c);
      c.on('error', () => u.destroy()); u.on('error', () => c.destroy()); }).listen(18790, '127.0.0.1');
    setTimeout(() => process.exit(0), 45000);" &
fi
sleep 2

adb logcat -c
MSYS_NO_PATHCONV=1 adb shell am start -n dev.tinyrtc.demo/.MainActivity -e peer "$url" --ei seconds $seconds
sleep 25
adb logcat -d -s 'tinyrtc-demo:*' 'tinyrtc:*' 'AndroidRuntime:E' > "$out/logcat.txt"
adb shell am force-stop dev.tinyrtc.demo
wait $peer_pid

echo "--- peer, final:"
tail -1 "$out/peer.out"
echo "--- demo, last lines (all of it: $out/logcat.txt):"
grep tinyrtc-demo "$out/logcat.txt" | tail -4
