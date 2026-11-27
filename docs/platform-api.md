# The platform halves

`apple/` (a Swift package) and `android/` (a Gradle library module) are the half of a call the OS
owns: the camera, the hardware H.264 codec, the audio unit with its echo canceller, the picture.
Everything else is the core, reached through `core/include/tinyrtc.h` and nowhere else. Both halves
expose the SAME objects, each in its language's idiom; an app (or a plugin of some framework) never
sees a C handle.

## The objects

| Object | Owns | Surface |
|---|---|---|
| `RtcEngine` — one per process | the core context; the audio unit; the audio route; the watch on network changes | `devices()` · `openCamera(options)` · `openMicrophone()` · `connect(config)` · `audioOutput` (earpiece / speaker) · `onDevicesChanged` |
| `LocalVideoTrack` | the camera session, the hardware encoder, the core video source | `apply(options)` (other camera, size, rate — the track stays; when that camera fails, the track is back on the one it had and `apply` throws; when that one fails too, the track ends) · `settings` · `maxBitrate` (the ceiling of the stream, default 1 500 000; the size and the rate under it follow the network) · `isEnabled` · `stop()` · `onEnded` |
| `LocalAudioTrack` | the microphone's share of the audio unit | `isEnabled` · `stop()` · `onEnded` |
| `RtcConnection` | one peer (a core connection) | `createOffer(iceRestart)` · `acceptOffer(sdp)` · `acceptAnswer(sdp)` · `addIceCandidate(line)` · `setTrack(kind, track?)` · `remoteAudio` · `remoteVideo` · `createDataChannel(label, options)` · `stats()` · `close()` · events: state, iceCandidate, iceRestartNeeded, remoteMedia, dataChannel |
| `RtcDataChannel` | one channel of a connection | `label` · `send(text)` · `send(bytes)` · `bufferedAmount()` · `bufferedAmountLowThreshold` · `close()` · events: open, message, bufferedAmountLow, close |
| `RemoteVideoTrack` / `RemoteAudioTrack` | what one peer sends; they exist from the connection's creation | video: attached to views · audio: `isMuted` (left out of the mix) |
| `RtcVideoView` — a `UIView` / an Android `View` | the picture of one track, local or remote; a track may be in several views at once | `track` · `objectFit` (cover / contain) · `mirror` · `onResize(width, height)` |

`CameraOptions`: `deviceId?`, `facing` (user / environment, default user), `width` × `height`
(default 1280 × 720), `frameRate` (default 30). The half picks the closest format the camera has.

## The rules both halves follow

- **Threads.** Public methods are called on the main thread and never block it: what waits on the
  core (the calls the header marks SYNC) runs on a worker and comes back as `async` (Swift) /
  `suspend` (Kotlin). Events are delivered on the main thread. The core's callbacks arrive on core
  threads: copy, hop, return — never call a SYNC function from inside one.
- **Lifetimes.** `close()` / `stop()` is explicit and idempotent, and after it returns no event of
  that object arrives. An object dropped without it cleans up the same way. A waiting call whose
  caller is cancelled (a coroutine, a Task) leaves nothing behind: what it made by then — a camera
  opened or opening, a core handle, a data channel — is let go of; what it did in the core (an
  offer made, an answer taken) stays done.
- **Audio** runs while at least one connection sends the microphone or exists to be heard, and stops
  with the last. 48 kHz mono int16 both ways, through the OS's voice processing (echo canceller,
  noise suppression, gain): `tinyrtc_audio_push` from the capture side, `tinyrtc_audio_pull` on the
  render side. The configuration of the OS audio session lives in ONE place per half and can be
  taken over by the host app (an app with its own audio must be able to say "I configure it").
  When the last call ends the session is given back as it was found.
- **The encoder obeys the core.** `target(bitrate, max_fps, max_height)` reconfigures it (bitrate
  on the fly; a smaller picture by scaling the camera's frames down, keeping the aspect ratio; the
  encoder is recreated when the size changes, and the first frame after is a keyframe with its
  parameter sets). `keyframe()` forces an IDR. A keyframe is always sent WITH its SPS and PPS.
  Constrained Baseline, level 3.1 or the level the size needs, no B-frames, real-time mode.
- **Rotation is sent, not baked in.** Frames are encoded as the sensor gives them; the degrees to
  show them upright travel with each frame (`rotation`), and the receiving view rotates.
- **Several views of one camera.** A local camera track shows in every view given it, at once (a
  self-view tile and a fullscreen preview). Android: the views share the camera's ONE preview
  stream (Camera2 surface sharing; the stream combinations LIMITED and LEGACY cameras guarantee
  hold the encoder and one preview, not a stream per view) — on Android 8+ (API 26) with a camera
  above LEGACY, as many views as the platform shares a stream with
  (`OutputConfiguration.getMaxSharedSurfaceCount()`, 4 on current versions); below API 26 or on a
  LEGACY camera, one. A view past the limit stays black and says so in the log, and gets the
  picture when a view before it lets go: it never takes the picture of a view that shows it.
- **Decoding belongs to the view.** A remote track that no view shows is not decoded. A view that
  starts showing a track (or whose decoder failed) asks for a keyframe
  (`tinyrtc_connection_request_keyframe`) and shows nothing until one arrives. Frames are shown
  `play_delay_us` after they arrive (lip sync with the sound).
- **A disabled local track** stops sending (the other side gets `remoteMedia` inactive) and frees
  nothing; `stop()` releases the device.
- **The OS takes the device** (a phone call, the camera claimed by another app, the app in the
  background on iOS): the track reports `onEnded` or pauses and resumes by itself — whichever the
  platform's API dictates; it never crashes and never leaves the device held.
- **Permissions are not asked here.** `openCamera` / `openMicrophone` fail with a "denied" error
  when the permission is missing; asking is the app's business.

## Errors

One error type per half with the cases `denied`, `notFound` (no such device / track / channel),
`failed(message)`, `closed`. The core's status codes map onto them and never surface as numbers.
An argument the API does not take (a track of the other kind, both limits of a channel, a ceiling
<= 0, a negative threshold) is `failed(message)` too. A call off the main thread is a bug of the
caller (Android: IllegalStateException), not one of these.
