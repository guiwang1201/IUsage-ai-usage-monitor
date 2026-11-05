import Foundation
import Security
import LocalAuthentication
import CryptoKit

// No networking, token refresh, credential writes, or interactive Keychain prompts.
enum CredentialFailure: Error, Equatable {
    case missing, accessRequired, unavailable(Int32), malformed, expired, unsupportedLogin
}

struct OAuthCredential {
    let accessToken: String
    let accountID: String?
}

enum ClaudeSource: Equatable {
    case keychain(service: String, account: String)
    case file(URL)
}

struct ClaudeCredentialRecord {
    let data: Data
    let source: ClaudeSource
}

struct CredentialReader {
    typealias KeychainRead = (String, String) throws -> Data
    let readKeychain: KeychainRead
    let readFile: (URL) throws -> Data

    init(readKeychain: @escaping KeychainRead = CredentialReader.systemKeychain,
         readFile: @escaping (URL) throws -> Data = CredentialReader.systemFile) {
        self.readKeychain = readKeychain
        self.readFile = readFile
    }

    /// Credentials are small; as on Windows, the read itself is bounded, so a bloated or
    /// replaced file cannot consume arbitrary memory. Oversized files read as malformed.
    static let maxFileBytes = 1_048_576
    static func systemFile(_ url: URL) throws -> Data {
        do {
            let handle = try FileHandle(forReadingFrom: url)
            defer { try? handle.close() }
            let data = try handle.read(upToCount: maxFileBytes + 1) ?? Data()
            guard data.count <= maxFileBytes else { throw CredentialFailure.malformed }
            return data
        }
        catch let failure as CredentialFailure { throw failure }
        catch let error as NSError {
            if error.domain == NSCocoaErrorDomain && [NSFileReadNoSuchFileError, NSFileNoSuchFileError].contains(error.code)
                || error.domain == NSPOSIXErrorDomain && error.code == Int(ENOENT) {
                throw CredentialFailure.missing
            }
            throw CredentialFailure.accessRequired
        }
    }

    static func systemKeychain(service: String, account: String) throws -> Data {
        let context = LAContext()
        context.interactionNotAllowed = true
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
            kSecReturnData: true,
            kSecMatchLimit: kSecMatchLimitOne,
            kSecUseAuthenticationContext: context
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        switch status {
        case errSecSuccess:
            guard let data = result as? Data else { throw CredentialFailure.malformed }
            return data
        case errSecItemNotFound: throw CredentialFailure.missing
        case errSecInteractionNotAllowed, errSecAuthFailed, errSecUserCanceled:
            throw CredentialFailure.accessRequired
        default: throw CredentialFailure.unavailable(status)
        }
    }

    // Matches the inspected production Claude Code client. Non-production OAuth
    // environments are deliberately not guessed or scanned.
    static func claudeService(configDirectory: String?, secureStorageDirectory: String?) -> String {
        let selected = secureStorageDirectory ?? configDirectory
        guard let selected, !selected.isEmpty else { return "Claude Code-credentials" }
        let normalized = selected.precomposedStringWithCanonicalMapping
        let hash = SHA256.hash(data: Data(normalized.utf8)).map { String(format: "%02x", $0) }.joined()
        return "Claude Code-credentials-" + hash.prefix(8)
    }

    func claude(home: URL, account: String, configDirectory: String? = nil,
                secureStorageDirectory: String? = nil, now: Date = Date()) throws -> OAuthCredential {
        let record = try claudeRecord(home: home, account: account, configDirectory: configDirectory,
                                      secureStorageDirectory: secureStorageDirectory)
        return try Self.parseClaude(record.data, now: now)
    }

    // The stored text and where it was found, so a renewed token can go back to the
    // same place and nowhere else.
    func claudeRecord(home: URL, account: String, configDirectory: String? = nil,
                      secureStorageDirectory: String? = nil) throws -> ClaudeCredentialRecord {
        let service = Self.claudeService(configDirectory: configDirectory, secureStorageDirectory: secureStorageDirectory)
        do {
            return ClaudeCredentialRecord(data: try readKeychain(service, account), source: .keychain(service: service, account: account))
        } catch CredentialFailure.missing {
            let folder = configDirectory.flatMap { $0.isEmpty ? nil : URL(fileURLWithPath: $0) }
                ?? home.appendingPathComponent(".claude")
            let url = folder.appendingPathComponent(".credentials.json")
            return ClaudeCredentialRecord(data: try readFile(url), source: .file(url))
        }
    }

    func read(_ source: ClaudeSource) throws -> Data {
        switch source {
        case .keychain(let service, let account): return try readKeychain(service, account)
        case .file(let url): return try readFile(url)
        }
    }

    // File mode only for now. Callers must not use it as a fallback for keyring,
    // auto, or ephemeral without first resolving the configured storage mode.
    func codexFile(directory: URL) throws -> OAuthCredential {
        try Self.parseCodex(readFile(directory.appendingPathComponent("auth.json")))
    }

    static func object(_ data: Data) throws -> [String: Any] {
        guard data.count <= 1_048_576,
              let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw CredentialFailure.malformed
        }
        return root
    }

    static func token(_ value: Any?) throws -> String {
        guard let value = value as? String, !value.isEmpty,
              !value.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else {
            throw CredentialFailure.malformed
        }
        return value
    }

    static func parseClaude(_ data: Data, now: Date) throws -> OAuthCredential {
        let root = try object(data)
        guard let oauth = root["claudeAiOauth"] as? [String: Any] else { throw CredentialFailure.unsupportedLogin }
        let access = try token(oauth["accessToken"])
        if let raw = oauth["expiresAt"] {
            guard let expiry = raw as? NSNumber, CFGetTypeID(expiry) != CFBooleanGetTypeID(),
                  expiry.doubleValue.isFinite else { throw CredentialFailure.malformed }
            if expiry.doubleValue / 1000 <= now.timeIntervalSince1970 { throw CredentialFailure.expired }
        }
        return OAuthCredential(accessToken: access, accountID: nil)
    }

    static func parseCodex(_ data: Data) throws -> OAuthCredential {
        let root = try object(data)
        guard let tokens = root["tokens"] as? [String: Any] else { throw CredentialFailure.unsupportedLogin }
        let access = try token(tokens["access_token"])
        let account = try tokens["account_id"].map { try token($0) }
        return OAuthCredential(accessToken: access, accountID: account)
    }
}
