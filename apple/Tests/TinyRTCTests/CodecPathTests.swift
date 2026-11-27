// (ii) The codec path for real: a moving pattern (CVPixelBuffers) -> H264Encoder (VideoToolbox)
// -> the core's video source -> connection A -> connection B -> the view's depacketizer
// (SampleBufferBuilder -> CMSampleBuffer) -> a VTDecompressionSession, and an RtcVideoView beside it.

import CoreMedia
import UIKit
import XCTest
@testable import TinyRTC

@MainActor
final class CodecPathTests: XCTestCase {
    func testPatternThroughEncoderConnectionAndDecoder() async throws {
        let engine = RtcEngine(configuration: .init(), audioDevice: ClockAudioDevice())
        let (a, b, _) = try await connectedPair(engine)
        let track = try await engine.makeVideoTrack(capturer: PatternCapturer(rotation: 90),
                                                    options: CameraOptions(width: 1280, height: 720, frameRate: 30))
        let encoder = track.pipeline.encoder
        let sink = DecodingSink()
        b.remoteVideo.feed.add(sink)
        // The view's own path beside it (its layer is not on a screen here: only the sizes are asserted).
        let view = RtcVideoView(frame: CGRect(x: 0, y: 0, width: 360, height: 640))
        var resized: [String] = []
        view.onResize = { resized.append("\($0)x\($1)") }
        view.track = b.remoteVideo
        var remoteMedia: [String] = []
        b.onRemoteMedia = { remoteMedia.append("\($0)=\($1)") }

        try a.setTrack(.video, track)

        // 1. Frames decode, at the sizes the encoder made.
        let flowing = await waitUntil(15) { sink.state.get.decoded >= 90 }
        let first = sink.state.get
        let stats = encoder.stats.get
        print("encoder: \(stats)")
        print("decoder: frames \(first.frames) keyframes \(first.keyframes) decoded \(first.decoded) errors \(first.decodeErrors) sizes \(first.sizes.map { "\($0.width)x\($0.height)" }) play_delay_us \(first.playDelaysUs.min() ?? 0)...\(first.playDelaysUs.max() ?? 0)")
        XCTAssertTrue(flowing, "decoded \(first.decoded)")
        XCTAssertEqual(first.decodeErrors, 0)
        XCTAssertEqual(first.rotations, [90], "the rotation travels with every frame")
        for size in first.sizes {
            XCTAssertTrue(stats.sizes.contains("\(size.width)x\(size.height)"), "decoded \(size.width)x\(size.height), encoded \(stats.sizes)")
        }
        XCTAssertTrue(remoteMedia.contains("video=true"), "\(remoteMedia)")
        XCTAssertTrue(b.remoteVideo.isActive)
        // Every keyframe is SPS, PPS, then the IDR; the SPS says Constrained Baseline.
        XCTAssertFalse(first.keyframeNals.isEmpty)
        for nals in first.keyframeNals {
            XCTAssertEqual(Array(nals.prefix(2)), [7, 8], "\(nals)")
            XCTAssertTrue(nals.contains(5), "\(nals)")
        }
        let sps = try XCTUnwrap(first.sps)
        XCTAssertEqual(sps[sps.startIndex + 1], 66, "profile_idc: Baseline")
        XCTAssertNotEqual(sps[sps.startIndex + 2] & 0x40, 0, "constraint_set1_flag: Constrained Baseline")
        print("sps: profile \(sps[sps.startIndex + 1]) constraints \(String(sps[sps.startIndex + 2], radix: 2)) level \(sps[sps.startIndex + 3]); encoder \(stats.profile) lowLatency=\(stats.lowLatency)")

        // 2. A keyframe request (the receiver's) brings an IDR with its parameter sets. The core
        // lets one through per second on each side.
        await pause(1.5)
        let keyframesBefore = sink.state.get.keyframes
        let forcedBefore = encoder.stats.get.keyframesForced
        b.remoteVideo.feed.askKeyframe()
        let idr = await waitUntil(3) { sink.state.get.keyframes > keyframesBefore }
        XCTAssertTrue(idr, "no keyframe after the request")
        XCTAssertGreaterThan(encoder.stats.get.keyframesForced, forcedBefore, "the encoder was not told")
        let requested = try XCTUnwrap(sink.state.get.keyframeNals.last)
        XCTAssertEqual(Array(requested.prefix(2)), [7, 8], "\(requested)")
        XCTAssertTrue(requested.contains(5), "\(requested)")
        print("a requested keyframe's units: \(requested)")

        // 3. A lower target: the ceiling of 180 lines at 15 fps. The session is made again at
        // 320x180, its first frame a keyframe with its sets, and B decodes that size.
        let framesBefore = encoder.stats.get.framesEncoded
        let timeBefore = Date()
        track.setLimits(bitrate: 150_000, fps: 15, height: 180)
        let smaller = await waitUntil(5) {
            let s = sink.state.get
            return s.lastSize.width == 320 && s.lastSize.height == 180
        }
        let afterTarget = encoder.stats.get
        XCTAssertTrue(smaller, "decoded \(sink.state.get.lastSize), encoder \(afterTarget.sizes) target \(afterTarget.maxHeight)p\(afterTarget.maxFps)")
        XCTAssertEqual(afterTarget.maxHeight, 180)
        XCTAssertEqual(afterTarget.maxFps, 15)
        XCTAssertLessThanOrEqual(afterTarget.bitrate, 150_000)
        let resizedKey = try XCTUnwrap(sink.state.get.keyframeNals.last)
        XCTAssertEqual(Array(resizedKey.prefix(2)), [7, 8], "\(resizedKey)")
        XCTAssertTrue(resizedKey.contains(5), "\(resizedKey)")
        await pause(2)
        let rate = Double(encoder.stats.get.framesEncoded - framesBefore) / Date().timeIntervalSince(timeBefore)
        print("frame rate at the 15 fps target: \(rate)")
        XCTAssertLessThan(rate, 19, "the rate was not lowered")
        XCTAssertGreaterThan(rate, 10)
        XCTAssertEqual(sink.state.get.decodeErrors, 0)

        // The view followed: the picture is shown upright (rotated 90: 180x320).
        XCTAssertEqual(resized.last, "180x320", "\(resized)")
        XCTAssertEqual(view.videoSize, CGSize(width: 180, height: 320))

        // 4. Disabled: nothing more is sent, the other side sees the video stop; enabled again,
        // it starts with a keyframe.
        track.isEnabled = false
        await pause(0.5)
        let framesWhenDisabled = sink.state.get.frames
        let inactive = await waitUntil(8) { remoteMedia.contains("video=false") }
        XCTAssertEqual(sink.state.get.frames, framesWhenDisabled, "frames while disabled")
        // The half's part is that nothing is sent (asserted above). Whether B's core reports the
        // stream paused is str0m's: its pause timer is reset by repair packets too, and the
        // bandwidth probe's padding on the video RTX keeps it running at times. Observed, not asserted.
        print("remote media after disabling: \(remoteMedia) (seen inactive within 8.5 s: \(inactive))")
        let keyframesWhenDisabled = sink.state.get.keyframes
        track.isEnabled = true
        let back = await waitUntil(5) { sink.state.get.frames > framesWhenDisabled + 10 }
        XCTAssertTrue(back)
        XCTAssertGreaterThan(sink.state.get.keyframes, keyframesWhenDisabled)
        XCTAssertEqual(sink.state.get.decodeErrors, 0)

        // The end: the track stopped, the source let go of by every connection, then closed.
        let atStop = (sink.state.get.frames, encoder.stats.get.framesIn, encoder.stats.get.framesEncoded)
        track.stop()
        XCTAssertThrowsError(try a.setTrack(.video, track))
        await pause(0.5)
        let framesAfterStop = (sink.state.get.frames, encoder.stats.get.framesIn, encoder.stats.get.framesEncoded)
        await pause(0.5)
        let later = (sink.state.get.frames, encoder.stats.get.framesIn, encoder.stats.get.framesEncoded)
        print("after stop (received, into the encoder, encoded): at \(atStop), +0.5 s \(framesAfterStop), +1 s \(later)")
        XCTAssertEqual(later.0, framesAfterStop.0, "frames after stop()")
        sink.close()
        a.close()
        b.close()
    }

    /// The encoder's size rule: the short side at most the target's height, aspect kept, even.
    func testSizeRule() {
        XCTAssertTrue(H264Encoder.size(width: 1280, height: 720, maxShortSide: 720) == (1280, 720))
        XCTAssertTrue(H264Encoder.size(width: 1280, height: 720, maxShortSide: 360) == (640, 360))
        XCTAssertTrue(H264Encoder.size(width: 1280, height: 720, maxShortSide: 240) == (426, 240))
        XCTAssertTrue(H264Encoder.size(width: 720, height: 1280, maxShortSide: 180) == (180, 320))
        XCTAssertTrue(H264Encoder.size(width: 640, height: 480, maxShortSide: 720) == (640, 480))
    }
}
