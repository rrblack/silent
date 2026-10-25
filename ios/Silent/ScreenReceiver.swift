import Foundation
import ReplayKit
import SwiftUI

/// Receives screen frames from the broadcast extension (see `ScreenSocket`)
/// and pushes them into the call. Listens only while in a room.
final class ScreenReceiver {
    /// Called on the main thread when sharing starts or stops.
    var onSharing: ((Bool) -> Void)?
    /// Send the phone's app audio along with the screen.
    var shareAudio: Bool {
        get {
            audioLock.lock()
            defer { audioLock.unlock() }
            return audioOn
        }
        set {
            audioLock.lock()
            audioOn = newValue
            audioLock.unlock()
        }
    }

    private let audioLock = NSLock()
    private var audioOn = true

    private var session: Session?

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
        let session = Session(listener: fd, room: room)
        session.shareAudio = { [weak self] in self?.shareAudio ?? false }
        self.session = session
        session.onSharing = { [weak self, weak session] sharing in
            DispatchQueue.main.async {
                guard let self, let session, self.session === session else { return }
                self.onSharing?(sharing)
            }
        }
        Thread.detachNewThread { session.run() }
    }

    /// Stops listening and disconnects the extension, which then ends the
    /// broadcast.
    func stop() {
        guard let session else { return }
        self.session = nil
        session.stop()
        if let path = ScreenSocket.path { unlink(path) }
    }
}

/// One room's listener and its connection to the extension. It runs on its
/// own thread and closes its sockets itself; `stop()` only wakes it, so a
/// file descriptor is never closed while that thread may still use it.
private final class Session {
    var onSharing: ((Bool) -> Void)?
    /// Whether app audio goes into the call; asked for each chunk.
    var shareAudio: () -> Bool = { true }

    private let listener: Int32
    private let room: SilentRoom
    private let lock = NSLock()
    private var client: Int32 = -1
    private var stopped = false

    /// Frames bigger than this are refused (a 4K screen is ~12 MB).
    private static let maxPlane = 32 * 1024 * 1024

    /// Full-range luma and chroma squeezed into video range (16–235 and
    /// 16–240), which is what the encoder assumes for I420.
    private static let lumaRange: [UInt8] = (0 ... 255).map { (l: Int) -> UInt8 in UInt8(16 + (l * 219 + 127) / 255) }
    private static let chromaRange: [UInt8] = (0 ... 255).map { (c: Int) -> UInt8 in
        let scaled = (c - 128) * 224
        return UInt8(128 + (scaled + (scaled >= 0 ? 127 : -127)) / 255)
    }

    private static func toVideoRange(_ y: inout Data, _ uv: inout Data) {
        remap(&y, lumaRange)
        remap(&uv, chromaRange)
    }

    private static func remap(_ plane: inout Data, _ table: [UInt8]) {
        table.withUnsafeBufferPointer { table in
            plane.withUnsafeMutableBytes { bytes in
                for i in bytes.indices { bytes[i] = table[Int(bytes[i])] }
            }
        }
    }

    /// Reads one app audio message and passes it on if the user shares audio.
    private func receiveAudio(_ client: Int32) -> Bool {
        var count: UInt32 = 0
        guard withUnsafeMutableBytes(of: &count, { ScreenSocket.read(client, $0.baseAddress!, 4) }) else { return false }
        let length = Int(UInt32(littleEndian: count))
        guard length <= ScreenSocket.maxAudioBytes else { return false }
        var pcm = Data(count: length)
        guard length == 0 || pcm.withUnsafeMutableBytes({ ScreenSocket.read(client, $0.baseAddress!, length) }) else {
            return false
        }
        if shareAudio() {
            room.pushScreenAudio(pcm: pcm)
        }
        return true
    }

    init(listener: Int32, room: SilentRoom) {
        self.listener = listener
        self.room = room
    }

    func stop() {
        lock.lock()
        stopped = true
        if client >= 0 { shutdown(client, SHUT_RDWR) }
        lock.unlock()
    }

    private var isStopped: Bool {
        lock.lock()
        defer { lock.unlock() }
        return stopped
    }

    func run() {
        defer { close(listener) }
        while !isStopped {
            var ready = pollfd(fd: listener, events: Int16(POLLIN), revents: 0)
            guard poll(&ready, 1, 250) > 0 else { continue }
            let fd = accept(listener, nil, nil)
            if fd < 0 { continue }
            var noSigPipe: Int32 = 1
            setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
            lock.lock()
            let accepted = !stopped
            if accepted { client = fd }
            lock.unlock()
            if accepted { serve(fd) }
            lock.lock()
            client = -1
            lock.unlock()
            close(fd)
        }
    }

    private func serve(_ client: Int32) {
        var sharing = false
        defer {
            if sharing {
                room.setScreen(on: false)
                onSharing?(false)
            }
        }
        var header = [UInt32](repeating: 0, count: ScreenSocket.headerFields)
        let word = MemoryLayout<UInt32>.size
        while header.withUnsafeMutableBytes({ ScreenSocket.read(client, $0.baseAddress!, word) }) {
            if UInt32(littleEndian: header[0]) == ScreenSocket.audioMagic {
                guard receiveAudio(client) else { return }
                continue
            }
            guard header.withUnsafeMutableBytes({
                ScreenSocket.read(client, $0.baseAddress! + word, ScreenSocket.headerSize - word)
            }) else { return }
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
            guard ok, !isStopped else { return }
            if h[6] != 0 { Self.toVideoRange(&y, &uv) }
            if !sharing {
                sharing = true
                room.setScreen(on: true)
                onSharing?(true)
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
