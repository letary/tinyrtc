// swift-tools-version: 5.9
//
// TinyRTC: the iOS half of a call (docs/platform-api.md). The core comes in as the binary target
// TinyRTCCore, built by build-core.sh; everything the OS owns — the camera, VideoToolbox, the
// voice-processing audio unit, the picture — is the Swift target TinyRTC.

import PackageDescription

let strict: [SwiftSetting] = [.enableExperimentalFeature("StrictConcurrency")]

let package = Package(
    name: "TinyRTC",
    platforms: [.iOS(.v15)],
    products: [.library(name: "TinyRTC", targets: ["TinyRTC"])],
    targets: [
        // ./build-core.sh makes it (git-ignored).
        .binaryTarget(name: "TinyRTCCore", path: "TinyRTCCore.xcframework"),
        .target(
            name: "TinyRTC",
            dependencies: ["TinyRTCCore"],
            swiftSettings: strict,
            linkerSettings: [
                // The core: its DTLS runs on CryptoKit / Security through a Swift bridge (the Swift
                // runtime and the bridge's libraries come as autolink options of the object).
                .linkedFramework("CryptoKit"),
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("Foundation"),
                // This half.
                .linkedFramework("AVFoundation"),
                .linkedFramework("AudioToolbox"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("CoreVideo"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("Network"),
                .linkedFramework("UIKit"),
            ]
        ),
        .testTarget(name: "TinyRTCTests", dependencies: ["TinyRTC"], swiftSettings: strict),
    ],
    swiftLanguageVersions: [.v5]
)
