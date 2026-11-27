// (i) The Swift bindings alone: two RtcConnections in one process — offer / answer, connected,
// data channel messages both ways, a channel closed, the connection closed, and no event after.

import Foundation
import XCTest
@testable import TinyRTC

@MainActor
final class ConnectionTests: XCTestCase {
    func testOfferAnswerChannelsAndClose() async throws {
        let device = ClockAudioDevice()
        let engine = RtcEngine(configuration: .init(logLevel: .warn), audioDevice: device)
        let a = try engine.connect()
        let b = try engine.connect()
        var statesA: [RtcConnectionState] = []
        var statesB: [RtcConnectionState] = []
        a.onStateChange = { statesA.append($0) }
        b.onStateChange = { statesB.append($0) }
        a.onIceCandidate = { [weak b] in b?.addIceCandidate($0) }
        b.onIceCandidate = { [weak a] in a?.addIceCandidate($0) }

        // A channel asked for before the offer rides in it.
        let chat = try await a.createDataChannel("chat")
        XCTAssertEqual(chat.state, .connecting)
        var openedAtA = 0
        chat.onOpen = { openedAtA += 1 }
        var remote: RtcDataChannel?
        var heardAtB: [RtcDataChannel.Message] = []
        b.onDataChannel = { channel in
            remote = channel
            channel.onMessage = { heardAtB.append($0) }
        }

        let offer = try await a.createOffer()
        XCTAssertTrue(offer.contains("m=audio") && offer.contains("m=video") && offer.contains("m=application"), offer)
        XCTAssertTrue(offer.contains("profile-level-id=42e01f"), offer)
        let answer = try await b.acceptOffer(offer)
        try await a.acceptAnswer(answer)

        // The core's errors, as the half's.
        do {
            try await a.acceptAnswer(answer)
            XCTFail("a second answer was taken")
        } catch RtcError.failed {}
        do {
            _ = try await b.acceptOffer("not a description")
            XCTFail("junk was taken for an offer")
        } catch RtcError.failed {}

        let connected = await waitUntil(10) { a.state == .connected && b.state == .connected }
        XCTAssertTrue(connected, "a \(a.state) b \(b.state)")
        XCTAssertEqual(statesA, [.connecting, .connected])
        XCTAssertEqual(statesB, [.connecting, .connected])
        // A connection exists: the audio device runs (render side only, no microphone track).
        let audioRuns = await waitUntil(2) { device.snapshot.running }
        XCTAssertTrue(audioRuns)
        XCTAssertFalse(device.snapshot.input)

        let opened = await waitUntil(5) { openedAtA == 1 && remote != nil }
        XCTAssertTrue(opened, "open at a: \(openedAtA), at b: \(String(describing: remote))")
        guard let remote else { return }
        XCTAssertEqual(remote.label, "chat")
        XCTAssertEqual(remote.state, .open)
        XCTAssertEqual(chat.state, .open)

        // Messages both ways, text and binary, in order.
        var heardAtA: [RtcDataChannel.Message] = []
        chat.onMessage = { heardAtA.append($0) }
        for i in 0..<20 { try await chat.send("m\(i)") }
        try await chat.send(Data([0, 1, 2, 255]))
        try await remote.send("pong")
        try await remote.send(Data(repeating: 7, count: 3000))
        let heard = await waitUntil(5) { heardAtB.count == 21 && heardAtA.count == 2 }
        XCTAssertTrue(heard, "b heard \(heardAtB.count), a heard \(heardAtA.count)")
        XCTAssertEqual(Array(heardAtB.prefix(20)), (0..<20).map { RtcDataChannel.Message.text("m\($0)") })
        XCTAssertEqual(heardAtB.last, .binary(Data([0, 1, 2, 255])))
        XCTAssertEqual(heardAtA, [.text("pong"), .binary(Data(repeating: 7, count: 3000))])
        let buffered = try await chat.bufferedAmount()
        XCTAssertLessThan(buffered, 1_000_000)

        // A channel the answering side opens mid-call: no new offer.
        var late: RtcDataChannel?
        a.onDataChannel = { late = $0 }
        let lateAtB = try await b.createDataChannel("late", options: .init(ordered: false, maxRetransmits: 0))
        let lateOpen = await waitUntil(5) { late != nil && lateAtB.state == .open }
        XCTAssertTrue(lateOpen)
        XCTAssertEqual(late?.label, "late")

        // The core counts bytes once a second.
        var stats = try await a.stats()
        let end = Date().addingTimeInterval(3)
        while stats.bytesSent == 0 && Date() < end {
            await pause(0.1)
            stats = try await a.stats()
        }
        XCTAssertEqual(stats.state, .connected)
        XCTAssertGreaterThan(stats.bytesSent, 0, "\(stats)")
        XCTAssertGreaterThan(stats.bytesReceived, 0, "\(stats)")

        // A channel closed on one side closes on the other; nothing of it after close().
        var remoteClosed = false
        remote.onClose = { remoteClosed = true }
        var afterChannelClose = 0
        chat.onMessage = { _ in afterChannelClose += 1 }
        chat.onClose = { afterChannelClose += 1 }
        chat.close()
        chat.close()
        XCTAssertEqual(chat.state, .closed)
        let channelClosed = await waitUntil(5) { remoteClosed }
        XCTAssertTrue(channelClosed)
        do {
            try await chat.send("after")
            XCTFail("sent on a closed channel")
        } catch RtcError.closed {}

        // The connection: closed at once for the app, seen by the other side, nothing after.
        var afterClose = 0
        a.onStateChange = { _ in afterClose += 1 }
        a.onIceCandidate = { _ in afterClose += 1 }
        a.onDataChannel = { _ in afterClose += 1 }
        a.onRemoteMedia = { _, _ in afterClose += 1 }
        late?.onMessage = { _ in afterClose += 1 }
        late?.onClose = { afterClose += 1 }
        try await lateAtB.send("to a closed side")
        a.close()
        a.close()
        XCTAssertEqual(a.state, .closed)
        XCTAssertTrue(a.isClosed)
        let closedAt = Date()
        let seen = await waitUntil(15) { b.state == .closed || b.state == .disconnected }
        print("b after a.close(): \(statesB) after \(Date().timeIntervalSince(closedAt)) s")
        XCTAssertTrue(seen, "b: \(statesB)")
        await pause(1)
        XCTAssertEqual(afterClose, 0, "events of a closed connection")
        XCTAssertEqual(afterChannelClose, 0, "events of a closed channel")
        do {
            _ = try await a.createOffer()
            XCTFail("an offer from a closed connection")
        } catch RtcError.closed {}
        XCTAssertThrowsError(try a.setTrack(.audio, nil))

        // The last connection gone: the audio stops.
        b.close()
        let audioStops = await waitUntil(2) { !device.snapshot.running }
        XCTAssertTrue(audioStops)
    }

