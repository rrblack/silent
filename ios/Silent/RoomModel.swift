import AudioToolbox
import AVFoundation
import Foundation
import Network
import UIKit

struct Friend: Identifiable {
    let id: UInt32
    var name = "Friend"
    var status = "Connecting…"
    var verified = false
    var mic = true
    var camera = false
    var screen = false
    var words: String?
    var mutedForMe = false
    /// How loud they play on this phone, in percent.
    var volume = 100
    /// You compared safety words with them and they matched.
    var confirmed = false
}

struct ChatLine: Identifiable {
    let id = UUID()
    let time = Date()
    let who: String
    var text: String
    var mine = false
    var system = false
    /// Set for a file line; the key to save it with.
    var file: String?
    var fileReady = false
    var image: UIImage?
}

/// Files can be up to this big (the engine's limit).
let maxFileBytes = 64 * 1024 * 1024

func sizeText(_ bytes: Int) -> String {
    ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
}

struct TileKey: Hashable {
    let peer: UInt32
    let screen: Bool
}

/// The call and everything the screens show. Lives on the main thread;
/// engine callbacks are hopped over to it.
final class RoomModel: ObservableObject {
    // Home screen
    @Published var name = ""
    @Published var joinCode = ""
    @Published var relayUrl = RoomModel.bakedRelay
    @Published var relayOnly = false
    @Published var sounds = true
    @Published var cameraHigh = false
    @Published var busy = false
    @Published var error = ""

    // Room
    @Published private(set) var room: SilentRoom?
    @Published private(set) var code = ""
    @Published private(set) var created = false
    @Published private(set) var micOn = true
    @Published private(set) var cameraOn = false
    @Published private(set) var frontCamera = true
    @Published private(set) var screenOn = false
    @Published private(set) var notice = ""
    @Published private(set) var friends: [UInt32: Friend] = [:]
    @Published private(set) var chat: [ChatLine] = []
    @Published private(set) var frames: [TileKey: UIImage] = [:]
    @Published private(set) var stats: [String] = []
    /// Who is talking now; includes `me` for yourself.
    @Published private(set) var speaking: Set<UInt32> = []
    private(set) var me: UInt32 = 0

    let camera = CameraFeed()
    private let screen = ScreenReceiver()
    /// Received files, in memory only, until the room is left.
    private(set) var files: [String: (name: String, data: Data)] = [:]
    private var sentFiles = 0
    /// The listener of the room being opened or in use; callbacks from any
    /// other (a room already left) are dropped.
    private var listener: Listener?
    private var network: NWPathMonitor?
    private var interface: NWInterface.InterfaceType?

    private static var bakedRelay: String {
        let url = Bundle.main.object(forInfoDictionaryKey: "SilentRelayURL") as? String ?? ""
        return url.hasPrefix("$") ? "" : url
    }

    func create() { open(code: nil) }

