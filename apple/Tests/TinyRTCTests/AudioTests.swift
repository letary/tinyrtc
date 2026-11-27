// The sound path's own pieces on the simulator: the rate converter both ways, and the real
// voice-processing unit with the session as the half configures it — render side only (a test
// runner has no microphone permission; with the input side on, the OS would end the process).

import AVFAudio
import XCTest
@testable import TinyRTC

/// A 1 kHz tone at 48 kHz for the pulling converter: the core's part.
private let tone48k: PcmSource = { user, pcm, frames in
    let phase = user!.assumingMemoryBound(to: Int.self)
    for i in 0..<frames {
        pcm[i] = Int16(sin(Double(phase.pointee + i) * 1000 * 2 * .pi / 48_000) * 10_000)
    }
    phase.pointee += frames
}

/// toneOf assumes 48 kHz; at another rate the crossings scale.
private func hz(_ pcm: ArraySlice<Int16>, rate: Double) -> Double {
    toneOf(pcm).hz * rate / 48_000
}

final class AudioTests: XCTestCase {
    /// Capture side: 16 kHz in, 48 kHz out, in the unit's slices; the tone stays a tone.
    func testConverterUp() throws {
        let converter = try XCTUnwrap(RateConverter(from: 16_000, to: 48_000, maxInputFrames: 4096))
        var input = [Int16](repeating: 0, count: 160)
        var output = [Int16](repeating: 0, count: 600)
        var all: [Int16] = []
        var at = 0
        for _ in 0..<100 { // 1 s in 10 ms slices
            for i in 0..<160 { input[i] = Int16(sin(Double(at + i) * 1000 * 2 * .pi / 16_000) * 10_000) }
            at += 160
            let n = input.withUnsafeMutableBufferPointer { inp in
                output.withUnsafeMutableBufferPointer { converter.push(inp.baseAddress!, count: 160, into: $0.baseAddress!, capacity: 600) }
            }
            all += output[0..<n]
        }
        XCTAssertGreaterThan(all.count, 47_000)
        XCTAssertLessThanOrEqual(all.count, 48_000)
        let (hz, rms) = toneOf(all.suffix(24_000))
        XCTAssertEqual(hz, 1000, accuracy: 15)
        XCTAssertGreaterThan(rms, 5000)
    }

    /// Render side: the core's 48 kHz pulled as needed, 16 kHz out, exactly as many samples as asked.
    func testConverterDown() throws {
        let phase = UnsafeMutablePointer<Int>.allocate(capacity: 1)
        phase.initialize(to: 0)
        defer { phase.deallocate() }
        let converter = try XCTUnwrap(RateConverter(from: 48_000, to: 16_000, maxInputFrames: 24_640, source: tone48k, user: phase))
        var output = [Int16](repeating: 0, count: 160)
        var all: [Int16] = []
        for _ in 0..<100 { // 1 s in 10 ms slices of the unit
            output.withUnsafeMutableBufferPointer { converter.pull(into: $0.baseAddress!, count: 160) }
            all += output
        }
        XCTAssertEqual(all.count, 16_000)
        XCTAssertEqual(Double(converter.pulled), 48_000, accuracy: 600)
        XCTAssertEqual(hz(all.suffix(8_000), rate: 16_000), 1000, accuracy: 15)
        XCTAssertGreaterThan(toneOf(all.suffix(8_000)).rms, 5000)
    }

    /// The voice-processing unit on the simulator, render side: the session as configured by
    /// RtcAudioSession, the unit at 48 kHz (and once more at the hardware rate through our
    /// converter), its render callback pulling the core's mix (silence: no connection) in real time.
    func testVoiceProcessingUnitRenders() throws {
        let session = RtcAudioSession.shared
        try session.activate()
        defer { session.deactivate() }
        let hardware = session.hardwareSampleRate
        let context = CoreContext(logLevel: .warn)
        // nil = as the device runs (48 kHz at the unit's client side); 16 kHz = a Bluetooth headset's
        // rate, through our converters in the unit's callbacks.
        for forced in [nil, 16_000.0] as [Double?] {
            let device = VoiceProcessingAudioDevice()
            device.forcedRate = forced
            do {
                try device.start(context: context, input: false, hardwareRate: hardware)
            } catch {
                XCTFail("the voice-processing unit did not start (forced rate \(String(describing: forced))): \(error)")
                continue
            }
            Thread.sleep(forTimeInterval: 0.3)
            let before = try XCTUnwrap(device.counters)
            let start = Date()
            Thread.sleep(forTimeInterval: 2)
            let after = try XCTUnwrap(device.counters)
            let seconds = Date().timeIntervalSince(start)
            device.stop()
            let unitPerSecond = Double(after.renderFrames - before.renderFrames) / seconds
            let corePerSecond = Double(after.pulledFrames - before.pulledFrames) / seconds
            print("voice-processing unit: hardware \(hardware) Hz, unit at \(after.unitRate) Hz, converter \(after.converted): \(Int(unitPerSecond)) frames/s rendered, \(Int(corePerSecond)) frames/s pulled from the core")
            XCTAssertEqual(unitPerSecond, after.unitRate, accuracy: after.unitRate * 0.1)
            XCTAssertEqual(corePerSecond, 48_000, accuracy: 4_800)
            XCTAssertEqual(after.converted, forced != nil)
        }
    }
}
