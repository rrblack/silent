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
}

struct ChatLine: Identifiable {
    let id = UUID()
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
    @Published private(set) var notice = ""
    @Published private(set) var friends: [UInt32: Friend] = [:]
    @Published private(set) var chat: [ChatLine] = []
    @Published private(set) var frames: [TileKey: UIImage] = [:]
    @Published private(set) var stats: [String] = []
    /// Who is talking now; includes `me` for yourself.
    @Published private(set) var speaking: Set<UInt32> = []
    private(set) var me: UInt32 = 0

    let camera = CameraFeed()
    /// Received files, in memory only, until the room is left.
    private(set) var files: [String: (name: String, data: Data)] = [:]
    private var sentFiles = 0
    private lazy var listener = Listener(model: self)
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
        let listener = self.listener
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
        camera.stop()
        camera.attach(nil)
        cameraOn = false
        network?.cancel()
        network = nil
        UIApplication.shared.isIdleTimerDisabled = false
        error = reason
        DispatchQueue.global(qos: .userInitiated).async {
            room.leave()
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        }
    }

    func toggleMic() {
        micOn.toggle()
        room?.setMic(on: micOn)
    }

    func toggleCamera() {
        cameraOn.toggle()
        room?.setCamera(on: cameraOn)
        if cameraOn {
            camera.start(front: frontCamera, high: cameraHigh)
        } else {
            camera.stop()
        }
    }

    private func updateFileLine(_ key: String, _ change: (inout ChatLine) -> Void) {
        guard let i = chat.lastIndex(where: { $0.file == key }) else { return }
        change(&chat[i])
    }

    /// Sends a file the user picked.
    func sendFile(at url: URL) {
        guard let room else { return }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
        guard size <= maxFileBytes, let data = try? Data(contentsOf: url) else {
            system("\(url.lastPathComponent) can't be sent: files can be up to 64 MB.")
            return
        }
        do {
            try room.sendFile(name: url.lastPathComponent, data: data)
        } catch {
            system("Couldn't send \(url.lastPathComponent): \(error.localizedDescription)")
            return
        }
        sentFiles += 1
        chat.append(ChatLine(
            who: name.isEmpty ? "Me" : name,
            text: "File: \(url.lastPathComponent) (\(sizeText(data.count)))",
            mine: true, file: "me:\(sentFiles)", fileReady: true,
            image: UIImage(data: data)?.preparingThumbnail(of: CGSize(width: 440, height: 440))
        ))
    }

    func note(_ text: String) {
        system(text)
    }

    /// Silences a friend for this phone only.
    func toggleMute(friend peer: UInt32) {
        guard var friend = friends[peer] else { return }
        friend.mutedForMe.toggle()
        friends[peer] = friend
        room?.setFriendMuted(peer: peer, muted: friend.mutedForMe)
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

    private func system(_ text: String) {
        chat.append(ChatLine(who: "", text: text, system: true))
    }

    fileprivate func apply(_ update: RoomUpdate) {
        switch update {
        case .peerJoining(let peer):
            friends[peer] = Friend(id: peer)
        case .peerVerified(let peer, let name, let replaces):
            var returning = false
            if let old = replaces, let previous = friends.removeValue(forKey: old) {
                returning = true
                friends[peer]?.mutedForMe = previous.mutedForMe
            }
            if let old = replaces {
                frames[TileKey(peer: old, screen: false)] = nil
                frames[TileKey(peer: old, screen: true)] = nil
            }
            if !returning { system("\(name) joined") }
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
                line.image = UIImage(data: data)?.preparingThumbnail(of: CGSize(width: 440, height: 440))
            }
        case .fileFailed(let from, let id):
            updateFileLine("\(from):\(id)") { line in
                line.text = line.text.replacingOccurrences(of: " · receiving…", with: "") + " · didn't arrive"
            }
        }
    }

    fileprivate func show(_ image: UIImage, for key: TileKey) {
        guard friends[key.peer] != nil else { return }
        frames[key] = image
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

/// Receives engine callbacks on background threads.
private final class Listener: RoomListener, @unchecked Sendable {
    weak var model: RoomModel?

    init(model: RoomModel) {
        self.model = model
    }

    func onUpdate(update: RoomUpdate) {
        DispatchQueue.main.async { [weak self] in self?.model?.apply(update) }
    }

    func onVideoFrame(peer: UInt32, screen: Bool, width: UInt32, height: UInt32, rgba: Data) {
        guard let image = Listener.image(width: Int(width), height: Int(height), rgba: rgba) else { return }
        let key = TileKey(peer: peer, screen: screen)
        DispatchQueue.main.async { [weak self] in self?.model?.show(image, for: key) }
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
