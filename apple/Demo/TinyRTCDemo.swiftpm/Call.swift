// One call to tools/peer: the camera and the microphone go out, the peer sends both back.

import AVFoundation
import SwiftUI
import TinyRTC

@MainActor
final class Call: ObservableObject {
    @Published var url = "http://192.168.0.4:18480"
    @Published var status = "idle"
    @Published var stats = ""
    @Published var inCall = false
    @Published var speaker = true {
        didSet { RtcEngine.shared.audioOutput = speaker ? .speaker : .earpiece }
    }

    let local = RtcVideoView()
    let remote = RtcVideoView()

    private var connection: RtcConnection?
    private var camera: LocalVideoTrack?
    private var microphone: LocalAudioTrack?
    private var timer: Timer?
    private var facing = CameraFacing.user

    init() {
        local.mirror = true
        local.objectFit = .cover
        remote.objectFit = .contain
        RtcEngine.shared.audioOutput = .speaker
    }

    func start() async {
        guard !inCall else { return }
        inCall = true
        status = "asking for the camera and the microphone"
        // Asking is the app's business: the library only says .denied.
        guard await AVCaptureDevice.requestAccess(for: .video), await Self.requestMicrophone() else {
            status = "camera or microphone denied (Settings > TinyRTCDemo)"
            inCall = false
            return
        }
        do {
            let engine = RtcEngine.shared
            let camera = try await engine.openCamera(CameraOptions(facing: facing, width: 1280, height: 720, frameRate: 30))
            let microphone = try await engine.openMicrophone()
            let connection = try engine.connect()
            self.camera = camera
            self.microphone = microphone
            self.connection = connection
            local.track = camera
            remote.track = connection.remoteVideo
            connection.onStateChange = { [weak self] state in self?.status = "\(state)" }
            try connection.setTrack(.video, camera)
            try connection.setTrack(.audio, microphone)
            let channel = try await connection.createDataChannel("chat")
            channel.onMessage = { message in print("echo: \(message)") }

            status = "calling \(url)"
            let offer = try await connection.createOffer()
            let answer = try await Signaling.post(offer, to: url)
            try await connection.acceptAnswer(answer)
            timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
                Task { @MainActor in await self?.refresh() }
            }
        } catch {
            status = "failed: \(error)"
            stop()
        }
    }

    func stop() {
        timer?.invalidate()
        timer = nil
        connection?.close()
        camera?.stop()
        microphone?.stop()
        local.track = nil
        remote.track = nil
        connection = nil
        camera = nil
        microphone = nil
        inCall = false
        stats = ""
    }

    func flip() async {
        facing = facing == .user ? .environment : .user
        local.mirror = facing == .user
        guard let camera else { return }
        do {
            try await camera.apply(CameraOptions(facing: facing, width: 1280, height: 720, frameRate: 30))
        } catch {
            status = "flip: \(error)"
        }
    }

    private func refresh() async {
        guard let connection, let s = try? await connection.stats() else { return }
        let size = remote.videoSize
        stats = "\(s.state) rtt \(s.rttMs) ms · video \(s.videoSendBps / 1000)/\(s.videoRecvBps / 1000) kbps \(s.videoRecvFps) fps "
            + "\(Int(size.width))x\(Int(size.height)) · audio \(s.audioSendBps / 1000)/\(s.audioRecvBps / 1000) kbps, buffer \(s.audioJitterBufferMs) ms"
    }

    private static func requestMicrophone() async -> Bool {
        if #available(iOS 17.0, *) {
            return await AVAudioApplication.requestRecordPermission()
        }
        return await withCheckedContinuation { done in
            AVAudioSession.sharedInstance().requestRecordPermission { done.resume(returning: $0) }
        }
    }
}
