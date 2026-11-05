import Foundation

struct AlertMark: Codable {
    var level = 0
    var reset: Int64?
}
enum AlertPolicy {
    // The rules (20%, 10% and 5% left, once each per window, re-armed by a passed reset)
    // live in the shared Rust `alert` module, as on Windows. This only marshals the
    // persisted marks across the C interface.
    static func evaluate(reading: SavedReading, marks: inout [AlertMark], now: Int64) -> [String] {
        var usage = reading.native
        var announced = marks.map { UInt8(clamping: $0.level) }
        let previous = marks.map { $0.reset ?? 0 }
        guard announced.count == 2 else { return [] }
        var buffer = [UInt8](repeating: 0, count: 512)
        var length = 0
        let code = iusage_alert_check(&usage, &announced, previous, now, &buffer, buffer.count, &length)
        guard code == 0 else { return [] }
        let resets = [reading.fiveReset, reading.sevenReset]
        for i in 0...1 { marks[i] = AlertMark(level: Int(announced[i]), reset: resets[i]) }
        return String(decoding: buffer[..<length], as: UTF8.self).split(separator: "\n").map(String.init)
    }
}

extension SavedReading {
    /// The reading laid out as `iusage_parse` leaves it, for the shared Rust rules.
    var native: MacUsage {
        var usage = MacUsage()
        if let five = five {
            usage.five_hour = five; usage.present |= 1
            if let reset = fiveReset { usage.five_reset = reset; usage.present |= 4 }
        }
        if let seven = seven {
            usage.seven_day = seven; usage.present |= 2
            if let reset = sevenReset { usage.seven_reset = reset; usage.present |= 8 }
        }
        return usage
    }
}

enum PacePolicy {
    // Whether the quota should last until it resets, worded and judged by the shared
    // Rust `panel` rules, as in the Windows detail panel. Callers show it only for a
    // fresh reading: after a failed fetch the numbers may be out of date.
    static func line(reading: SavedReading, now: Int64, utcOffset: Int64 = Int64(TimeZone.current.secondsFromGMT())) -> (text: String, warn: Bool) {
        var usage = reading.native
        var buffer = [UInt8](repeating: 0, count: 256)
        var length = 0
        var warn = false
        guard iusage_pace(&usage, now, utcOffset, &buffer, buffer.count, &length, &warn) == 0 else { return ("", false) }
        return (String(decoding: buffer[..<length], as: UTF8.self), warn)
    }
}
