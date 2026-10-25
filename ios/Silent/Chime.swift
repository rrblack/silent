import Foundation

/// The desktop app's join and leave chimes, made in memory as WAV data.
/// They play through AVAudioPlayer: iOS keeps system sounds quiet while the
/// microphone records, which it does for the whole call.
enum Chime {
    static let joined = wav(notes: [(659.3, 0), (880, 0.09)])
    static let left = wav(notes: [(880, 0), (587.3, 0.09)])

    private static let rate = 44_100

    /// Each note a sine with a quick attack and an exponential fade.
    private static func wav(notes: [(frequency: Double, start: Double)]) -> Data {
        let note = 0.22
        let count = Int((0.09 + note) * Double(rate))
        var samples = [Double](repeating: 0, count: count)
        for (frequency, start) in notes {
            let first = Int(start * Double(rate))
            for i in first ..< count {
                let t = Double(i - first) / Double(rate)
                if t > note { break }
                samples[i] += 0.25 * min(t / 0.005, 1) * exp(-t * 18) * sin(2 * .pi * frequency * t)
            }
        }

        var data = Data()
        func put<T: FixedWidthInteger>(_ value: T) {
            withUnsafeBytes(of: value.littleEndian) { data.append(contentsOf: $0) }
        }
        data.append(contentsOf: Array("RIFF".utf8))
        put(UInt32(36 + count * 2))
        data.append(contentsOf: Array("WAVEfmt ".utf8))
        put(UInt32(16)) // format chunk size
        put(UInt16(1)) // PCM
        put(UInt16(1)) // mono
        put(UInt32(rate))
        put(UInt32(rate * 2)) // bytes per second
        put(UInt16(2)) // bytes per frame
        put(UInt16(16)) // bits per sample
        data.append(contentsOf: Array("data".utf8))
        put(UInt32(count * 2))
        for sample in samples {
            put(Int16(max(-1, min(1, sample)) * 32767))
        }
        return data
    }
}
