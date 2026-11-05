import Foundation
var usage = MacUsage()
let body = Data(#"{"five_hour":{"utilization":32,"resets_at":"2026-10-03T00:00:00Z"},"seven_day":{"utilization":64}}"#.utf8)
let status = body.withUnsafeBytes { bytes in
    iusage_parse(0, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count, &usage)
}
precondition(status == 0 && usage.five_hour == 32 && usage.seven_day == 64 && usage.present == 7)
// The gap is the shared Windows value (src/schedule.rs); follow it rather than a copy.
let gap = iusage_min_gap_secs()
precondition(gap >= 30)
var gate = MacGate()
precondition(iusage_start(&gate, 1000))
precondition(!iusage_start(&gate, 1000 + gap - 1))
precondition(iusage_can_request(&gate, 1000 + gap))
let limited = 1000 + gap
precondition(iusage_complete(&gate, limited, 429, 600, UInt32(gap)))
precondition(!iusage_can_request(&gate, limited + 599))
precondition(iusage_can_request(&gate, limited + 600))
let restored = gate
precondition(restored.blocked_until == limited + 600 && restored.backoff == 300)
precondition(iusage_complete(&gate, limited + 600, 429, 0, UInt32(gap)))
precondition(gate.backoff == 600 && gate.blocked_until == limited + 1200)
precondition(iusage_complete(&gate, limited + 1200, 200, 0, UInt32(gap)))
precondition(gate.backoff == 0 && gate.next_poll == limited + 1200 + gap)
print("PASS: Swift/Rust ABI parsing and request-gate integration (offline)")
