import SwiftUI

@main
struct SilentApp: App {
    @StateObject private var model = RoomModel()

    var body: some Scene {
        WindowGroup {
            Group {
                if model.room != nil {
                    RoomView(model: model)
                } else {
                    HomeView(model: model)
                }
            }
            .preferredColorScheme(.dark)
        }
    }
}

enum Palette {
    static let bg = Color(red: 0x07 / 255, green: 0x07 / 255, blue: 0x07 / 255)
    static let panel = Color(red: 0x10 / 255, green: 0x10 / 255, blue: 0x10 / 255)
    static let card = Color(red: 0x17 / 255, green: 0x17 / 255, blue: 0x17 / 255)
    static let text = Color(red: 0xF2 / 255, green: 0xF2 / 255, blue: 0xF2 / 255)
    static let subtext = Color(red: 0xA3 / 255, green: 0xA3 / 255, blue: 0xA3 / 255)
    static let accent = Color(red: 0xE1 / 255, green: 0x1D / 255, blue: 0x2E / 255)
    static let accentDim = Color(red: 0x4A / 255, green: 0x0F / 255, blue: 0x16 / 255)
    static let danger = Color(red: 0x9B / 255, green: 0x11 / 255, blue: 0x1E / 255)
}
