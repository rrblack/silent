import AVFoundation
import SwiftUI
import UIKit

/// Front or back camera through AVFoundation. Frames go to the room as
/// NV12 (the engine encodes them), and to `session` for the self view.
/// Frames are turned to match how the phone is held, so friends always see
/// them upright.
final class CameraFeed: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    let session = AVCaptureSession()
    /// Called on the main thread when the camera can't start.
    var onFailure: (() -> Void)?
    /// The room frames go to; only touched on `queue`.
    private var room: SilentRoom?
    private let queue = DispatchQueue(label: "silent.camera")
    private let output = AVCaptureVideoDataOutput()

    /// Frames go to `room` from now on (`nil` while not in a room).
    func attach(_ room: SilentRoom?) {
        queue.async { self.room = room }
    }

    override init() {
        super.init()
        UIDevice.current.beginGeneratingDeviceOrientationNotifications()
        NotificationCenter.default.addObserver(
            self, selector: #selector(orientationChanged),
            name: UIDevice.orientationDidChangeNotification, object: nil
        )
    }

    func start(front: Bool, high: Bool) {
        let orientation = Self.videoOrientation(UIDevice.current.orientation) ?? .portrait
        queue.async {
            self.orientation = orientation
            if !self.configure(front: front, high: high) {
                DispatchQueue.main.async { self.onFailure?() }
            }
        }
    }

    func stop() {
        queue.async {
            if self.session.isRunning { self.session.stopRunning() }
        }
    }

    /// Only touched on `queue`.
    private var orientation = AVCaptureVideoOrientation.portrait

    @objc private func orientationChanged() {
        guard let orientation = Self.videoOrientation(UIDevice.current.orientation) else { return }
        queue.async {
            self.orientation = orientation
            self.applyOrientation()
        }
    }

    /// Face up, face down and unknown keep the last orientation.
    private static func videoOrientation(_ device: UIDeviceOrientation) -> AVCaptureVideoOrientation? {
        switch device {
        case .portrait: return .portrait
        case .portraitUpsideDown: return .portraitUpsideDown
        // The device names the side the top went to, the camera the side of
        // the home button, so left and right swap.
        case .landscapeLeft: return .landscapeRight
        case .landscapeRight: return .landscapeLeft
        default: return nil
        }
    }

    private func applyOrientation() {
        guard let connection = output.connection(with: .video), connection.isVideoOrientationSupported else { return }
        connection.videoOrientation = orientation
    }

    /// Returns false when there's no usable camera.
    private func configure(front: Bool, high: Bool) -> Bool {
        session.beginConfiguration()
        session.sessionPreset = high ? .hd1920x1080 : .hd1280x720
        session.inputs.forEach(session.removeInput)
        let position: AVCaptureDevice.Position = front ? .front : .back
        guard let device = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: position)
            ?? AVCaptureDevice.default(for: .video),
            let input = try? AVCaptureDeviceInput(device: device),
            session.canAddInput(input)
        else {
            session.commitConfiguration()
            if session.isRunning { session.stopRunning() }
            return false
        }
        session.addInput(input)
        if high { Self.prefer60fps(device) }
        if session.outputs.isEmpty {
            // Video range, which is what the encoder assumes for I420.
            output.videoSettings = [
                kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            ]
            output.alwaysDiscardsLateVideoFrames = true
            output.setSampleBufferDelegate(self, queue: queue)
            if session.canAddOutput(output) { session.addOutput(output) }
        }
        if let connection = output.connection(with: .video),
           connection.isVideoMirroringSupported
        {
            connection.isVideoMirrored = false
        }
        applyOrientation()
        session.commitConfiguration()
        if !session.isRunning { session.startRunning() }
        return true
    }

    /// 1080p at up to 60 fps where the camera can (30 in low light).
    private static func prefer60fps(_ device: AVCaptureDevice) {
        let format = device.formats.first { format in
            let size = CMVideoFormatDescriptionGetDimensions(format.formatDescription)
            return size.width == 1920 && size.height == 1080
                && format.videoSupportedFrameRateRanges.contains { $0.maxFrameRate >= 60 && $0.minFrameRate <= 30 }
        }
        guard let format, (try? device.lockForConfiguration()) != nil else { return }
        device.activeFormat = format
        device.activeVideoMinFrameDuration = CMTime(value: 1, timescale: 60)
        device.activeVideoMaxFrameDuration = CMTime(value: 1, timescale: 30)
        device.unlockForConfiguration()
    }

    func captureOutput(_: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from _: AVCaptureConnection) {
        guard let room, let buffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard CVPixelBufferGetPlaneCount(buffer) == 2,
              let yBase = CVPixelBufferGetBaseAddressOfPlane(buffer, 0),
              let uvBase = CVPixelBufferGetBaseAddressOfPlane(buffer, 1)
        else { return }
        let width = CVPixelBufferGetWidthOfPlane(buffer, 0)
        let height = CVPixelBufferGetHeightOfPlane(buffer, 0)
        let yStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let uvStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)
        let uvLength = uvStride * CVPixelBufferGetHeightOfPlane(buffer, 1)
        // NV12: U and V interleave in one plane, V one byte after U.
        room.pushCameraFrame(
            width: UInt32(width), height: UInt32(height),
            y: Data(bytes: yBase, count: yStride * height),
            u: Data(bytes: uvBase, count: uvLength),
            v: Data(bytes: uvBase + 1, count: uvLength - 1),
            yStride: UInt32(yStride), uvStride: UInt32(uvStride), uvPixelStride: 2,
            rotationDegrees: 0
        )
    }
}

/// The self view.
struct CameraPreview: UIViewRepresentable {
    let session: AVCaptureSession

    func makeUIView(context _: Context) -> PreviewView {
        let view = PreviewView()
        view.previewLayer.session = session
        view.previewLayer.videoGravity = .resizeAspectFill
        return view
    }

    func updateUIView(_: PreviewView, context _: Context) {}

    final class PreviewView: UIView {
        override class var layerClass: AnyClass { AVCaptureVideoPreviewLayer.self }
        var previewLayer: AVCaptureVideoPreviewLayer { layer as! AVCaptureVideoPreviewLayer }

        override init(frame: CGRect) {
            super.init(frame: frame)
            // The preview's connection only exists once the camera runs, which
            // can be after the first layout.
            NotificationCenter.default.addObserver(
                self, selector: #selector(sessionStarted),
                name: .AVCaptureSessionDidStartRunning, object: nil
            )
        }

        required init?(coder: NSCoder) {
            fatalError("not used from storyboards")
        }

        @objc private func sessionStarted() {
            DispatchQueue.main.async { self.setNeedsLayout() }
        }

        /// Keeps the self view upright when the screen rotates (the two
        /// orientation enums share raw values).
        override func layoutSubviews() {
            super.layoutSubviews()
            guard let connection = previewLayer.connection, connection.isVideoOrientationSupported,
                  let interface = window?.windowScene?.interfaceOrientation,
                  let orientation = AVCaptureVideoOrientation(rawValue: interface.rawValue)
            else { return }
            connection.videoOrientation = orientation
        }
    }
}
