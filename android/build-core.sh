#!/usr/bin/env bash
# The core for Android: the README "size" build (profile `mobile`, std rebuilt for size, a panic that
# aborts on the spot) for both ABIs, and each libtinyrtc.a copied to tinyrtc/core-lib/<abi>/, where
# the JNI shim's CMakeLists.txt links it (git-ignored). Run it again after any change in core/.
#
#   ANDROID_NDK_HOME=<sdk>/ndk/27.0.12077973 ./build-core.sh
#
# Needs: cargo-ndk; the Rust nightly is the one core/rust-toolchain.toml names.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
core="$here/../core"
out="$here/tinyrtc/core-lib"

if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  echo "build-core.sh: set ANDROID_NDK_HOME (e.g. \$LOCALAPPDATA/Android/Sdk/ndk/27.0.12077973)" >&2
  exit 2
fi

# The library's minSdk: the .a calls nothing newer than that.
platform=23

cd "$core"
cargo ndk -t arm64-v8a -t armeabi-v7a -P "$platform" build --profile mobile \
  -Zbuild-std=std,panic_abort -Zbuild-std-features=optimize_for_size -Zpanic-immediate-abort

for pair in arm64-v8a:aarch64-linux-android armeabi-v7a:armv7-linux-androideabi; do
  abi=${pair%%:*}
  triple=${pair#*:}
  mkdir -p "$out/$abi"
  cp "$core/target/$triple/mobile/libtinyrtc.a" "$out/$abi/libtinyrtc.a"
  echo "build-core.sh: $out/$abi/libtinyrtc.a ($(wc -c < "$out/$abi/libtinyrtc.a") bytes)"
done
