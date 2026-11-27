#!/bin/bash
# apple/build-core.sh — the core as apple/TinyRTCCore.xcframework, on a Mac.
#
#   ./build-core.sh            both slices: aarch64-apple-ios (devices), aarch64-apple-ios-sim
#
# Per slice: the README's size recipe (nightly, std rebuilt for size, a panic that aborts on the spot,
# the CryptoKit DTLS backend), then ONE object out of the whole static library (`ld -r`) in which
# only the `tinyrtc_*` functions of the header stay global. libopus, Rust's std, str0m and the
# CryptoKit bridge become local symbols of that object: an app that links its own libopus or another
# Rust library does not collide with ours, and cannot call into ours by accident.
#
# What the final link of an app needs (all of it comes with the object as autolink options, which
# `ld -r` keeps; Package.swift names the frameworks too): the Swift runtime (in the OS since iOS 12.2;
# the CryptoKit bridge of str0m-apple-crypto is Swift, compiled by the swiftc of this Xcode), the
# toolchain's back-deployment archives swiftCompatibility56 / swiftCompatibilityPacks (Xcode adds
# their search path for any target with Swift in it), CryptoKit, Security, CoreFoundation, Foundation,
# libobjc, libSystem. A consumer must link it with an Xcode whose Swift is not older than the one that
# built it.
#
# The simulator slice is arm64 only (Apple silicon Macs): an app built for "any iOS Simulator" needs
# EXCLUDED_ARCHS[sdk=iphonesimulator*] = x86_64, or a concrete simulator as the destination.
#
# Outputs (git-ignored): apple/build/core/<slice>/, apple/TinyRTCCore.xcframework.

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
core="$here/../core"
build="$here/build/core"
xcframework="$here/TinyRTCCore.xcframework"
min_ios=15.0

trap 'echo "build-core.sh: failed at line $LINENO" >&2' ERR
if [ -f "$HOME/.cargo/env" ]; then set +u; source "$HOME/.cargo/env"; set -u; fi

# The deployment target of libopus's C (cc), of the Swift bridge (str0m-apple-crypto's build.rs) and
# of the Rust code: an app with iOS 15 links it without "built for newer iOS" warnings.
export IPHONEOS_DEPLOYMENT_TARGET=$min_ios
# core/.cargo/config.toml gives these flags to aarch64-apple-ios only; the simulator slice gets the
# same ones here (a CARGO_TARGET_<triple>_RUSTFLAGS variable is that config key).
export CARGO_TARGET_AARCH64_APPLE_IOS_SIM_RUSTFLAGS="-Zdefault-visibility=hidden -Zlocation-detail=none -Zfmt-debug=none -Cforce-unwind-tables=no"

rm -rf "$build" "$xcframework"
mkdir -p "$build/headers/TinyRTCCore"

# The header and its module map: Swift imports it as the module TinyRTCCore. In a folder of their
# own: an app's xcframeworks share one Headers search path, and two module.modulemap files at its
# root collide.
cp "$core/include/tinyrtc.h" "$build/headers/TinyRTCCore/"
cat > "$build/headers/TinyRTCCore/module.modulemap" <<'EOF'
module TinyRTCCore {
    header "tinyrtc.h"
    export *
}
EOF

# The exported functions: every function the header declares (a line outside a comment with
# `tinyrtc_x(`), with the leading underscore of Mach-O.
grep -v '^[[:space:]]*//' "$core/include/tinyrtc.h" \
  | grep -oE 'tinyrtc_[a-z0-9_]+\(' | tr -d '(' | sort -u | sed 's/^/_/' > "$build/exported.txt"
echo "exported: $(wc -l < "$build/exported.txt" | tr -d ' ') functions of the header"

