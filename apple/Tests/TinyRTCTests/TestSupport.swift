// What the tests drive the half with on a simulator, which has no camera and (in a test runner) no
// microphone permission: a clock that stands in for the audio unit, a capturer that draws a moving
// pattern, a sink that decodes the remote frames with VideoToolbox.

import AVFoundation
import CoreMedia
import CoreVideo
import Foundation
import TinyRTCCore
import VideoToolbox
import XCTest
@testable import TinyRTC

// MARK: waiting

@MainActor
func waitUntil(_ seconds: Double, _ condition: () -> Bool) async -> Bool {
    let end = Date().addingTimeInterval(seconds)
    while !condition() {
        if Date() > end { return false }
        try? await Task.sleep(nanoseconds: 5_000_000)
    }
    return true
}

@MainActor
func pause(_ seconds: Double) async {
    try? await Task.sleep(nanoseconds: UInt64(seconds * 1e9))
}

/// Two connections of `engine`, signaled through each other, candidates trickled both ways.
@MainActor
func connectedPair(_ engine: RtcEngine, channel: String? = nil) async throws -> (RtcConnection, RtcConnection, RtcDataChannel?) {
    let a = try engine.connect()
    let b = try engine.connect()
    a.onIceCandidate = { [weak b] in b?.addIceCandidate($0) }
    b.onIceCandidate = { [weak a] in a?.addIceCandidate($0) }
    var made: RtcDataChannel?
    if let channel { made = try await a.createDataChannel(channel) }
    let offer = try await a.createOffer()
    let answer = try await b.acceptOffer(offer)
    try await a.acceptAnswer(answer)
    let connected = await waitUntil(10) { a.state == .connected && b.state == .connected }
    XCTAssertTrue(connected, "not connected: a \(a.state), b \(b.state)")
    return (a, b, made)
}

// MARK: sound

/// Stands in for the voice-processing unit: a 10 ms clock that pushes a tone (with `input`) and
/// pulls the mix, like the unit's two callbacks.
final class ClockAudioDevice: AudioDevice, @unchecked Sendable {
    let needsRecordPermission = false
    let toneHz: Double
    private let queue = DispatchQueue(label: "test.audio-clock", qos: .userInteractive)
    private var timer: DispatchSourceTimer?
    private let state = Locked(State())

    struct State {
        var running = false
        var input = false
        var starts = 0
        var pushed = 0
        var pulled: [Int16] = []
    }

    init(toneHz: Double = 440) {
        self.toneHz = toneHz
    }

    var snapshot: State { state.get }

    func start(context: CoreContext, input: Bool, hardwareRate: Double) throws {
        stop()
        state.write {
            $0.running = true
            $0.input = input
            $0.starts += 1
        }
        let timer = DispatchSource.makeTimerSource(flags: .strict, queue: queue)
        let start = DispatchTime.now()
        let counter = Locked(0)
        let tone = toneHz
        timer.setEventHandler { [state] in
            // Real time: as many 10 ms ticks as are due.
            let due = Int((DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 10_000_000)
            var ticks = counter.get
            defer { counter.write { $0 = ticks } }
            while ticks < due {
                var pcm = [Int16](repeating: 0, count: 480)
                if input {
                    for i in 0..<480 {
                        let n = Double(ticks * 480 + i)
                        pcm[i] = Int16(sin(n * tone * 2 * .pi / 48_000) * 8000)
                    }
                    tinyrtc_audio_push(context.raw, pcm, 480)
                }
                var out = [Int16](repeating: 0, count: 480)
                tinyrtc_audio_pull(context.raw, &out, 480)
                state.write {
                    if input { $0.pushed += 480 }
                    $0.pulled.append(contentsOf: out)
                    if $0.pulled.count > 48_000 * 30 { $0.pulled.removeFirst($0.pulled.count - 48_000 * 30) }
                }
                ticks += 1
            }
        }
        timer.schedule(deadline: .now(), repeating: .milliseconds(10), leeway: .milliseconds(1))
        timer.resume()
        self.timer = timer
    }

    func stop() {
        timer?.cancel()
        timer = nil
        state.write { $0.running = false }
    }
}

/// Zero crossings per second / 2 and the RMS of a stretch of sound.
func toneOf(_ pcm: ArraySlice<Int16>) -> (hz: Double, rms: Double) {
    guard pcm.count > 1 else { return (0, 0) }
    var crossings = 0
    var energy = 0.0
    var previous = pcm.first!
    for sample in pcm {
        if (previous < 0) != (sample < 0) { crossings += 1 }
        previous = sample
        energy += Double(sample) * Double(sample)
    }
    return (Double(crossings) / 2 / (Double(pcm.count) / 48_000), (energy / Double(pcm.count)).squareRoot())
}

// MARK: pictures

/// A moving pattern instead of a camera: NV12 buffers from a pool, at `frameRate`, on the encoder's
/// queue (where the camera delivers too). Each row is a level, and a bright bar moves across.
final class PatternCapturer: VideoCapturer, @unchecked Sendable {
    private var timer: DispatchSourceTimer?
    private var pool: CVPixelBufferPool?
    private var size = (width: 1280, height: 720)
    private var frameRate = 30
    private var frame = 0
    private weak var encoder: H264Encoder?
    private let ended = Locked<(@Sendable () -> Void)?>(nil)
    let rotation: UInt16

    init(rotation: UInt16 = 90) {
        self.rotation = rotation
    }

    var onEnded: (@Sendable () -> Void)? {
        get { ended.get }
        set { ended.write { $0 = newValue } }
    }

