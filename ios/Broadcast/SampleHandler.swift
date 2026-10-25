import AVFoundation
import CoreMedia
import ImageIO
import ReplayKit

/// The ReplayKit Broadcast Upload Extension: iOS hands it the screen and the
/// sound of the apps on it, and it passes both to the Silent app over
/// `ScreenSocket`. It runs in its own process with a 50 MB memory limit, so
/// it only copies, converts and forwards.
final class SampleHandler: RPBroadcastSampleHandler {
    private var socket: Int32 = -1
    private let queue = DispatchQueue(label: "silent.broadcast")
    private let lock = NSLock()
    private var busy = false
    /// The broadcast ended (stopped by the user or failed); nothing more is
    /// sent or reported.
    private var finished = false
    private var lastFrame = Date.distantPast

    override func broadcastStarted(withSetupInfo _: [String: NSObject]?) {
        guard let path = ScreenSocket.path, var addr = ScreenSocket.address(path) else {
            fail("Screen sharing needs a signed build of Silent.")
            return
        }
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        let connected = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) == 0
            }
        }
        guard fd >= 0, connected else {
            if fd >= 0 { close(fd) }
            fail("Open Silent and join a room first.")
            return
        }
        var noSigPipe: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
        // A write that makes no progress for a second means the app stopped
        // reading (left the room, or was suspended): end the broadcast.
        var timeout = timeval(tv_sec: 1, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        socket = fd
    }

    override func broadcastFinished() {
        lock.lock()
        finished = true
        lock.unlock()
        if socket >= 0 { shutdown(socket, SHUT_RDWR) }
        queue.sync {
            if socket >= 0 { close(socket) }
            socket = -1
        }
    }

    override func processSampleBuffer(_ sampleBuffer: CMSampleBuffer, with type: RPSampleBufferType) {
        if type == .audioApp {
            forwardAudio(sampleBuffer)
            return
        }
        guard type == .video, socket >= 0,
              let buffer = CMSampleBufferGetImageBuffer(sampleBuffer)
        else { return }
        // At most 30 frames a second, and never queue up behind a slow app.
        let now = Date()
        lock.lock()
        let take = !finished && !busy && now.timeIntervalSince(lastFrame) >= 1.0 / 30
        if take {
            busy = true
            lastFrame = now
        }
        lock.unlock()
        guard take else { return }
        let rotation = Self.rotation(of: sampleBuffer)
        queue.async {
            defer {
                self.lock.lock()
                self.busy = false
                self.lock.unlock()
            }
            if !self.send(buffer, rotation: rotation) {
                self.lock.lock()
                let report = !self.finished
                self.finished = true
                self.lock.unlock()
                if report { self.fail("Silent stopped receiving the screen.") }
            }
        }
    }

    private func send(_ buffer: CVPixelBuffer, rotation: UInt32) -> Bool {
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard CVPixelBufferGetPlaneCount(buffer) == 2,
              let y = CVPixelBufferGetBaseAddressOfPlane(buffer, 0),
              let uv = CVPixelBufferGetBaseAddressOfPlane(buffer, 1)
        else { return true }
        let height = CVPixelBufferGetHeightOfPlane(buffer, 0)
        let yStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let uvStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)
        let header: [UInt32] = [
            ScreenSocket.magic,
            UInt32(CVPixelBufferGetWidthOfPlane(buffer, 0)),
            UInt32(height),
            UInt32(yStride),
            UInt32(uvStride),
            rotation,
            CVPixelBufferGetPixelFormatType(buffer) == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange ? 1 : 0,
        ].map(\.littleEndian)
        return header.withUnsafeBytes { ScreenSocket.write(socket, $0) }
            && ScreenSocket.write(socket, UnsafeRawBufferPointer(start: y, count: yStride * height))
            && ScreenSocket.write(
                socket,
                UnsafeRawBufferPointer(start: uv, count: uvStride * CVPixelBufferGetHeightOfPlane(buffer, 1))
            )
    }

    /// App audio converted to what the call sends, and the format it was
    /// converted from (only touched on `queue`).
    private var converter: AVAudioConverter?
    private var converterInput: AVAudioFormat?
    private static let audioFormat = AVAudioFormat(
        commonFormat: .pcmFormatInt16, sampleRate: 48_000, channels: 2, interleaved: true
    )!

    private func forwardAudio(_ sampleBuffer: CMSampleBuffer) {
        lock.lock()
        let done = finished
        lock.unlock()
        guard !done, socket >= 0 else { return }
        queue.async {
            guard let pcm = self.convert(sampleBuffer) else { return }
            let header: [UInt32] = [ScreenSocket.audioMagic, UInt32(pcm.count)].map(\.littleEndian)
            let sent = header.withUnsafeBytes { ScreenSocket.write(self.socket, $0) }
                && pcm.withUnsafeBytes { ScreenSocket.write(self.socket, $0) }
            if !sent {
                self.lock.lock()
                let report = !self.finished
                self.finished = true
                self.lock.unlock()
                if report { self.fail("Silent stopped receiving the screen.") }
            }
        }
    }

    /// 16-bit stereo at 48 kHz, whatever ReplayKit delivers.
    private func convert(_ sampleBuffer: CMSampleBuffer) -> Data? {
        guard let description = CMSampleBufferGetFormatDescription(sampleBuffer),
              var stream = CMAudioFormatDescriptionGetStreamBasicDescription(description)?.pointee,
              let input = AVAudioFormat(streamDescription: &stream)
        else { return nil }
        let frames = AVAudioFrameCount(CMSampleBufferGetNumSamples(sampleBuffer))
        guard frames > 0, let source = AVAudioPCMBuffer(pcmFormat: input, frameCapacity: frames) else { return nil }
        source.frameLength = frames
        guard CMSampleBufferCopyPCMDataIntoAudioBufferList(
            sampleBuffer, at: 0, frameCount: Int32(frames), into: source.mutableAudioBufferList
        ) == noErr else { return nil }

        if converterInput != input {
            converter = AVAudioConverter(from: input, to: Self.audioFormat)
            converterInput = input
        }
        let capacity = AVAudioFrameCount(Double(frames) * 48_000 / input.sampleRate) + 64
        guard let converter, let output = AVAudioPCMBuffer(pcmFormat: Self.audioFormat, frameCapacity: capacity)
        else { return nil }
        var fed = false
        var error: NSError?
        converter.convert(to: output, error: &error) { _, status in
            if fed {
                status.pointee = .noDataNow
                return nil
            }
            fed = true
            status.pointee = .haveData
            return source
        }
        guard error == nil, output.frameLength > 0, let samples = output.int16ChannelData else { return nil }
        return Data(bytes: samples[0], count: Int(output.frameLength) * 4)
    }

    /// Degrees to turn the frame clockwise to make it upright.
    private static func rotation(of sampleBuffer: CMSampleBuffer) -> UInt32 {
        let value = CMGetAttachment(sampleBuffer, key: RPVideoSampleOrientationKey as CFString, attachmentModeOut: nil)
        switch (value as? NSNumber).flatMap({ CGImagePropertyOrientation(rawValue: $0.uint32Value) }) {
        case .left: return 90
        case .down: return 180
        case .right: return 270
        default: return 0
        }
    }

    private func fail(_ reason: String) {
        finishBroadcastWithError(NSError(
            domain: "app.silent.broadcast", code: 1,
            userInfo: [NSLocalizedDescriptionKey: reason]
        ))
    }
}
