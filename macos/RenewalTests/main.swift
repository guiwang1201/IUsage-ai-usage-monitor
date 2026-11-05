import Foundation

// Every test here uses a mock keychain, a temporary folder and a mock transport. No
// real credential, keychain item or network is touched.
let now: Int64 = 1_790_900_000
let clientVersion = "2.1.281"
func credentials(refresh: String, expiresAt: Int64, access: String = "old-access") -> Data {
    Data("""
    {
      "mcpOAuth": { "accessToken": "keep-me", "list": [1, {"a": "}"}] },
      "claudeAiOauth": {
        "accessToken": "\(access)",
        "refreshToken": "\(refresh)",
        "expiresAt": \(expiresAt * 1000),
        "refreshTokenExpiresAt": 1791900000000,
        "scopes": ["user:inference", "user:profile"]
      },
      "tail": null
    }

    """.utf8)
}
let renewedBody = Data(#"{"access_token":"new-access","refresh_token":"swift-new","expires_in":28800,"refresh_token_expires_in":2592000}"#.utf8)

final class World {
    var keychain: Data?
    var keychainWrites = 0
    var failWrites = false
    var failFromWrite = Int.max
    var posts: [(url: URL, headers: [String: String], body: Data)] = []
    var reply: Result<UsageHTTP.Response, Error> = .success(.init(status: 200, body: renewedBody, retryAfter: nil))
    var duringPost: (() -> Void)?
    var saved = OfflineState()
    var clock = now
    var gateAtPost: SavedGate?
    lazy var runtime = OfflineRuntime(state: saved, save: { [unowned self] in self.saved = $0 })
    let service = "Claude Code-credentials", account = "mock"
    var record: ClaudeCredentialRecord { ClaudeCredentialRecord(data: keychain!, source: .keychain(service: service, account: account)) }
    lazy var store = ClaudeStore(
        reader: CredentialReader(readKeychain: { [unowned self] _, _ in
            guard let data = self.keychain else { throw CredentialFailure.missing }
            return data
        }, readFile: { _ in throw CredentialFailure.missing }),
        writeKeychain: { [unowned self] _, _, data in
            self.keychainWrites += 1
            if self.failWrites || self.keychainWrites >= self.failFromWrite { throw CredentialFailure.accessRequired }
            self.keychain = data
        })
    lazy var renewal = ClaudeRenewal(runtime: runtime, store: store, now: { [unowned self] in self.clock }) { [unowned self] url, headers, body, callback in
        self.posts.append((url, headers, body))
        self.gateAtPost = self.saved.gates[0]
        self.duringPost?()
        callback(self.reply)
    }
    func renew(version: String? = clientVersion) -> ClaudeRenewal.Outcome {
        var result: ClaudeRenewal.Outcome?
        try! renewal.renew(record: record, version: version) { result = $0 }
        let deadline = Date().addingTimeInterval(2)
        while result == nil && Date() < deadline { RunLoop.main.run(until: Date().addingTimeInterval(0.01)) }
        return result!
    }
}
func text(_ data: Data?) -> String { String(decoding: data ?? Data(), as: UTF8.self) }
var checks = 0
func check(_ value: @autoclosure () -> Bool, _ note: String = "", line: Int = #line) { precondition(value(), "check failed at line \(line) \(note)"); checks += 1 }

// 1. An expired token is renewed, the request is reserved before it is sent, only the
// token values change, and the usage request follows one minimum gap later.
do {
    let w = World()
    let before = credentials(refresh: "swift-1", expiresAt: now - 60)
    w.keychain = before
    check(w.renew() == .finished(nil))
    check(w.posts.count == 1)
    let post = w.posts[0]
    check(post.url.absoluteString == "https://platform.claude.com/v1/oauth/token")
    check(post.headers["User-Agent"] == "claude-code/2.1.281" && post.headers["Content-Type"] == "application/json")
    check(text(post.body).contains(#""refresh_token":"swift-1""#) && text(post.body).contains(#""grant_type":"refresh_token""#))
    check(w.gateAtPost?.lastAttempt == now, "reserved and saved before the request")
    let expected = text(before).replacingOccurrences(of: "old-access", with: "new-access")
        .replacingOccurrences(of: "swift-1", with: "swift-new")
        .replacingOccurrences(of: String((now - 60) * 1000), with: String((now + 28_800) * 1000))
        .replacingOccurrences(of: "1791900000000", with: String((now + 2_592_000) * 1000))
    check(text(w.keychain) == expected)
    check(w.saved.gates[0].nextPoll == now + requestGap && w.saved.gates[0].blockedUntil == 0 && w.saved.gates[0].backoff == 0)
    check(w.runtime.issue(0) == nil)
    // The gap holds: no second renewal right away.
    check(w.renew() == .wait)
}
print("PASS: renewal reserves before sending, rewrites only the token values, schedules the next request one gap later")

// 2. Someone else renewed while the request was in flight: their credentials stay.
do {
    let w = World()
    w.keychain = credentials(refresh: "swift-race", expiresAt: now - 60)
    let theirs = credentials(refresh: "theirs", expiresAt: now + 28_000, access: "theirs-access")
    w.duringPost = { w.keychain = theirs }
    check(w.renew() == .finished(nil))
    check(w.keychain == theirs)
}
print("PASS: credentials renewed by another process meanwhile are not overwritten")

// 3. A refused refresh token is not offered again.
do {
    let w = World()
    let before = credentials(refresh: "swift-refused", expiresAt: now - 60)
    w.keychain = before
    w.reply = .success(.init(status: 400, body: Data(#"{"error":"invalid_grant"}"#.utf8), retryAfter: nil))
    check(w.renew() == .finished(ClaudeRenewal.refusedLabel))
    check(w.keychain == before)
    check(w.saved.gates[0].blockedUntil >= now + 300 && w.runtime.issue(0) == ClaudeRenewal.refusedLabel)
    w.clock = now + 400
    check(w.renew() == .issue(LocalIssue(CredentialFailure.expired, provider: 0)))
    check(w.posts.count == 1, "asked once")
    // Signing in again stores a different refresh token, which is tried.
    w.keychain = credentials(refresh: "swift-refused-after-login", expiresAt: now - 60)
    w.reply = .success(.init(status: 200, body: renewedBody, retryAfter: nil))
    check(w.renew() == .finished(nil) && w.posts.count == 2)
}
print("PASS: a refused refresh token waits out the gate and is not offered again until the store changes")

// 4. Rate limit and network failure leave the store alone and use the gate.
do {
    let w = World()
    let before = credentials(refresh: "swift-limited", expiresAt: now - 60)
    w.keychain = before
    w.reply = .success(.init(status: 429, body: Data(), retryAfter: 900))
    check(w.renew() == .finished("限流") && w.keychain == before)
    check(w.saved.gates[0].blockedUntil == now + 900)
    check(w.renew() == .wait, "nothing may ask during the wait")
    let offline = World()
    offline.keychain = before
    offline.reply = .failure(URLError(.notConnectedToInternet))
    check(offline.renew() == .finished("离线") && offline.keychain == before)
    let broken = World()
    broken.keychain = before
    broken.reply = .success(.init(status: 200, body: Data("{}".utf8), retryAfter: nil))
    check(broken.renew() == .finished("数据错误") && broken.keychain == before)
}
print("PASS: rate limits, network failures and unusable answers leave the credentials alone")

// 5. A store that will not take a write is never worth a renewal: nothing is sent,
// nothing is reserved.
do {
    let w = World()
    let before = credentials(refresh: "swift-readonly", expiresAt: now - 60)
    w.keychain = before
    w.failWrites = true
    check(w.renew() == .issue(.cannotSaveCredentials))
    check(w.posts.isEmpty && w.saved.gates[0].lastAttempt == 0 && w.keychain == before)
    // A refused write is not asked for again with the same credentials for a while.
    check(w.keychainWrites == 1)
    for minute in 1...5 { w.clock = now + Int64(minute * 60); check(w.renew() == .issue(.cannotSaveCredentials)) }
    check(w.keychainWrites == 1, "no new write attempts inside the wait")
    w.clock = now + ClaudeRenewal.proofRetrySeconds
    check(w.renew() == .issue(.cannotSaveCredentials) && w.keychainWrites == 2, "tried again once the wait is over")
    // New credentials end the wait at once, and a store that now takes the write is used.
    w.failWrites = false
    w.keychain = credentials(refresh: "swift-readonly-renewed-login", expiresAt: w.clock - 60)
    check(w.renew() == .finished(nil) && w.posts.count == 1)
    // A layout that cannot be patched safely is treated the same way.
    let duplicated = World()
    duplicated.keychain = Data(#"{"claudeAiOauth":{"accessToken":"a","accessToken":"b","refreshToken":"swift-dup","expiresAt":1000}}"#.utf8)
    check(duplicated.renew() == .issue(.cannotSaveCredentials) && duplicated.posts.isEmpty)
}
print("PASS: an unwritable store or unpatchable layout sends nothing and reserves nothing")

// 6. The write that follows a good answer fails: reported, and waited out like a refusal.
do {
    let w = World()
    w.keychain = credentials(refresh: "swift-lost", expiresAt: now - 60)
    w.failFromWrite = 2 // the proof write passes, the real one does not
    check(w.renew() == .finished(ClaudeRenewal.cannotSaveLabel))
    check(w.posts.count == 1 && w.saved.gates[0].blockedUntil >= now + 300)
    check(w.keychainWrites == 4, "the proof, then three tries")
}
print("PASS: a failed write-back is reported and the gate holds off a retry")

// 7. Not expired, changed since read, gate closed, bad version: nothing is sent.
do {
    let w = World()
    w.keychain = credentials(refresh: "swift-fine", expiresAt: now + 3600)
    check(w.renew() == .notNeeded && w.posts.isEmpty && w.keychainWrites == 0)
    let changed = World()
    changed.keychain = credentials(refresh: "swift-changed", expiresAt: now - 60)
    let stale = changed.record
    changed.keychain = credentials(refresh: "swift-changed-by-claude", expiresAt: now + 28_000)
    var outcome: ClaudeRenewal.Outcome?
    try! changed.renewal.renew(record: stale, version: clientVersion) { outcome = $0 }
    check(outcome == .notNeeded && changed.posts.isEmpty && changed.keychainWrites == 0)
    let gated = World()
    gated.keychain = credentials(refresh: "swift-gated", expiresAt: now - 60)
    gated.saved.gates[0].blockedUntil = now + 200
    check(gated.renew() == .wait && gated.keychainWrites == 0 && gated.posts.isEmpty)
    let unversioned = World()
    unversioned.keychain = credentials(refresh: "swift-unversioned", expiresAt: now - 60)
    check(unversioned.renew(version: nil) == .issue(.invalidRequestInput))
    check(unversioned.renew(version: "2.1") == .issue(.invalidRequestInput) && unversioned.posts.isEmpty)
}
print("PASS: nothing is sent when not expired, changed since read, held by the gate, or without a client version")

// 8. The file store: replaced atomically, other bytes kept, symlinks refused.
do {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("iusage-renewal-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }
    let file = dir.appendingPathComponent(".credentials.json")
    let before = credentials(refresh: "swift-file", expiresAt: now - 60)
    try before.write(to: file)
    let store = ClaudeStore(reader: CredentialReader(readKeychain: { _, _ in throw CredentialFailure.missing }))
    let record = try CredentialReader(readKeychain: { _, _ in throw CredentialFailure.missing })
        .claudeRecord(home: dir, account: "mock", configDirectory: dir.path)
    if case .file(let found) = record.source { check(found.path == file.path) } else { fatalError("expected the file source") }
    try store.prove(record)
    var saved = OfflineState()
    let runtime = OfflineRuntime(state: saved, save: { saved = $0 })
    let renewal = ClaudeRenewal(runtime: runtime, store: store, now: { now }) { _, _, _, callback in
        callback(.success(.init(status: 200, body: renewedBody, retryAfter: nil)))
    }
    var outcome: ClaudeRenewal.Outcome?
    try renewal.renew(record: record, version: clientVersion) { outcome = $0 }
    let deadline = Date().addingTimeInterval(2)
    while outcome == nil && Date() < deadline { RunLoop.main.run(until: Date().addingTimeInterval(0.01)) }
    check(outcome == .finished(nil))
    let after = try String(contentsOf: file, encoding: .utf8)
    check(after.contains("new-access") && after.contains("swift-new") && after.contains(#""tail": null"#) && after.contains(#"{"a": "}"}"#))
    let mode = (try FileManager.default.attributesOfItem(atPath: file.path)[.posixPermissions] as? NSNumber)?.intValue
    check(mode == 0o600)
    let leftovers = try FileManager.default.contentsOfDirectory(atPath: dir.path).filter { $0.hasPrefix(".iusage-") }
    check(leftovers.isEmpty)
    let link = dir.appendingPathComponent("link.json")
    try FileManager.default.createSymbolicLink(at: link, withDestinationURL: file)
    check(!ClaudeStore.systemFileReplaceable(link) && ClaudeStore.systemFileReplaceable(file))
    check(!ClaudeStore.systemFileReplaceable(dir.appendingPathComponent("missing.json")))
}
print("PASS: the file store replaces atomically with owner-only permissions, keeps other bytes and refuses symlinks")

// 9. The transport posts only to the token endpoint and gets only the usage endpoints.
do {
    let post = URL(string: "https://platform.claude.com/v1/oauth/token")!
    check(UsageHTTP.allowed(method: "POST", url: post))
    check(!UsageHTTP.allowed(method: "GET", url: post))
    check(!UsageHTTP.allowed(method: "POST", url: URL(string: "https://api.anthropic.com/api/oauth/usage")!))
    check(!UsageHTTP.allowed(method: "POST", url: URL(string: "https://platform.claude.com/v1/oauth/token?x=1")!))
    check(!UsageHTTP.allowed(method: "POST", url: URL(string: "http://platform.claude.com/v1/oauth/token")!))
    check(!UsageHTTP.allowed(method: "POST", url: URL(string: "https://example.com/v1/oauth/token")!))
}
print("PASS: transport allowlist is exact per method and endpoint")
print("PASS: \(checks) renewal checks; mock keychain, temporary files and mock transport only")
