// swift-tools-version: 5.9
//
// An app package (Xcode / Swift Playgrounds format): open this folder in Xcode, pick a team under
// Signing & Capabilities, run on a device. See ../README.md.

import AppleProductTypes
import PackageDescription

let package = Package(
    name: "TinyRTCDemo",
    platforms: [.iOS("16.0")],
    products: [
        .iOSApplication(
            name: "TinyRTCDemo",
            targets: ["TinyRTCDemo"],
            bundleIdentifier: "codes.le.tinyrtc.demo",
            teamIdentifier: "",
            displayVersion: "1.0",
            bundleVersion: "1",
            appIcon: .placeholder(icon: .camera),
            accentColor: .presetColor(.blue),
            supportedDeviceFamilies: [.phone, .pad],
            supportedInterfaceOrientations: [.portrait, .landscapeLeft, .landscapeRight],
            capabilities: [
                .camera(purposeString: "The call sends the camera to the test peer, which sends it back."),
                .microphone(purposeString: "The call sends the microphone to the test peer, which sends it back."),
                .localNetwork(purposeString: "The test peer runs on a computer of the local network."),
            ]
        ),
    ],
    dependencies: [
        // The TinyRTC package (apple/), with TinyRTCCore.xcframework built by apple/build-core.sh.
        .package(path: "../.."),
    ],
    targets: [
        .executableTarget(
            name: "TinyRTCDemo",
            dependencies: [.product(name: "TinyRTC", package: "apple")],
            path: "."
        ),
    ]
)
