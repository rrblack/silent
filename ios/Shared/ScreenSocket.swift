import Foundation

/// How the broadcast extension hands screen frames to the app: a Unix socket
/// in the shared App Group container. Each frame is a header of six
/// little-endian UInt32s (magic, width, height, Y stride, UV stride, rotation)
/// followed by the Y plane and the interleaved UV plane (NV12).
enum ScreenSocket {
    static let appGroup = "group.app.silent"
    static let magic: UInt32 = 0x5349_4C31 // "SIL1"
    static let headerSize = 6 * MemoryLayout<UInt32>.size

    /// `nil` when the App Group isn't available (unsigned builds).
    static var path: String? {
        FileManager.default
            .containerURL(forSecurityApplicationGroupIdentifier: appGroup)?
            .appendingPathComponent("screen.sock").path
    }

    static func address(_ path: String) -> sockaddr_un? {
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else { return nil }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        return addr
    }

    /// Writes all of `data`, or returns false.
    static func write(_ fd: Int32, _ data: UnsafeRawBufferPointer) -> Bool {
        var sent = 0
        while sent < data.count {
            let n = Darwin.write(fd, data.baseAddress! + sent, data.count - sent)
            if n <= 0 { return false }
            sent += n
        }
        return true
    }

    /// Reads exactly `count` bytes into `buffer`, or returns false.
    static func read(_ fd: Int32, _ buffer: UnsafeMutableRawPointer, _ count: Int) -> Bool {
        var got = 0
        while got < count {
            let n = Darwin.read(fd, buffer + got, count - got)
            if n <= 0 { return false }
            got += n
        }
        return true
    }
}
