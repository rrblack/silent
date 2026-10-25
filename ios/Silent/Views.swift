import SwiftUI
import UniformTypeIdentifiers

/// A received file handed to the save dialog. Never written anywhere else.
struct FileBlob: FileDocument {
    static var readableContentTypes: [UTType] { [.data] }
    let data: Data

    init(data: Data) {
        self.data = data
    }

    init(configuration: ReadConfiguration) throws {
        data = configuration.file.regularFileContents ?? Data()
    }

    func fileWrapper(configuration _: WriteConfiguration) throws -> FileWrapper {
        FileWrapper(regularFileWithContents: data)
    }
}

struct HomeView: View {
    @ObservedObject var model: RoomModel
    @State private var showSettings = false

    var body: some View {
        ScrollView {
            VStack(spacing: 16) {
                Text("Silent").font(.system(size: 44, weight: .heavy)).foregroundColor(Palette.text)
                Text("Encrypted calls and chat for up to four friends.\nNo accounts. Nothing stored.")
                    .font(.subheadline).foregroundColor(Palette.subtext).multilineTextAlignment(.center)

                field("Your name", text: $model.name)
                Button(action: model.create) {
                    Text(model.busy ? "Connecting…" : "Start a room").bold().frame(maxWidth: .infinity).padding(12)
                }
                .background(Palette.accent).foregroundColor(.white).clipShape(Capsule())
                .disabled(model.busy)

                Text("or join a friend").font(.caption).foregroundColor(Palette.subtext)
                HStack {
                    field("1234-word-word-word", text: $model.joinCode)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .onSubmit(model.join)
                    Button("Join", action: model.join)
                        .disabled(model.busy || model.joinCode.isEmpty)
                        .padding(.horizontal, 18).padding(.vertical, 12)
                        .background(Palette.card).foregroundColor(Palette.text).clipShape(Capsule())
                }

                if !model.error.isEmpty {
                    Text(model.error).font(.footnote).foregroundColor(Palette.accent).multilineTextAlignment(.center)
                }

                Button(showSettings ? "Hide settings" : "Settings") { showSettings.toggle() }
                    .font(.footnote).foregroundColor(Palette.accent)
                if showSettings {
                    Toggle("Camera in 1080p, 60 fps", isOn: $model.cameraHigh)
                    Toggle("Hide my IP from friends (relay all media)", isOn: $model.relayOnly)
                    field("Relay server", text: $model.relayUrl)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                }
            }
            .tint(Palette.accent)
            .foregroundColor(Palette.text)
            .padding(24)
            .frame(maxWidth: 460)
            .frame(maxWidth: .infinity)
        }
        .background(Palette.bg.ignoresSafeArea())
    }

    private func field(_ placeholder: String, text: Binding<String>) -> some View {
        TextField(placeholder, text: text)
            .padding(12)
            .background(Palette.card)
            .clipShape(RoundedRectangle(cornerRadius: 10))
    }
}

struct RoomView: View {
    @ObservedObject var model: RoomModel
    @State private var showChat = false
    @State private var showVerify = false
    @State private var showStats = false
    @State private var draft = ""
    @State private var picking = false
    @State private var saving: (name: String, blob: FileBlob)?

    private enum Tile: Identifiable {
        case me
        case friend(Friend, screen: Bool)

        var id: String {
            switch self {
            case .me: return "me"
            case .friend(let f, let screen): return "\(f.id):\(screen)"
            }
        }
    }

    private var friends: [Friend] { model.friends.values.sorted { $0.id < $1.id } }

    private var tiles: [Tile] {
        [.me] + friends.map { .friend($0, screen: false) } + friends.filter(\.screen).map { .friend($0, screen: true) }
    }