    func join() {
        if let problem = checkCode(code: joinCode) {
            error = "That doesn't look like a Silent code: \(problem)."
            return
        }
        open(code: joinCode.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private func open(code: String?) {
        let relay = relayUrl.trimmingCharacters(in: .whitespacesAndNewlines)
        if relay.isEmpty {
            error = "Set a relay server under Settings first."
            return
        }
        busy = true
        error = ""
        let settings = RoomSettings(
            relayUrl: relay, name: name, cameraHigh: cameraHigh,
            screenDetail: false, relayOnly: relayOnly, useAudio: true
        )
        let listener = Listener(model: self)
        self.listener = listener
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Result<SilentRoom, Error> {
                if let code {
                    return try joinRoom(code: code, settings: settings, listener: listener)
                }
                return try createRoom(settings: settings, listener: listener)
            }
            DispatchQueue.main.async {
                self.busy = false
                switch result {
                case .success(let room):
                    self.entered(room, created: code == nil)
                case .failure(let error):
                    if self.listener === listener { self.listener = nil }
                    self.error = error.localizedDescription
                }
            }
        }
    }

    private func entered(_ room: SilentRoom, created: Bool) {
        self.room = room
        self.created = created
        code = room.code()
        me = room.me()
        speaking = []
        micOn = true
        cameraOn = false
        notice = ""
        friends = [:]
        chat = []
        frames = [:]
        files = [:]
        camera.attach(room)
        camera.onFailure = { [weak self] in self?.cameraFailed() }
        screenOn = false
        screen.onSharing = { [weak self] on in self?.screenOn = on }
        screen.start(room: room)
        startAudioSession()
        watchNetwork()
        UIApplication.shared.isIdleTimerDisabled = true
        if created {
            system("Room created. Share the code with up to three friends.")
        }
    }

    func leave(reason: String = "") {
        guard let room else { return }
        self.room = nil
        listener = nil
        camera.stop()
        camera.attach(nil)
        screen.stop()
        screenOn = false
        cameraOn = false
        network?.cancel()
        network = nil
        UIApplication.shared.isIdleTimerDisabled = false
        error = reason
        DispatchQueue.global(qos: .userInitiated).async {
            room.leave()
            DispatchQueue.main.async {
                // Not if a new call started meanwhile.
                guard self.room == nil, !self.busy else { return }
                try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
            }
        }
    }

    func toggleMic() {
        micOn.toggle()
        room?.setMic(on: micOn)
    }

    func toggleCamera() {
        if cameraOn {
            cameraOn = false
            room?.setCamera(on: false)
            camera.stop()
            return
        }
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            cameraStarted()
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .video) { granted in
                DispatchQueue.main.async {
                    if granted { self.cameraStarted() } else { self.cameraDenied() }
                }
            }
        default:
            cameraDenied()
        }
    }

    private func cameraStarted() {
        guard room != nil, !cameraOn else { return }
        cameraOn = true
        room?.setCamera(on: true)
        camera.start(front: frontCamera, high: cameraHigh)
    }

    private func cameraDenied() {
        system("Silent can't use the camera. Allow it in Settings → Silent.")
    }

    /// The camera couldn't start (in use elsewhere, or no camera).
    private func cameraFailed() {
        guard cameraOn else { return }
        cameraOn = false
        room?.setCamera(on: false)
        system("The camera couldn't start.")
    }

    private func updateFileLine(_ key: String, _ change: (inout ChatLine) -> Void) {
        guard let i = chat.lastIndex(where: { $0.file == key }) else { return }
        change(&chat[i])
    }

    /// Sends a file the user picked. Reading it and making the preview
    /// happen off the main thread.
    func sendFile(at url: URL) {
        guard let room else { return }
        let fileName = url.lastPathComponent
        DispatchQueue.global(qos: .userInitiated).async {
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
            let data = size <= maxFileBytes ? (try? Data(contentsOf: url)) : nil
            let result = Result<Void, Error> {
                if let data { try room.sendFile(name: fileName, data: data) }
            }
            let preview = data.flatMap(thumbnail)
            DispatchQueue.main.async {
                guard self.room === room else { return }
                guard let data else {
                    self.system("\(fileName) can't be sent: files can be up to 64 MB.")
                    return
                }
                if case .failure(let error) = result {
                    self.system("Couldn't send \(fileName): \(error.localizedDescription)")
                    return
                }
                self.sentFiles += 1
                self.chat.append(ChatLine(
                    who: self.name.isEmpty ? "Me" : self.name,
                    text: "File: \(fileName) (\(sizeText(data.count)))",
                    mine: true, file: "me:\(self.sentFiles)", fileReady: true,
                    image: preview
                ))
            }
        }
    }

    func note(_ text: String) {
        system(text)
    }

    func toggleConfirmed(_ peer: UInt32) {
        friends[peer]?.confirmed.toggle()
    }

    /// Silences a friend for this phone only.
    func toggleMute(friend peer: UInt32) {
        guard var friend = friends[peer] else { return }
        friend.mutedForMe.toggle()
        friends[peer] = friend
        room?.setFriendMuted(peer: peer, muted: friend.mutedForMe)
    }

    /// How loud a friend plays on this phone only, 0 to 200 percent.
    func setVolume(friend peer: UInt32, percent: Int) {
        guard let friend = friends[peer], friend.volume != percent else { return }
        friends[peer]?.volume = percent
        room?.setFriendVolume(peer: peer, volume: Double(percent) / 100)
    }

    func flipCamera() {
        frontCamera.toggle()
        if cameraOn {
            camera.start(front: frontCamera, high: cameraHigh)
        }
    }

    func send(_ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, let room else { return }
        room.sendChat(text: trimmed)
        chat.append(ChatLine(who: name.isEmpty ? "Me" : name, text: trimmed, mine: true))
    }

    func copyCode() {
        UIPasteboard.general.string = code
    }

    func refreshStats() {
        guard let room else { return }
        let names = friends.mapValues(\.name)
        DispatchQueue.global(qos: .utility).async {
            let lines = room.stats().flatMap { s -> [String] in
                let who = names[s.peer] ?? "Friend"
                let rtt = s.rttMs.map { " (round trip \(Int($0)) ms)" } ?? ""
                return ["\(who)\(rtt)"] + s.lines.map { "    \($0)" }
            }
            DispatchQueue.main.async {
                self.stats = lines.isEmpty ? ["No video flowing yet."] : lines
            }
        }
    }

    /// The system's begin and end recording sounds for a friend joining or
    /// leaving.
    private func chime(joined: Bool) {
        guard sounds else { return }
        AudioServicesPlaySystemSound(joined ? 1113 : 1114)
    }

    private func system(_ text: String) {
        chat.append(ChatLine(who: "", text: text, system: true))
    }

    fileprivate func apply(_ update: RoomUpdate, from listener: Listener) {
        guard listener === self.listener else { return }
        switch update {
        case .peerJoining(let peer):
            friends[peer] = Friend(id: peer)
        case .peerVerified(let peer, let name, let replaces):
            var returning = false
            if let old = replaces, let previous = friends.removeValue(forKey: old) {
                returning = true
                friends[peer]?.mutedForMe = previous.mutedForMe
                friends[peer]?.volume = previous.volume
            }
            if let old = replaces {
                frames[TileKey(peer: old, screen: false)] = nil
                frames[TileKey(peer: old, screen: true)] = nil
            }
            if !returning {
                system("\(name) joined")
                chime(joined: true)
            }
            friends[peer]?.name = name
            friends[peer]?.status = "Securing connection…"
            friends[peer]?.verified = true
        case .peerWrongCode(let peer):
            friends[peer] = nil
            if created {
                system("Someone tried to join with the wrong code and was kept out.")
            } else if friends.isEmpty {
                leave(reason: "That code doesn't match the room. Check the words and try again.")
            }
        case .peerConnected(let peer, let safetyWords):
            friends[peer]?.status = ""
            friends[peer]?.words = safetyWords.joined(separator: " ")
        case .peerConnectionLost(let peer):
            friends[peer]?.status = "Reconnecting…"
        case .peerLeft(let peer):
            if let friend = friends.removeValue(forKey: peer), friend.verified {
                system("\(friend.name) left")
                chime(joined: false)
            }
            frames[TileKey(peer: peer, screen: false)] = nil
            frames[TileKey(peer: peer, screen: true)] = nil
        case .peerMedia(let peer, let mic, let camera, let screen):
            friends[peer]?.mic = mic
            friends[peer]?.camera = camera
            friends[peer]?.screen = screen
            if !camera { frames[TileKey(peer: peer, screen: false)] = nil }
            if !screen { frames[TileKey(peer: peer, screen: true)] = nil }
        case .chat(let from, let text, _):
            chat.append(ChatLine(who: friends[from]?.name ?? "Friend", text: text))
        case .localMediaError(_, let reason):
            system(reason)
        case .relayLost:
            notice = "Relay offline: the call continues, reconnecting…"
        case .relayRestored:
            if notice.hasPrefix("Relay") { notice = "" }
        case .speaking(let peers):
            speaking = Set(peers)
        case .removed:
            leave(reason: "You were removed from the room: your code doesn't match. Check the words and try again.")
        case .fileIncoming(let from, let id, let name, let size):
            chat.append(ChatLine(
                who: friends[from]?.name ?? "Friend",
                text: "File: \(name) (\(sizeText(Int(size)))) · receiving…",
                file: "\(from):\(id)"
            ))
        case .fileReceived(let from, let id, let name, let data):
            let key = "\(from):\(id)"
            files[key] = (name, data)
            updateFileLine(key) { line in
                line.text = "File: \(name) (\(sizeText(data.count)))"
                line.fileReady = true
            }
            let room = self.room
            DispatchQueue.global(qos: .utility).async {
                guard let preview = thumbnail(data) else { return }
                DispatchQueue.main.async {
                    guard self.room === room else { return }
                    self.updateFileLine(key) { $0.image = preview }
                }
            }
        case .fileFailed(let from, let id):
            updateFileLine("\(from):\(id)") { line in
                line.text = line.text.replacingOccurrences(of: " · receiving…", with: "") + " · didn't arrive"
            }
        }
    }

    fileprivate func show(_ shown: [TileKey: UIImage], from listener: Listener) {
        guard listener === self.listener else { return }
        for (key, image) in shown {
            // A frame still in flight when the friend turned it off stays away.
            guard let friend = friends[key.peer], key.screen ? friend.screen : friend.camera else { continue }
            frames[key] = image
        }
    }

    private func startAudioSession() {
        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(
                .playAndRecord, mode: .voiceChat,
                options: [.defaultToSpeaker, .allowBluetooth, .allowBluetoothA2DP]
            )
            try session.setActive(true)
        } catch {
            system("Audio is unavailable: \(error.localizedDescription)")
        }
    }

    /// Restarts ICE when the phone switches networks (Wi-Fi to mobile data).
    private func watchNetwork() {
        interface = nil
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            let current = path.status == .satisfied ? path.availableInterfaces.first?.type : nil
            DispatchQueue.main.async { self?.networkChanged(to: current) }
        }
        monitor.start(queue: DispatchQueue(label: "silent.network"))
        network = monitor
    }

    private func networkChanged(to current: NWInterface.InterfaceType?) {
        defer { if current != nil { interface = current } }
        guard let current, let previous = interface, previous != current else { return }
        room?.restartConnections()
    }
}

