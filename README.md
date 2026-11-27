# tinyrtc

A small real-time call library for iOS and Android: audio, video and data between peers, over
WebRTC's wire, with ONE fixed profile instead of a browser's whole media stack. It talks to itself
and to a browser. About 2 MB per architecture, where libwebrtc is about 10.

The profile: Opus; H.264 Constrained Baseline 3.1, encoded and decoded by the phone's hardware;
SCTP data channels; one transport. Every connection holds all three from its first offer, so
nothing a call does later is a renegotiation. No simulcast, no second codec, no TURN: a call that
finds no direct path crosses a relay that forwards bytes it cannot read (tests/blind_relay.rs).

Signaling is not here: an app carries three kinds of text — an offer, an answer, candidates — over
whatever channel it has.

```
core/      Rust, C ABI (include/tinyrtc.h): ICE with its STUN client, DTLS, SRTP, RTP / RTCP, bandwidth
           estimation, data channels — on str0m — plus the SDP of the profile, Opus, the audio jitter
           buffer and the mix. No platform code: it builds and is tested on a desktop.
           vendor/ = libopus, and patched crates (vendor/README.md)
apple/     Swift package: capture, VideoToolbox, the voice-processing audio unit, the picture   (to come)
android/   Gradle module `tinyrtc` (Kotlin + one JNI library holding the core) and a demo app; docs/platform-api.md
mediaserver/ the media server (mediaserver/README.md): rooms it forwards, and the blind relay of a 1:1 call; one
           UDP port, a private HTTP API for the backend of an app
tools/     peer/ = a desktop peer a real browser calls (node interop.mjs); release.sh = a release (below)
```

Who does what in a call:

```
            platform half                          core
camera  →  hardware H.264 encoder  →  frames  →  packets → network
           hardware H.264 decoder  ←  frames  ←  packets ← network     + "send at N bit/s", "keyframe now"
microphone → OS echo canceller  →  PCM  →  Opus → packets
speaker    ←                       PCM  ←  jitter buffer ← Opus ← packets   (every peer, mixed)
```

## Build and check

```
core:        cargo test --release                      every module, and whole calls on loopback UDP
tools/peer:  node interop.mjs [--loss N] [--all-h264]  a call with Chrome (headless, fake devices)
mediaserver: cargo test --release                      rooms, calls and the API, phones = core connections
             its image: mediaserver/Dockerfile (context = the repo root)
android:     ./build-core.sh, then ./gradlew :tinyrtc:assembleRelease :demo:assembleDebug;
             demo/run-against-peer.sh <pc-lan-ip> runs the demo on a phone against tools/peer
size:        cd core && cargo ndk -t arm64-v8a -t armeabi-v7a build --profile mobile \
               -Zbuild-std=std,panic_abort -Zbuild-std-features=optimize_for_size -Zpanic-immediate-abort
iOS:         on a Mac, the same with --target aarch64-apple-ios --no-default-features --features crypto-apple
```

The core is built with the Rust nightly core/rust-toolchain.toml names, with rust-src (the size build
rebuilds std); rustup installs it on the first cargo command there. The media server builds with
stable. Android: `ANDROID_NDK_HOME`, cargo-ndk.

## Releases

Every push runs the tests of the core and of the media server (.github/workflows/ci.yml). A tag
`vX.Y.Z`, on a commit whose core/Cargo.toml and mediaserver/Cargo.toml say X.Y.Z, is a release
(.github/workflows/release.yml), built on GitHub's machines:

```
ghcr.io/<owner>/tinyrtc-mediaserver:X.Y.Z  :X.Y  :latest    the media server's image
tinyrtc-android-X.Y.Z.aar                                   the Android library, two ABIs
TinyRTCCore-X.Y.Z.xcframework.zip                           the core for iOS (built with Xcode 26)
checksums.txt                                               sha256 of the two files
```

The last three are the files of the GitHub Release. The Kotlin and Swift sources of the platform
halves are the tag's android/ and apple/ folders.

`tools/release.sh X.Y.Z` makes one from a clean main: the version in both manifests and both
Cargo.lock, the commit `chore: vX.Y.Z`, the tag, and (it asks first; `--push` does not) the push.