    var previewSession: AVCaptureSession? { nil }
    var device: AVCaptureDevice? { nil }

    func start(_ options: CameraOptions, into encoder: H264Encoder) async throws -> CameraSettings {
        self.encoder = encoder
        return try await encoder.queue.run { [self] in
            configure(options)
            let timer = DispatchSource.makeTimerSource(queue: encoder.queue)
            timer.setEventHandler { [weak self] in self?.tick() }
            timer.schedule(deadline: .now(), repeating: .nanoseconds(1_000_000_000 / frameRate), leeway: .milliseconds(1))
            timer.resume()
            self.timer = timer
            return settings
        }
    }

    func apply(_ options: CameraOptions) async throws -> CameraSettings {
        guard let encoder else { throw RtcError.closed }
        return try await encoder.queue.run { [self] in
            configure(options)
            return settings
        }
    }

    func stop() async {
        guard let encoder else { return }
        _ = try? await encoder.queue.run { [self] in
            timer?.cancel()
            timer = nil
        }
    }

    private var settings: CameraSettings {
        CameraSettings(deviceId: "pattern", facing: .environment, width: size.width, height: size.height, frameRate: frameRate)
    }

    private func configure(_ options: CameraOptions) {
        size = (max(options.width, options.height), min(options.width, options.height))
        frameRate = options.frameRate
        let attributes: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferWidthKey as String: size.width,
            kCVPixelBufferHeightKey as String: size.height,
            kCVPixelBufferIOSurfacePropertiesKey as String: [String: Any](),
        ]
        pool = nil
        CVPixelBufferPoolCreate(kCFAllocatorDefault, nil, attributes as CFDictionary, &pool)
    }

    private func tick() {
        guard let pool, let encoder else { return }
        var made: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &made) == kCVReturnSuccess, let buffer = made else { return }
        CVPixelBufferLockBaseAddress(buffer, [])
        let (width, height) = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer))
        let luma = CVPixelBufferGetBaseAddressOfPlane(buffer, 0)!
        let lumaStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let bar = (frame * 12) % max(width - 64, 1)
        for row in 0..<height {
            let line = luma + row * lumaStride
            memset(line, Int32(16 + (row + frame * 3) % 200), width)
            memset(line + bar, 235, 64)
        }
        let chroma = CVPixelBufferGetBaseAddressOfPlane(buffer, 1)!
        let chromaStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)
        for row in 0..<height / 2 {
            memset(chroma + row * chromaStride, Int32(64 + (frame % 128)), width)
        }
        CVPixelBufferUnlockBaseAddress(buffer, [])
        let time = CMClockGetTime(CMClockGetHostTimeClock())
        frame += 1
        encoder.encode(buffer, time: time, rotation: rotation)
    }
}

/// A remote track's frames through the view's path (SampleBufferBuilder) into a VideoToolbox
/// decoder: what the display layer does, where it can be looked at without a screen.
final class DecodingSink: VideoFrameSink, @unchecked Sendable {
    private let queue = DispatchQueue(label: "test.decoder")
    private let builder = SampleBufferBuilder()
    private var session: VTDecompressionSession?
    let state = Locked(State())

    struct State {
        var frames = 0
        var keyframes = 0
        var decoded = 0
        var decodeErrors = 0
        var sizes: [CMVideoDimensions] = []
        var lastSize = CMVideoDimensions(width: 0, height: 0)
        var keyframeNals: [[UInt8]] = []
        var sps: Data?
        var rotations = Set<UInt16>()
        var playDelaysUs: [Int64] = []
    }

    func receive(_ frame: EncodedFrame) {
        queue.async { [self] in decode(frame) }
    }

    private func decode(_ frame: EncodedFrame) {
        state.write {
            $0.frames += 1
            $0.rotations.insert(frame.rotation)
            $0.playDelaysUs.append(frame.playDelayUs)
            if frame.keyframe {
                $0.keyframes += 1
                $0.keyframeNals.append(frame.nalTypes)
                if let sps = frame.sps { $0.sps = sps }
            }
        }
        guard let (buffer, changed) = builder.make(frame, presentationTime: frame.arrival, displayImmediately: false),
              let format = builder.format else { return }
        if changed || session == nil {
            if let session { VTDecompressionSessionInvalidate(session) }
            session = nil
            var made: VTDecompressionSession?
            let attributes = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange] as CFDictionary
            VTDecompressionSessionCreate(allocator: nil, formatDescription: format, decoderSpecification: nil,
                                         imageBufferAttributes: attributes, outputCallback: nil, decompressionSessionOut: &made)
            session = made
        }
        guard let session else { return }
        let status = VTDecompressionSessionDecodeFrame(session, sampleBuffer: buffer, flags: [], infoFlagsOut: nil) { [state] status, _, image, _, _ in
            state.write {
                guard status == noErr, let image else {
                    $0.decodeErrors += 1
                    return
                }
                let size = CMVideoDimensions(width: Int32(CVPixelBufferGetWidth(image)), height: Int32(CVPixelBufferGetHeight(image)))
                $0.decoded += 1
                if size.width != $0.lastSize.width || size.height != $0.lastSize.height { $0.sizes.append(size) }
                $0.lastSize = size
            }
        }
        if status != noErr { state.write { $0.decodeErrors += 1 } }
    }

    func close() {
        queue.sync {
            if let session {
                VTDecompressionSessionWaitForAsynchronousFrames(session)
                VTDecompressionSessionInvalidate(session)
            }
            session = nil
        }
    }
}