    var body: some View {
        VStack(spacing: 8) {
            HStack {
                Button(action: model.copyCode) {
                    VStack(alignment: .leading) {
                        Text("Room code · tap to copy").font(.caption2).foregroundColor(Palette.subtext)
                        Text(model.code).font(.headline).foregroundColor(Palette.text)
                    }
                }
                Spacer()
                let connected = friends.contains { $0.words != nil }
                Button("Stats") { showStats = true }.disabled(!connected)
                Button("Verify") { showVerify = true }.disabled(!connected)
            }
            .padding(.horizontal, 12)
            if !model.notice.isEmpty {
                Text(model.notice).font(.caption).foregroundColor(Palette.subtext)
            }

            ScrollView {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 160), spacing: 8)], spacing: 8) {
                    ForEach(tiles) { tile($0) }
                }
                .padding(.horizontal, 8)
            }

            HStack(spacing: 14) {
                control(model.micOn ? "mic.fill" : "mic.slash.fill", active: !model.micOn, action: model.toggleMic)
                control(model.cameraOn ? "video.fill" : "video.slash.fill", active: model.cameraOn, action: model.toggleCamera)
                if model.cameraOn {
                    control("arrow.triangle.2.circlepath.camera", active: false, action: model.flipCamera)
                }
                // Starts and stops the broadcast extension, which streams the screen.
                BroadcastButton()
                    .frame(width: 52, height: 52)
                    .background(model.screenOn ? Palette.accent : Palette.card)
                    .clipShape(Circle())
                control("bubble.left.fill", active: showChat) { showChat = true }
                control("phone.down.fill", active: false, danger: true) { model.leave() }
            }
            .padding(.bottom, 8)
        }
        .tint(Palette.accent)
        .background(Palette.bg.ignoresSafeArea())
        .sheet(isPresented: $showChat) { chat }
        .sheet(isPresented: $showVerify) { verify }
        .sheet(isPresented: $showStats) { stats }
    }

    @ViewBuilder
    private func tile(_ tile: Tile) -> some View {
        ZStack(alignment: .bottomLeading) {
            Palette.card
            switch tile {
            case .me:
                if model.cameraOn {
                    CameraPreview(session: model.camera.session)
                } else {
                    avatar(model.name.isEmpty ? "You" : model.name, status: "")
                }
                label((model.micOn ? "" : "muted · ") + (model.name.isEmpty ? "You" : model.name) + " (you)", muted: !model.micOn)
            case .friend(let friend, let screen):
                if let image = model.frames[TileKey(peer: friend.id, screen: screen)] {
                    Image(uiImage: image).resizable().aspectRatio(contentMode: screen ? .fit : .fill)
                } else {
                    avatar(friend.name, status: friend.status)
                }
                label(
                    (friend.mic || screen ? "" : "muted · ") + friend.name + (screen ? "'s screen" : "")
                        + (friend.mutedForMe && !screen ? " · muted for you" : "")
                        + (friend.confirmed && !screen ? " · verified" : ""),
                    muted: !friend.mic && !screen
                )
            }
        }
        .aspectRatio(3 / 4, contentMode: .fit)
        .clipShape(RoundedRectangle(cornerRadius: 12))
        .overlay(
            RoundedRectangle(cornerRadius: 12)
                .stroke(isSpeaking(tile) ? Palette.ok : Color.clear, lineWidth: 3)
        )
        .contextMenu {
            if case .friend(let friend, false) = tile {
                Button(friend.mutedForMe ? "Unmute for me" : "Mute for me") {
                    model.toggleMute(friend: friend.id)
                }
            }
        }
    }

    private func isSpeaking(_ tile: Tile) -> Bool {
        switch tile {
        case .me: return model.speaking.contains(model.me)
        case .friend(let friend, let screen): return !screen && model.speaking.contains(friend.id)
        }
    }

    private func avatar(_ name: String, status: String) -> some View {
        VStack(spacing: 8) {
            Text(String(name.prefix(1)).uppercased())
                .font(.title.bold()).foregroundColor(Palette.text)
                .frame(width: 64, height: 64).background(Palette.accentDim).clipShape(Circle())
            if !status.isEmpty {
                Text(status).font(.caption).foregroundColor(Palette.subtext)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func label(_ text: String, muted: Bool) -> some View {
        Text(text).font(.caption).foregroundColor(Palette.text).lineLimit(1)
            .padding(.horizontal, 10).padding(.vertical, 4)
            .background(muted ? Palette.danger : Color.black.opacity(0.7)).clipShape(Capsule())
            .padding(8)
    }

    private func control(_ symbol: String, active: Bool, danger: Bool = false, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol).font(.title3).foregroundColor(.white).frame(width: 52, height: 52)
                .background(danger ? Palette.danger : active ? Palette.accent : Palette.card).clipShape(Circle())
        }
    }

    private var chat: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Chat").font(.headline)
            Text("End-to-end encrypted. Gone when you leave.").font(.caption).foregroundColor(Palette.subtext)
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    ForEach(model.chat) { line in
                        if line.system {
                            Text(line.text).font(.footnote.italic()).foregroundColor(Palette.subtext)
                        } else {
                            VStack(alignment: .leading, spacing: 2) {
                                HStack(spacing: 6) {
                                    Text(line.who).font(.caption.bold()).foregroundColor(line.mine ? Palette.accent : Palette.text)
                                    Text(line.time, style: .time).font(.caption2).foregroundColor(Palette.subtext)
                                }
                                Text(line.text).foregroundColor(Palette.text)
                                if let image = line.image {
                                    Image(uiImage: image).resizable().scaledToFit()
                                        .frame(maxWidth: 220, maxHeight: 220).clipShape(RoundedRectangle(cornerRadius: 8))
                                }
                                if let key = line.file, line.fileReady, !line.mine, let file = model.files[key] {
                                    Button("Save…") { saving = (file.name, FileBlob(data: file.data)) }
                                        .font(.footnote)
                                }
                            }
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            HStack {
                Button { picking = true } label: { Image(systemName: "paperclip") }
                    // Its own view: iOS 16 shows only one of an importer and
                    // an exporter attached to the same view.
                    .fileImporter(isPresented: $picking, allowedContentTypes: [.item]) { result in
                        if case .success(let url) = result { model.sendFile(at: url) }
                    }
                TextField("Message", text: $draft)
                    .padding(10).background(Palette.card).clipShape(RoundedRectangle(cornerRadius: 10))
                    .onSubmit(sendDraft)
                Button("Send", action: sendDraft)
            }
        }
        .padding(16)
        .fileExporter(
            isPresented: Binding(get: { saving != nil }, set: { if !$0 { saving = nil } }),
            document: saving?.blob,
            contentType: .data,
            defaultFilename: saving?.name
        ) { result in
            if case .success(let url) = result { model.note("Saved \(url.lastPathComponent).") }
        }
        .background(Palette.panel.ignoresSafeArea())
        .tint(Palette.accent)
    }

    private func sendDraft() {
        model.send(draft)
        draft = ""
    }

    private var verify: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Check for eavesdroppers").font(.headline)
            Text("Read your words aloud with each friend. If they match, nobody is listening in. If they differ, leave the room.")
                .font(.footnote).foregroundColor(Palette.subtext)
            ForEach(friends.filter { $0.words != nil }) { friend in
                VStack(alignment: .leading, spacing: 4) {
                    Text(friend.name).font(.caption).foregroundColor(Palette.subtext)
                    HStack {
                        Text(friend.words ?? "").font(.title3.bold())
                        Spacer()
                        Button(friend.confirmed ? "Verified" : "They match") { model.toggleConfirmed(friend.id) }
                            .foregroundColor(friend.confirmed ? Palette.ok : Palette.accent)
                    }
                }
            }
            Spacer()
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Palette.panel.ignoresSafeArea())
    }

    private var stats: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Call quality").font(.headline)
            Text("Refreshed every second.").font(.caption).foregroundColor(Palette.subtext)
            ForEach(Array(model.stats.enumerated()), id: \.offset) { _, line in
                Text(line).font(.caption.monospaced())
            }
            Spacer()
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Palette.panel.ignoresSafeArea())
        .task {
            while !Task.isCancelled {
                model.refreshStats()
                try? await Task.sleep(nanoseconds: 1_000_000_000)
            }
        }
    }
}
