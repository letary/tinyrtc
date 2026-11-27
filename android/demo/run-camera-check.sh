#!/usr/bin/env bash
# The camera track's own checks on the attached phone (CameraCheck.kt), no peer: no tapping, ONE
# launch, ONE logcat dump at the end (never a live log). From android/, after ./gradlew :demo:assembleDebug:
#
#   demo/run-camera-check.sh [--views N] [--apply-camera <id>] [--no-install]
#
# --views N: the camera in N views at once (default 2). --apply-camera <id>: also switch the track to
# that camera (the ids: the check line "cameras"). --no-install: the APK is installed already (a
# phone that asks for a tap on every adb install).
# Every step logs {"event":"check","step":...,"ok":...}; the summary below lists them.
set -u
views=2
camera=""
install=1
while [ $# -gt 0 ]; do
  case $1 in
    --views) views=${2:?}; shift ;;
    --apply-camera) camera=${2:?}; shift ;;
    --no-install) install=0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

here="$(cd "$(dirname "$0")" && pwd)"
out="$here/build/camera-check"
mkdir -p "$out"
native() { if command -v cygpath > /dev/null; then cygpath -m "$1"; else echo "$1"; fi; }
apk="$(native "$here/build/outputs/apk/debug/demo-debug.apk")"

if [ $install = 1 ]; then
  adb install -r "$apk" || { echo "install failed (a phone that asks for a tap: --no-install)" >&2; exit 1; }
fi
adb shell pm grant dev.tinyrtc.demo android.permission.CAMERA
adb shell pm grant dev.tinyrtc.demo android.permission.RECORD_AUDIO

adb logcat -c
extra=()
[ -n "$camera" ] && extra=(-e applyCamera "$camera")
MSYS_NO_PATHCONV=1 adb shell am start -n dev.tinyrtc.demo/.MainActivity -e check camera --ei views "$views" ${extra[@]+"${extra[@]}"}
sleep 45
adb logcat -d -s 'tinyrtc-demo:*' 'tinyrtc:*' 'AndroidRuntime:E' > "$out/logcat.txt"
adb shell am force-stop dev.tinyrtc.demo

echo "--- checks (all of the log: $out/logcat.txt):"
grep -o '{.*"event":"check".*}' "$out/logcat.txt" || echo "no check line: the app did not run (see the log)"
grep -q '"step":"done"' "$out/logcat.txt" || echo "--- no \"done\" line: the checks did not finish in 45 s"
