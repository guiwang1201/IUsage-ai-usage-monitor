import Foundation

var count = 0
func check(_ value: @autoclosure () -> Bool) { precondition(value()); count += 1 }
func fails(_ expected: CredentialFailure, _ action: () throws -> Void) {
    do { try action(); fatalError("Expected failure") }
    catch let error as CredentialFailure { check(error == expected) }
    catch { fatalError("Unexpected error type") }
}
let valid = Data(#"{"claudeAiOauth":{"accessToken":"mock-token","expiresAt":2000000}}"#.utf8)
let now = Date(timeIntervalSince1970: 1000)
let home = URL(fileURLWithPath: "/mock")
check(CredentialReader.claudeService(configDirectory: nil, secureStorageDirectory: nil) == "Claude Code-credentials")
check(CredentialReader.claudeService(configDirectory: "/custom", secureStorageDirectory: "") == "Claude Code-credentials")
check(CredentialReader.claudeService(configDirectory: "/a", secureStorageDirectory: nil) != CredentialReader.claudeService(configDirectory: "/b", secureStorageDirectory: nil))
check(CredentialReader.claudeService(configDirectory: "/e\u{301}", secureStorageDirectory: nil) == CredentialReader.claudeService(configDirectory: "/é", secureStorageDirectory: nil))
var fileReads = 0
let reader = CredentialReader(readKeychain: { _, _ in valid }, readFile: { _ in fileReads += 1; return valid })
check(try! reader.claude(home: home, account: "mock", now: now).accessToken == "mock-token")
check(fileReads == 0)
let fallback = CredentialReader(readKeychain: { _, _ in throw CredentialFailure.missing }, readFile: { url in check(url.path == "/custom/.credentials.json"); return valid })
check(try! fallback.claude(home: home, account: "mock", configDirectory: "/custom", now: now).accessToken == "mock-token")
for failure in [CredentialFailure.accessRequired, .unavailable(-1)] {
    let blocked = CredentialReader(readKeychain: { _, _ in throw failure }, readFile: { _ in fatalError("Must not bypass Keychain failure") })
    fails(failure) { _ = try blocked.claude(home: home, account: "mock", now: now) }
}
fails(.expired) { _ = try CredentialReader.parseClaude(valid, now: Date(timeIntervalSince1970: 2000)) }
fails(.malformed) { _ = try CredentialReader.parseClaude(Data(#"{"claudeAiOauth":{"accessToken":"x","expiresAt":true}}"#.utf8), now: now) }
fails(.malformed) { _ = try CredentialReader.parseCodex(Data(#"{"tokens":{"access_token":"x\r\ny"}}"#.utf8)) }
fails(.unsupportedLogin) { _ = try CredentialReader.parseCodex(Data(#"{"OPENAI_API_KEY":"mock"}"#.utf8)) }
fails(.malformed) { _ = try CredentialReader.parseCodex(Data("[]".utf8)) }
check(try! CredentialReader.parseCodex(Data(#"{"tokens":{"access_token":"mock","account_id":"test"}}"#.utf8)).accountID == "test")
print("PASS: \(count) offline checks; no real credentials or network used")
check(try! CodexStorageMode.parseConfiguration("# no override") == .file)
check(try! CodexStorageMode.parseConfiguration("cli_auth_credentials_store = 'keyring' # note") == .keyring)
check(try! CodexStorageMode.parseConfiguration("[profiles.test]\ncli_auth_credentials_store = 'auto'") == .file)
fails(.malformed) { _ = try CodexStorageMode.parseConfiguration("cli_auth_credentials_store = 'unknown'") }
let noFallback = CredentialReader(readKeychain: { _, _ in fatalError("No guessed keyring") }, readFile: { _ in fatalError("No stale file fallback") })
for mode in [CodexStorageMode.ephemeral] {
    fails(.unsupportedLogin) { _ = try noFallback.codex(directory: home, mode: mode) }
}
print("PASS: Codex storage selection; unsupported modes do not read stale files")

fails(.malformed) { _ = try CodexStorageMode.parseConfiguration("cli_auth_credentials_store = # missing") }

let codexMock = Data(#"{"tokens":{"access_token":"mock-keychain","account_id":"mock"}}"#.utf8)
let keyReader = CredentialReader(readKeychain: { service, key in
    check(service == "Codex Auth" && key.hasPrefix("cli|") && key.count == 20)
    return codexMock
}, readFile: { _ in fatalError("Keyring must win") })
check(try! keyReader.codex(directory: home, mode: .keyring).accessToken == "mock-keychain")
check(try! keyReader.codex(directory: home, mode: .auto).accessToken == "mock-keychain")
let absentKey = CredentialReader(readKeychain: { _, _ in throw CredentialFailure.missing }, readFile: { _ in codexMock })
check(try! absentKey.codex(directory: home, mode: .auto).accountID == "mock")
fails(.missing) { _ = try absentKey.codex(directory: home, mode: .keyring) }
print("PASS: direct Codex Keychain, auto fallback, keyring isolation")

fails(.unsupportedLogin) { _ = try noFallback.codex(directory: home, mode: .auto, backend: .secrets) }
print("PASS: encrypted secrets backend is explicitly rejected")

let claudeLabels = [CredentialFailure.missing, .expired, .accessRequired, .malformed, .unsupportedLogin, .unavailable(-25308)].map { LocalIssue($0, provider: 0) }
check(Set(claudeLabels.map(\.label)).count == claudeLabels.count)
check(claudeLabels.allSatisfy { !$0.label.isEmpty && $0.recheckSeconds >= 15 })
check(LocalIssue(CredentialFailure.missing, provider: 0).label.hasPrefix("未登录"))
check(LocalIssue(CredentialFailure.expired, provider: 0).label.contains("Claude Code"))
check(LocalIssue(CredentialFailure.expired, provider: 1).label.contains("Codex"))
// A missing or expired login is the case a person fixes within seconds; others are not.
check(LocalIssue(CredentialFailure.missing, provider: 1).recheckSeconds == 15)
check(LocalIssue(CredentialFailure.expired, provider: 1).recheckSeconds == 15)
check(LocalIssue(CredentialFailure.malformed, provider: 0).recheckSeconds == 60)
check(LocalIssue(CocoaError(.fileReadUnknown), provider: 0).label == "凭据不可用")
print("PASS: local credential problems map to distinct labels and recheck intervals")

// As on Windows, credential reads are bounded: a bloated file is rejected without loading it all.
let sizeDir = FileManager.default.temporaryDirectory.appendingPathComponent("iusage-size-\(UUID().uuidString)")
try! FileManager.default.createDirectory(at: sizeDir, withIntermediateDirectories: true)
defer { try? FileManager.default.removeItem(at: sizeDir) }
let small = sizeDir.appendingPathComponent("small.json"), large = sizeDir.appendingPathComponent("large.json")
try! Data("{}".utf8).write(to: small)
try! Data(count: CredentialReader.maxFileBytes + 1).write(to: large)
check((try? CredentialReader.systemFile(small)) == Data("{}".utf8))
do { _ = try CredentialReader.systemFile(large); check(false) } catch CredentialFailure.malformed {} catch { check(false) }
do { _ = try CredentialReader.systemFile(sizeDir.appendingPathComponent("absent.json")); check(false) } catch CredentialFailure.missing {} catch { check(false) }
print("PASS: credential file reads are bounded to 1 MiB")
