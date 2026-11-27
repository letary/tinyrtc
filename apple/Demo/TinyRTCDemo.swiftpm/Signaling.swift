// tools/peer's signaling: POST the offer to <url>/offer, the answer comes back.
//
// Over a plain TCP connection (Network.framework), not URLSession: the peer speaks http:// on a LAN
// address, which App Transport Security would refuse without an Info.plist exception.

import Foundation
import Network

enum Signaling {
    struct Failure: Error, CustomStringConvertible {
        let description: String
    }

    static func post(_ sdp: String, to base: String) async throws -> String {
        guard let url = URL(string: base.trimmingCharacters(in: .whitespaces)), let host = url.host else {
            throw Failure(description: "not a URL: \(base)")
        }
        guard let port = NWEndpoint.Port(rawValue: UInt16(url.port ?? 80)) else { throw Failure(description: "bad port") }
        let path = (url.path.isEmpty || url.path == "/" ? "" : url.path) + "/offer"
        let body = Data(sdp.utf8)
        var request = Data("POST \(path) HTTP/1.1\r\nHost: \(host)\r\nContent-Type: application/sdp\r\nContent-Length: \(body.count)\r\nConnection: close\r\n\r\n".utf8)
        request.append(body)

        let connection = NWConnection(host: NWEndpoint.Host(host), port: port, using: .tcp)
        defer { connection.cancel() }
        let response: Data = try await withCheckedThrowingContinuation { continuation in
            let once = Once(continuation)
            connection.stateUpdateHandler = { state in
                switch state {
                case .failed(let error), .waiting(let error):
                    once.resume(throwing: Failure(description: "\(host):\(port): \(error)"))
                default:
                    break
                }
            }
            connection.start(queue: .global())
            connection.send(content: request, completion: .contentProcessed { error in
                if let error { once.resume(throwing: error) }
            })
            receive(connection, into: Data(), once)
        }
        guard let split = response.range(of: Data("\r\n\r\n".utf8)) else { throw Failure(description: "no HTTP response") }
        let head = String(decoding: response[..<split.lowerBound], as: UTF8.self)
        let answer = String(decoding: response[split.upperBound...], as: UTF8.self)
        guard head.hasPrefix("HTTP/1.1 200") || head.hasPrefix("HTTP/1.0 200") else {
            throw Failure(description: "\(head.components(separatedBy: "\r\n").first ?? head): \(answer)")
        }
        return answer
    }

    /// Reads until the peer closes (it sends `Connection: close`).
    private static func receive(_ connection: NWConnection, into data: Data, _ once: Once) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { chunk, _, done, error in
            var data = data
            if let chunk { data.append(chunk) }
            if let error {
                once.resume(throwing: error)
            } else if done {
                once.resume(returning: data)
            } else {
                receive(connection, into: data, once)
            }
        }
    }

    /// A continuation resumed once, whichever callback comes first.
    private final class Once: @unchecked Sendable {
        private var continuation: CheckedContinuation<Data, Error>?
        private let lock = NSLock()

        init(_ continuation: CheckedContinuation<Data, Error>) {
            self.continuation = continuation
        }

        func resume(returning data: Data) {
            take()?.resume(returning: data)
        }

        func resume(throwing error: Error) {
            take()?.resume(throwing: error)
        }

        private func take() -> CheckedContinuation<Data, Error>? {
            lock.lock()
            defer { lock.unlock() }
            defer { continuation = nil }
            return continuation
        }
    }
}
