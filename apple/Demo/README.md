# TinyRTCDemo

A call to `tools/peer` with the real camera and microphone: the peer sends both back, so the big
picture is your own camera after a round trip (encoded on the phone, decoded on the phone), and you
hear yourself through the voice-processing unit (echo cancelled: with the speaker on, expect the
echo of your voice, not a feedback howl). The small picture is the local preview; the line under
it is the connection's numbers, once a second.

## Run it

1. The core, once (and after a change of `core/`): on the Mac, `apple/build-core.sh`.
2. The peer, on any computer of the phone's network:

   ```
   cd tools/peer && cargo build --release
   ./target/release/tinyrtc-peer --host <that computer's LAN address> --http-port 18480 --seconds 3600
   ```

   (`--host` must be the address the phone reaches: the peer offers only that one candidate.)
3. Open `apple/Demo/TinyRTCDemo.swiftpm` in Xcode (File > Open, the folder itself: it is an app
   package; Xcode resolves the local TinyRTC package at `../..`).
4. Signing: select the TinyRTCDemo target > Signing & Capabilities > Team = your team. The bundle id
   `codes.le.tinyrtc.demo` may need changing to one your team can sign (Package.swift,
   `bundleIdentifier`; or set the team there with `teamIdentifier`).
5. Pick the phone as the destination, Run. Type `http://<peer address>:18480`, tap Call. Allow the
   camera, the microphone and the local network when asked.

The camera, microphone and local-network usage strings come from `Package.swift` (`capabilities`).
The signaling goes over a plain TCP socket (`Signaling.swift`), so no App Transport Security
exception is needed for the peer's `http://`.

## What to look at

- The echo upright in every orientation of the phone, both cameras (Flip camera): the rotation is
  sent with each frame and applied by the receiving view, not baked into the pixels. The echo
  comes back from `tools/peer` without the rotation extension, so here the big picture shows the
  frames as the sensor gives them (sideways in portrait); upright rotation shows between two
  tinyrtc ends.
- Speaker / earpiece, a Bluetooth headset, a phone call coming in (the call pauses and comes back),
  the app in the background and back (the camera pauses; the picture asks for a keyframe on return).
- The numbers: the video bitrate and size climbing as the bandwidth estimate grows (240p first,
  up to 720p), the audio jitter buffer.
