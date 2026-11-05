import Foundation
var saved = OfflineState()
var clock: Int64 = 1000
var sends = 0
var pending: ((Result<UsageHTTP.Response, Error>) -> Void)?
let runtime = OfflineRuntime(state: saved, save: { saved = $0 })
let coordinator = FetchCoordinator(runtime: runtime, now: { clock }) { url, headers, callback in
    precondition(saved.gates[1].lastAttempt == clock) // persisted before transport
    precondition(url.host == "chatgpt.com" && headers["Authorization"] == "Bearer mock")
    sends += 1; pending = callback
}
var finished = false
precondition(try! coordinator.fetch(provider: 1, token: "mock", account: nil) { _ in finished = true })
clock = 1200 // even after the gap, the outstanding request remains unique
precondition(try! !coordinator.fetch(provider: 1, token: "mock", account: nil) { _ in })
precondition(sends == 1)
pending?(.success(UsageHTTP.Response(status: 429, body: Data(), retryAfter: 600)))
let deadline = Date().addingTimeInterval(2)
while !finished && Date() < deadline { RunLoop.main.run(until: Date().addingTimeInterval(0.01)) }
precondition(finished && saved.gates[1].blockedUntil == 1800)
precondition(try! !coordinator.fetch(provider: 1, token: "mock", account: nil) { _ in })
print("PASS: async coordinator persists reservation, prohibits overlap, and retains 429 cooldown; mock only")

func drain(_ ready: () -> Bool) {
    let end = Date().addingTimeInterval(2)
    while !ready() && Date() < end { RunLoop.main.run(until: Date().addingTimeInterval(0.01)) }
    precondition(ready())
}
let session = try! ClaudeSession("sessionKey=test-key; ")
precondition(session.key == "test-key")
for key in ["", "x; other=y", "x\r\nHeader:y", "é"] {
    precondition((try? ClaudeSession(key)) == nil)
}
precondition((try? ClaudeSession("x", organization: "../bad")) == nil)
precondition((try? ClaudeSession.discover(Data(#"[{"uuid":"one"},{"uuid":"two"}]"#.utf8))) == nil)
var webSaved = OfflineState()
let webRuntime = OfflineRuntime(state: webSaved, save: { webSaved = $0 })
var webCalls = 0
var webPending: ((Result<UsageHTTP.Response, Error>) -> Void)?
var webURL: URL?
let web = FetchCoordinator(runtime: webRuntime, now: { clock }) { url, headers, callback in
    precondition(webSaved.gates[0].lastAttempt == clock)
    precondition(headers["Cookie"] == "sessionKey=test-key" && headers["Authorization"] == nil)
    webCalls += 1; webPending = callback; webURL = url
}
clock = 5000; finished = false
precondition(try! web.fetchClaude(session: session) { _ in finished = true })
precondition(webURL?.path == "/api/organizations")
webPending?(.success(.init(status: 200, body: Data(#"[{"uuid":"org-1"}]"#.utf8), retryAfter: nil)))
drain { finished }
precondition(webSaved.claudeOrganization == "org-1" && webSaved.readings[0] == nil)
precondition(try! !web.fetchClaude(session: session) { _ in })
clock += requestGap; finished = false
precondition(try! web.fetchClaude(session: session) { _ in finished = true })
precondition(webURL?.path == "/api/organizations/org-1/usage")
webPending?(.success(.init(status: 429, body: Data(), retryAfter: 86400)))
drain { finished }
precondition(webSaved.gates[0].blockedUntil == clock + 86400)
try! webRuntime.configureClaude(identity: session.identity)
precondition(webSaved.gates[0].blockedUntil == clock + 86400)
let restored = OfflineRuntime(state: webSaved, save: { _ in })
precondition(!restored.canRequest(provider: 0, now: clock + 7200))
clock += 86400; finished = false
precondition(try! web.fetchClaude(session: session) { _ in finished = true })
webPending?(.success(.init(status: 401, body: Data(), retryAfter: nil)))
drain { finished }
clock += 90000
precondition(try! !web.fetchClaude(session: session) { _ in })
precondition(webSaved.claudeSessionExpired == true && webCalls == 3)
let expired = OfflineRuntime(state: webSaved, save: { _ in })
let noSend = FetchCoordinator(runtime: expired, now: { clock }) { _, _, _ in fatalError("expired session sent") }
precondition(try! !noSend.fetchClaude(session: session) { _ in })
let failing = OfflineRuntime(state: OfflineState(), save: { _ in throw CocoaError(.fileWriteNoPermission) })
let neverSend = FetchCoordinator(runtime: failing, now: { clock }) { _, _, _ in fatalError("save failed but sent") }
do { _ = try neverSend.fetchClaude(session: session) { _ in }; fatalError("must fail") } catch {}
print("PASS: SessionKey validation, discovery, persisted gaps, 24h cooldown, login edits, restart, 401 stop, save failure; zero network")

let explicit = try! ClaudeSession("test-key", organization: "org-2")
var directState = OfflineState()
let directRuntime = OfflineRuntime(state: directState, save: { directState = $0 })
let direct = FetchCoordinator(runtime: directRuntime, now: { clock }) { url, _, callback in
    precondition(url.path == "/api/organizations/org-2/usage")
    webPending = callback
}
for (status, body) in [(200, #"{"five_hour":{"utilization":42},"seven_day":null}"#), (403, ""), (200, "malformed")] {
    clock += 1000; finished = false
    precondition(try! direct.fetchClaude(session: explicit) { _ in finished = true })
    webPending?(.success(.init(status: status, body: Data(body.utf8), retryAfter: nil)))
    drain { finished }
    precondition(directState.readings[0]?.five == 42)
    precondition(directState.claudeSessionExpired != true)
}
let diskState = try! JSONDecoder().decode(OfflineState.self, from: JSONEncoder().encode(webSaved))
precondition(diskState.claudeSessionExpired == true && diskState.claudeSessionIdentity == session.identity)
let oldGate = directState.gates[0].blockedUntil
try! directRuntime.configureClaude(identity: "another-session")
precondition(directState.readings[0] == nil && directState.gates[0].blockedUntil == oldGate)
print("PASS: explicit organization, shared usage parser, 403/non-JSON retain cache, persisted expiry, account isolation")
