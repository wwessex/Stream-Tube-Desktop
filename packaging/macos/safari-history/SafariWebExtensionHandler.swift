import Foundation
import Network
import SafariServices

class SafariWebExtensionHandler: NSObject, NSExtensionRequestHandling {
    func beginRequest(with context: NSExtensionContext) {
        guard let request = context.inputItems.first as? NSExtensionItem,
              let message = request.userInfo?[SFExtensionMessageKey] as? [String: Any],
              let portString = message["port"] as? String,
              let port = NWEndpoint.Port(portString), port.rawValue > 0,
              let token = message["token"] as? String,
              token.utf8.count == 43,
              token.utf8.allSatisfy(Self.isTokenByte),
              let purpose = message["purpose"] as? String, purpose == "history-and-reactions",
              let cookies = message["cookies"] as? [[String: String]],
              cookies.count <= 20 else {
            Self.finish(context, received: false)
            return
        }
        let body: [String: Any] = ["purpose": purpose, "cookies": cookies]
        guard let payload = try? JSONSerialization.data(withJSONObject: body),
              payload.count <= 16_384 else {
            Self.finish(context, received: false)
            return
        }

        let connection = NWConnection(host: "127.0.0.1", port: port, using: .tcp)
        let queue = DispatchQueue(label: "streamtube.safari.handoff")
        var done = false
        let finish: (Bool) -> Void = { received in
            guard !done else { return }
            done = true
            connection.cancel()
            Self.finish(context, received: received)
        }
        queue.asyncAfter(deadline: .now() + 4) { finish(false) }
        connection.stateUpdateHandler = { state in
            switch state {
            case .ready:
                var packet = Data(token.utf8)
                packet.append(10)
                packet.append(payload)
                packet.append(10)
                connection.send(content: packet, completion: .contentProcessed { error in
                    guard error == nil else { finish(false); return }
                    connection.receive(minimumIncompleteLength: 1, maximumLength: 1) { data, _, _, _ in
                        finish(data == Data([1]))
                    }
                })
            case .failed, .cancelled:
                finish(false)
            default:
                break
            }
        }
        connection.start(queue: queue)
    }

    private static func isTokenByte(_ byte: UInt8) -> Bool {
        (65...90).contains(byte) || (97...122).contains(byte) || (48...57).contains(byte) || byte == 45 || byte == 95
    }

    private static func finish(_ context: NSExtensionContext, received: Bool) {
        let response = NSExtensionItem()
        response.userInfo = [SFExtensionMessageKey: ["received": received]]
        context.completeRequest(returningItems: [response], completionHandler: nil)
    }
}
