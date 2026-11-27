// The screen: the peer's URL, the preview, the echo, a line of numbers.

import SwiftUI
import TinyRTC

@main
struct TinyRTCDemoApp: App {
    var body: some Scene {
        WindowGroup { CallScreen() }
    }
}

struct CallScreen: View {
    @StateObject private var call = Call()

    var body: some View {
        VStack(spacing: 8) {
            HStack {
                TextField("http://host:18480", text: $call.url)
                    .textFieldStyle(.roundedBorder)
                    .keyboardType(.URL)
                    .autocorrectionDisabled()
                    .textInputAutocapitalization(.never)
                    .disabled(call.inCall)
                Button(call.inCall ? "Hang up" : "Call") {
                    if call.inCall { call.stop() } else { Task { await call.start() } }
                }
                .buttonStyle(.borderedProminent)
            }
            HStack {
                Toggle("Speaker", isOn: $call.speaker).fixedSize()
                Spacer()
                Button("Flip camera") { Task { await call.flip() } }.disabled(!call.inCall)
            }
            GeometryReader { geometry in
                ZStack(alignment: .bottomTrailing) {
                    VideoView(view: call.remote)
                    VideoView(view: call.local)
                        .frame(width: geometry.size.width / 3, height: geometry.size.height / 3)
                        .clipShape(RoundedRectangle(cornerRadius: 8))
                        .padding(8)
                }
            }
            Text(call.status).font(.footnote).frame(maxWidth: .infinity, alignment: .leading)
            Text(call.stats).font(.caption.monospacedDigit()).lineLimit(2).frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding()
    }
}

/// One RtcVideoView (owned by the call) in SwiftUI.
struct VideoView: UIViewRepresentable {
    let view: RtcVideoView

    func makeUIView(context: Context) -> RtcVideoView { view }
    func updateUIView(_ uiView: RtcVideoView, context: Context) {}
}
