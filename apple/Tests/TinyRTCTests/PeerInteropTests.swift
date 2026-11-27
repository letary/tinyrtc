// (iii) A call with tools/peer (str0m on RustCrypto) running on the Mac: this side's DTLS is the
// CryptoKit backend. The peer echoes the video and the sound and every channel message.
//
// Run by apple/test-peer.sh, which starts the peer on 127.0.0.1 with fixed ports and passes its
// URL as TINYRTC_PEER (TEST_RUNNER_TINYRTC_PEER for xcodebuild). Skipped without it.
//
// The sound: the simulator's test runner has no microphone permission (and the Mac mini no
// microphone), so the voice-processing unit's input side cannot run here. The tone goes in and the
// echo comes out through the engine's audio device interface, driven by ClockAudioDevice: the same
// push / pull calls the unit's callbacks make, on a 10 ms clock.

import CoreMedia
import Foundation
import XCTest
@testable import TinyRTC

@MainActor
final class PeerInteropTests: XCTestCase {
    func testCallWithToolsPeer() async throws {
        guard let base = ProcessInfo.processInfo.environment["TINYRTC_PEER"], let url = URL(string: base + "/offer") else {
            throw XCTSkip("TINYRTC_PEER is not set (apple/test-peer.sh runs this test)")
        }
        let device = ClockAudioDevice(toneHz: 440)
        let engine = RtcEngine(configuration: .init(logLevel: .info), audioDevice: device)
        let connection = try engine.connect()
        var states: [RtcConnectionState] = []
        connection.onStateChange = { states.append($0) }
        let channel = try await connection.createDataChannel("chat")
        var echoes: [RtcDataChannel.Message] = []
        channel.onMessage = { echoes.append($0) }

        let microphone = try await engine.openMicrophone()
        let camera = try await engine.makeVideoTrack(capturer: PatternCapturer(rotation: 90),
                                                     options: CameraOptions(width: 640, height: 480, frameRate: 30))
        try connection.setTrack(.audio, microphone)
        try connection.setTrack(.video, camera)
        let sink = DecodingSink()
        connection.remoteVideo.feed.add(sink)

        // Signaling: POST the offer, the answer comes back. The peer gathers nothing later.
        let offer = try await connection.createOffer()
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.httpBody = Data(offer.utf8)
        request.setValue("application/sdp", forHTTPHeaderField: "Content-Type")
        let (body, response) = try await URLSession.shared.data(for: request)
        XCTAssertEqual((response as? HTTPURLResponse)?.statusCode, 200, String(decoding: body, as: UTF8.self))
        try await connection.acceptAnswer(String(decoding: body, as: UTF8.self))

        let connected = await waitUntil(15) { connection.state == .connected }
        XCTAssertTrue(connected, "states \(states)")
        guard connected else { return }
        print("CryptoKit (this side) <-> RustCrypto (tools/peer): DTLS connected, states \(states)")
        let opened = await waitUntil(5) { channel.state == .open }
        XCTAssertTrue(opened)
        XCTAssertTrue(device.snapshot.input, "the microphone side of the audio device runs")

        // Ten seconds of a call: the pattern, the tone, a message every 250 ms.
        var sent = 0
        let start = Date()
        while Date().timeIntervalSince(start) < 10 {
            try await channel.send("ping \(sent)")
            sent += 1
            await pause(0.25)
        }
        await pause(1)

        let stats = try await connection.stats()
        let video = sink.state.get
        let encoder = camera.pipeline.encoder.stats.get
        let pulled = device.snapshot.pulled
        let (hz, rms) = toneOf(pulled.suffix(48_000 * 2))
        print("stats: \(stats)")
        print("encoder: \(encoder)")
        print("echoed video: frames \(video.frames) keyframes \(video.keyframes) decoded \(video.decoded) errors \(video.decodeErrors) sizes \(video.sizes.map { "\($0.width)x\($0.height)" }) rotations \(video.rotations) play_delay_us \(video.playDelaysUs.min() ?? 0)...\(video.playDelaysUs.max() ?? 0)")
        print("echoed sound over the last 2 s: \(hz) Hz, rms \(rms)")
        print("channel: sent \(sent), echoed \(echoes.count)")

        XCTAssertEqual(stats.state, .connected)
        XCTAssertGreaterThan(stats.audioSendBps, 10_000, "\(stats)")
        XCTAssertGreaterThan(stats.audioRecvBps, 10_000, "\(stats)")
        XCTAssertGreaterThan(stats.videoSendBps, 50_000, "\(stats)")
        XCTAssertGreaterThan(stats.videoRecvBps, 50_000, "\(stats)")
        XCTAssertGreaterThanOrEqual(echoes.count, sent - 2)
        XCTAssertEqual(echoes.first, .text("ping 0"))
        XCTAssertGreaterThan(video.decoded, 150, "echoed frames decoded")
        XCTAssertEqual(video.decodeErrors, 0)
        // The peer writes the echo again without the video-orientation extension: it comes back
        // at 0 (between two tinyrtc connections the rotation travels: CodecPathTests).
        XCTAssertEqual(video.rotations, [0])
        for size in video.sizes {
            XCTAssertTrue(encoder.sizes.contains("\(size.width)x\(size.height)"), "decoded \(size), encoded \(encoder.sizes)")
        }
        XCTAssertEqual(abs(hz - 440) < 20, true, "the echo is not the tone: \(hz) Hz")
        XCTAssertGreaterThan(rms, 2000, "the echo is too quiet")

        camera.stop()
        microphone.stop()
        connection.close()
        sink.close()
    }
}