slice() { # <rust target> <sdk> <ld platform> <dir>
  local target=$1 sdk=$2 platform=$3 dir="$build/$4"
  mkdir -p "$dir"
  echo "== $target"
  (cd "$core" && cargo rustc --lib --crate-type staticlib --target "$target" --profile mobile \
      -Zbuild-std=std,panic_abort -Zbuild-std-features=optimize_for_size -Zpanic-immediate-abort \
      --no-default-features --features crypto-apple)
  local lib="$dir/libtinyrtc.a"
  cp "$core/target/$target/mobile/libtinyrtc.a" "$lib"
  # std's compiler_builtins comes with its LLVM bitcode beside the machine code (a section
  # __LLVM,__bitcode, written by Rust's LLVM). Nothing uses it, and the tools of an Xcode whose LLVM
  # is older than Rust's refuse the whole file over it. Removed with Rust's own objcopy.
  "$objcopy" --remove-section=__LLVM,__bitcode --remove-section=__LLVM,__cmdline "$lib"

  # Every function of the header must be in the library, or the export list hides nothing but
  # also exports a hole.
  local missing
  missing=$(comm -23 "$build/exported.txt" <(nm -gU "$lib" | awk '$2 == "T" { print $3 }' | sort -u))
  if [ -n "$missing" ]; then echo "not in $lib: $missing" >&2; exit 1; fi

  local sdk_version
  sdk_version=$(xcrun --sdk "$sdk" --show-sdk-version)
  xcrun ld -r -arch arm64 -platform_version "$platform" "$min_ios" "$sdk_version" \
    -exported_symbols_list "$build/exported.txt" -all_load "$lib" -o "$dir/TinyRTCCore.o"
  rm -f "$dir/libTinyRTCCore.a"
  xcrun libtool -static -no_warning_for_no_symbols -o "$dir/libTinyRTCCore.a" "$dir/TinyRTCCore.o"

  # Nothing but the header's functions is defined globally.
  local globals
  globals=$(nm -gU "$dir/TinyRTCCore.o" | awk '{ print $3 }' | sort -u)
  if [ "$globals" != "$(cat "$build/exported.txt")" ]; then
    echo "unexpected global symbols in $dir/TinyRTCCore.o:" >&2
    comm -13 "$build/exported.txt" <(echo "$globals") >&2
    exit 1
  fi

  # What an app gets from it: a dynamic library linked from the object alone with dead stripping,
  # the way the app's own link strips it (code + data, not the symbol table).
  xcrun -sdk "$sdk" swiftc -emit-library -target "arm64-apple-ios$min_ios$( [ "$sdk" = iphonesimulator ] && echo -simulator )" \
    "$dir/TinyRTCCore.o" -o "$dir/probe.dylib" \
    -Xlinker -dead_strip -Xlinker -exported_symbols_list -Xlinker "$build/exported.txt"
  local archive linked
  archive=$(stat -f%z "$dir/libTinyRTCCore.a")
  linked=$(size -m "$dir/probe.dylib" | awk '/^Segment __(TEXT|DATA|DATA_CONST):/ { next } /^\ttotal/ { t += $2 } END { print t }')
  echo "$4: libTinyRTCCore.a $archive bytes; linked into an app (dead-stripped code + data): $linked bytes"
  echo "$4 archive=$archive linked=$linked" >> "$build/sizes.txt"
}

# llvm-objcopy of the toolchain core/rust-toolchain.toml names (its component llvm-tools).
objcopy="$(cd "$core" && rustc --print sysroot)/lib/rustlib/$(cd "$core" && rustc -vV | sed -n 's/^host: //p')/bin/llvm-objcopy"
[ -x "$objcopy" ] || { echo "build-core.sh: no $objcopy (rustup component add llvm-tools)" >&2; exit 1; }

: > "$build/sizes.txt"
slice aarch64-apple-ios     iphoneos        ios           ios-arm64
slice aarch64-apple-ios-sim iphonesimulator ios-simulator ios-arm64-simulator

xcodebuild -create-xcframework \
  -library "$build/ios-arm64/libTinyRTCCore.a" -headers "$build/headers" \
  -library "$build/ios-arm64-simulator/libTinyRTCCore.a" -headers "$build/headers" \
  -output "$xcframework" >/dev/null
echo "== $xcframework"
cat "$build/sizes.txt"