    /// The simulator has no camera, and a test runner no camera permission: one of the two errors,
    /// never a crash and never a number.
    func testCameraErrors() async {
        let engine = RtcEngine(configuration: .init(), audioDevice: ClockAudioDevice())
        do {
            _ = try await engine.openCamera()
            XCTFail("a camera opened on the simulator")
        } catch let error as RtcError {
            print("openCamera on the simulator: \(error)")
            XCTAssertTrue(error == .denied || error == .notFound, "\(error)")
        } catch {
            XCTFail("not an RtcError: \(error)")
        }
    }

    /// Dropped without close(): cleaned up the same way, and the engine lets the audio go.
    func testDroppedConnectionCleansUp() async throws {
        let device = ClockAudioDevice()
        let engine = RtcEngine(configuration: .init(), audioDevice: device)
        var a: RtcConnection?
        var b: RtcConnection?
        do {
            let (x, y, _) = try await connectedPair(engine)
            (a, b) = (x, y)
        }
        XCTAssertEqual(a?.state, .connected)
        var bStates: [RtcConnectionState] = []
        b?.onStateChange = { bStates.append($0) }
        weak let gone = a
        a = nil
        XCTAssertNil(gone, "the connection is still held by something")
        let start = Date()
        let seen = await waitUntil(15) { bStates.contains(.closed) || bStates.contains(.disconnected) }
        print("b after a was dropped: \(bStates) after \(Date().timeIntervalSince(start)) s")
        XCTAssertTrue(seen, "b: \(bStates)")
        b = nil
        let audioStops = await waitUntil(2) { !device.snapshot.running }
        XCTAssertTrue(audioStops)
    }
}
