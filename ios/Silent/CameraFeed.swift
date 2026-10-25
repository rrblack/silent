import AVFoundation
import SwiftUI
import UIKit

/// Front or back camera through AVFoundation. Frames go to the room as
/// NV12 (the engine encodes them), and to `session` for the self view.
final class CameraFeed: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    let session = AVCaptureSession()
    /// The room frames go to; only touched on `queue`.
    private var room: SilentRoom?
    private let queue = DispatchQueue(label: "silent.camera")
    private let output = AVCaptureVideoDataOutput()

    /// Frames go to `room` from now on (`nil` while not in a room).
    func attach(_ room: SilentRoom?) {
        queue.async { self.room = room }
    }

    func start(front: Bool, high: Bool) {
        queue.async { self.configure(front: front, high: high) }
    }

    func stop() {
        queue.async {
            if self.session.isRunning { self.session.stopRunning() }
        }
    }

    private func configure(front: Bool, high: Bool) {
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
            return
        }
        session.addInput(input)
        if session.outputs.isEmpty {
            output.videoSettings = [
                kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            ]
            output.alwaysDiscardsLateVideoFrames = true
            output.setSampleBufferDelegate(self, queue: queue)
            if session.canAddOutput(output) { session.addOutput(output) }
        }
        if let connection = output.connection(with: .video) {
            // Upright frames, so the engine needs no rotation.
            if connection.isVideoOrientationSupported { connection.videoOrientation = .portrait }
            if connection.isVideoMirroringSupported { connection.isVideoMirrored = false }
        }
        session.commitConfiguration()
        if !session.isRunning { session.startRunning() }
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
    }
}
