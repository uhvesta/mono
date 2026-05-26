import Foundation
import Security

/// Storage for the user-supplied `ANTHROPIC_API_KEY`.
///
/// Lives in the macOS Keychain (generic-password class) rather than
/// `UserDefaults` so the secret is not readable from a plain
/// `defaults read` dump and benefits from the same OS-level protections
/// as other stored credentials. See work item #735 — pre-#735 the only
/// way to feed a key to the engine was a shell `export`, which doesn't
/// survive launching Boss from Finder/Spotlight.
///
/// Resolution at engine-launch time: when this store has a value,
/// `EngineProcessController` injects it into the engine subprocess
/// env, overriding any inherited `ANTHROPIC_API_KEY`. With no stored
/// value the engine inherits whatever the user exported in their
/// shell, preserving the pre-#735 behaviour.
///
/// Storage backend: the *data-protection* keychain (every query passes
/// `kSecUseDataProtectionKeychain`). The legacy file-based keychain
/// authorizes reads via an interactive ACL tied to the calling app's
/// code signature; ad-hoc dev builds re-sign with a fresh cdhash on
/// every rebuild, so "Always Allow" never sticks and macOS re-prompts
/// on each launch and Settings open. Data-protection items are gated by
/// keychain-access-group membership (declared in `Boss.entitlements` /
/// `installer/entitlements/app.entitlements`) and never raise a dialog.
/// Note: a key stored under the pre-migration legacy keychain is not
/// visible here — the user re-enters it once in Settings.
enum APIKeyStore {
    /// Service identifier for every keychain item this app owns.
    /// Distinct from `CFBundleIdentifier` so a future "second secret"
    /// can reuse the same generic-password class without colliding on
    /// the (service, account) primary key.
    static let service = "dev.spinyfin.bossmacapp.secrets"

    /// Account name for the `ANTHROPIC_API_KEY` entry. Matches the env
    /// var the engine reads so the indirection has only one canonical
    /// name to keep in sync.
    static let anthropicApiKeyAccount = "ANTHROPIC_API_KEY"

    /// Read the stored API key, or `nil` if no entry exists.
    ///
    /// Returns `nil` on any non-`errSecItemNotFound` error too — a
    /// corrupted keychain entry must not block engine launch. The user
    /// can still set the key from their shell or paste it again in
    /// Settings to overwrite the bad entry.
    static func readAnthropicApiKey() -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: anthropicApiKeyAccount,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
            // Use the data-protection keychain rather than the legacy
            // file-based one. Legacy items gate reads on an interactive
            // ACL bound to the app's code signature, so ad-hoc dev builds
            // (whose cdhash changes every rebuild) re-trigger the
            // "Boss wants to use your confidential information" dialog on
            // every launch even after "Always Allow". Data-protection
            // items are gated by keychain-access-group membership instead,
            // so no password dialog is ever raised. See Boss.entitlements.
            kSecUseDataProtectionKeychain as String: true,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        guard status == errSecSuccess,
              let data = item as? Data,
              let value = String(data: data, encoding: .utf8),
              !value.isEmpty
        else {
            return nil
        }
        return value
    }

    /// Persist a non-empty API key, replacing any existing entry.
    /// Empty / whitespace-only values are rejected so a "save" never
    /// silently clobbers a real key with an unusable empty string —
    /// callers wanting "clear" must call `clearAnthropicApiKey()`.
    static func saveAnthropicApiKey(_ value: String) throws {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            throw APIKeyStoreError.empty
        }
        guard let data = trimmed.data(using: .utf8) else {
            throw APIKeyStoreError.encodingFailed
        }

        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: anthropicApiKeyAccount,
            kSecUseDataProtectionKeychain as String: true,
        ]
        let attributesToUpdate: [String: Any] = [
            kSecValueData as String: data,
        ]

        let updateStatus = SecItemUpdate(query as CFDictionary, attributesToUpdate as CFDictionary)
        switch updateStatus {
        case errSecSuccess:
            return
        case errSecItemNotFound:
            var addQuery = query
            addQuery[kSecValueData as String] = data
            // Restrict access to "this device only, after first unlock"
            // so the key never roams via iCloud Keychain and is unavailable
            // when the device is locked at boot.
            addQuery[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            let addStatus = SecItemAdd(addQuery as CFDictionary, nil)
            guard addStatus == errSecSuccess else {
                throw APIKeyStoreError.keychainStatus(addStatus)
            }
        default:
            throw APIKeyStoreError.keychainStatus(updateStatus)
        }
    }

    /// Remove any stored API key. Idempotent — succeeds even if no
    /// entry exists, so callers can wire a "Clear" button without
    /// worrying about double-click races.
    static func clearAnthropicApiKey() throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: anthropicApiKeyAccount,
            kSecUseDataProtectionKeychain as String: true,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw APIKeyStoreError.keychainStatus(status)
        }
    }
}

enum APIKeyStoreError: Error, LocalizedError {
    case empty
    case encodingFailed
    case keychainStatus(OSStatus)

    var errorDescription: String? {
        switch self {
        case .empty:
            return "API key cannot be empty."
        case .encodingFailed:
            return "Could not encode API key as UTF-8."
        case .keychainStatus(let status):
            let message = SecCopyErrorMessageString(status, nil) as String?
            return "Keychain error \(status): \(message ?? "unknown")"
        }
    }
}
