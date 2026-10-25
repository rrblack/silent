import Foundation
import ReplayKit
import SwiftUI

/// Receives screen frames from the broadcast extension (see `ScreenSocket`)
/// and pushes them into the call. Listens only while in a room.
final class ScreenReceiver {
    /// Called on the main thread when sharing starts or stops.
    var onSharing: ((Bool) -> Void)?

    private var listener: Int32 = -1
    private var room: SilentRoom?
    private let queue = DispatchQueue(label: "silent.screen")

    /// Frames bigger than this are refused (a 4K screen is ~12 MB).
    private static let maxPlane = 32 * 1024 * 1024

    func start(room: SilentRoom) {
        stop()
        guard let path = ScreenSocket.path, var addr = ScreenSocket.address(path) else { return }
        unlink(path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return }
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) == 0
            }
        }
        guard bound, listen(fd, 1) == 0 else {
            close(fd)
            return
        }
        listener = fd
        self.room = room
        queue.async { self.acceptLoop(fd, room: room) }
    }

    func stop() {
        guard listener >= 0 else { return }
        shutdown(listener, SHUT_RDWR)
        close(listener)
        listener = -1
        room = nil
        if let path = ScreenSocket.path { unlink(path) }
    }

    private func acceptLoop(_ fd: Int32, room: SilentRoom) {
        while true {
            let client = accept(fd, nil, nil)
            if client < 0 { return }
            var noSigPipe: Int32 = 1
            setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
            serve(client, room: room)
            close(client)
        }
    }

    private func serve(_ client: Int32, room: SilentRoom) {
        var sharing = false
        defer {
            if sharing {
                room.setScreen(on: false)
                DispatchQueue.main.async { self.onSharing?(false) }
            }
        }
        var header = [UInt32](repeating: 0, count: 6)
        while header.withUnsafeMutableBytes({ ScreenSocket.read(client, $0.baseAddress!, ScreenSocket.headerSize) }) {
            let h = header.map { UInt32(littleEndian: $0) }
            let (width, height, yStride, uvStride, rotation) = (Int(h[1]), Int(h[2]), Int(h[3]), Int(h[4]), h[5])
            let yLength = yStride * height
            let uvLength = uvStride * ((height + 1) / 2)
            guard h[0] == ScreenSocket.magic, width > 0, height > 0, yStride >= width,
                  yLength <= Self.maxPlane, uvLength <= Self.maxPlane
            else { return }
            var y = Data(count: yLength)
            var uv = Data(count: uvLength)
            let ok = y.withUnsafeMutableBytes { ScreenSocket.read(client, $0.baseAddress!, yLength) }
                && uv.withUnsafeMutableBytes { ScreenSocket.read(client, $0.baseAddress!, uvLength) }
            guard ok else { return }
            if !sharing {
                sharing = true
                room.setScreen(on: true)
                DispatchQueue.main.async { self.onSharing?(true) }
            }
            room.pushScreenYuv(
                width: UInt32(width), height: UInt32(height),
                y: y, u: uv, v: uv.dropFirst(),
                yStride: UInt32(yStride), uvStride: UInt32(uvStride), uvPixelStride: 2,
                rotationDegrees: rotation
            )
        }
    }
}

/// The system "start broadcast" button, preset to Silent's extension.
struct BroadcastButton: UIViewRepresentable {
    func makeUIView(context _: Context) -> RPSystemBroadcastPickerView {
        let picker = RPSystemBroadcastPickerView(frame: CGRect(x: 0, y: 0, width: 52, height: 52))
        picker.preferredExtension = "app.silent.broadcast"
        picker.showsMicrophoneButton = false
        return picker
    }

    func updateUIView(_: RPSystemBroadcastPickerView, context _: Context) {}
}