/// A small preview of an image file, or `nil` for other files.
private func thumbnail(_ data: Data) -> UIImage? {
    UIImage(data: data)?.preparingThumbnail(of: CGSize(width: 440, height: 440))
}

/// Receives one room's engine callbacks on background threads.
private final class Listener: RoomListener, @unchecked Sendable {
    weak var model: RoomModel?
    /// Only the newest frame per tile waits for the main thread, so a busy
    /// main thread drops frames instead of piling them up.
    private let lock = NSLock()
    private var pending: [TileKey: UIImage] = [:]
    private var scheduled = false

    init(model: RoomModel) {
        self.model = model
    }

    func onUpdate(update: RoomUpdate) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.model?.apply(update, from: self)
        }
    }

    func onVideoFrame(peer: UInt32, screen: Bool, width: UInt32, height: UInt32, rgba: Data) {
        guard let image = Listener.image(width: Int(width), height: Int(height), rgba: rgba) else { return }
        lock.lock()
        pending[TileKey(peer: peer, screen: screen)] = image
        let schedule = !scheduled
        scheduled = true
        lock.unlock()
        guard schedule else { return }
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.lock.lock()
            let shown = self.pending
            self.pending = [:]
            self.scheduled = false
            self.lock.unlock()
            self.model?.show(shown, from: self)
        }
    }

    private static func image(width: Int, height: Int, rgba: Data) -> UIImage? {
        guard width > 0, height > 0, rgba.count >= width * height * 4,
              let provider = CGDataProvider(data: rgba as CFData),
              let cg = CGImage(
                  width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32,
                  bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
                  provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent
              )
        else { return nil }
        return UIImage(cgImage: cg)
    }
}
