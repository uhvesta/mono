import Foundation
import Security

/// Storage for the user-supplied `ANTHROPIC_API_KEY`.
///
/// Persists the secret so it is not readable from a plain `defaults read`
/// dump and benefits from OS-level protections. See work item #735 —
/// pre-#735 the only way to feed a key to the engine was a shell
/// `export`, which doesn't survive launching Boss from Finder/Spotlight.
///
/// Resolution at engine-launch time: when this store has a value,
/// `EngineProcessController` injects it into the engine subprocess
/// env, overriding any inherited `ANTHROPIC_API_KEY`. With no stored
/// value the engine inherits whatever the user exported in their
/// shell, preserving the pre-#735 behaviour.
///
/// ## Storage backend (issue #784)
///
/// The backend is chosen at runtime, because the legacy *file-based*
/// keychain authorizes reads via an interactive ACL bound to the calling
/// app's code signature: ad-hoc dev builds re-sign with a fresh cdhash on
/// every rebuild, so "Always Allow" never sticks and macOS re-prompts on
/// each launch and Settings open.
///
///   - **Developer ID release builds** carry the `keychain-access-groups`
///     entitlement (`installer/entitlements/app.entitlements`) and use the
///     *data-protection* keychain (`kSecUseDataProtectionKeychain`). Those
///     items are gated by access-group membership rather than an ACL, so
///     no password dialog is ever raised regardless of signature.
///   - **Ad-hoc dev builds** (the bazel `macos_application`, which has no
///     provisioning profile) MUST NOT carry that entitlement:
///     `keychain-access-groups` is an Apple *restricted* entitlement and
///     AMFI SIGKILLs an ad-hoc binary that declares it at exec (#784 — the
///     reason c4e15f51 had to be reverted in #786). Without the
///     entitlement, `SecItemAdd` / `SecItemUpdate` return
///     `errSecMissingEntitlement` (-34018), so we fall back to a `0600`
///     file under Application Support. A file's identity is stable across
///     rebuilds, so unlike the legacy keychain it never re-prompts when
///     the ad-hoc cdhash changes — and it never raises a dialog at all.
///
/// Selection cannot be a compile-time flag: release builds are the *same*
/// bazel binary re-signed by `installer/release.sh`, so the running
/// process checks the `keychain-access-groups` entitlement at runtime via
/// `SecTaskCopyValueForEntitlement` to discover whether it is entitled.
/// Note: a `SecItemCopyMatching` read-probe is NOT used here because it
/// returns `errSecItemNotFound` on both entitled *and* unentitled processes
/// when no item exists yet — it cannot distinguish dev from release builds.
///
/// A key previously stored under the legacy keychain is not visible to
/// either backend; the user re-enters it once in Settings (no
/// auto-migration, since a legacy read would itself raise the dialog we
/// are removing).
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

    /// Env var that forces the file backend at an explicit path. Set by
    /// tests so they never read or clobber the real user's stored key
    /// (the #705 production-path hazard) and never depend on the test
    /// host's code signature. Also serves as a power-user escape hatch.
    static let fileOverrideEnvVar = "BOSS_API_KEY_FILE"

    // MARK: - Public API

    /// Read the stored API key, or `nil` if no entry exists.
    ///
    /// Returns `nil` on any error too — a corrupted entry must not block
    /// engine launch. The user can still set the key from their shell or
    /// paste it again in Settings to overwrite the bad entry.
    static func readAnthropicApiKey() -> String? {
        switch backend() {
        case .keychain:
            return keychainRead()
        case .file(let path):
            return fileRead(path)
        }
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
        switch backend() {
        case .keychain:
            try keychainSave(data)
        case .file(let path):
            try fileSave(data, at: path)
        }
    }

    /// Remove any stored API key. Idempotent — succeeds even if no
    /// entry exists, so callers can wire a "Clear" button without
    /// worrying about double-click races.
    static func clearAnthropicApiKey() throws {
        switch backend() {
        case .keychain:
            try keychainClear()
        case .file(let path):
            try fileClear(path)
        }
    }

    // MARK: - Backend selection

    private enum Backend {
        case keychain
        case file(path: String)
    }

    /// Resolve the active backend for this process. Recomputed on each
    /// call (the keychain probe is a single cheap query and these
    /// operations are infrequent) so a test can point `fileOverrideEnvVar`
    /// at a temp path per-test without process-wide caching getting in
    /// the way.
    private static func backend() -> Backend {
        if let override = ProcessInfo.processInfo.environment[fileOverrideEnvVar],
           !override.isEmpty {
            return .file(path: override)
        }
        return dataProtectionKeychainAvailable()
            ? .keychain
            : .file(path: defaultFilePath())
    }

    /// `true` when the data-protection keychain is usable by this
    /// process. Detected by checking whether the `keychain-access-groups`
    /// entitlement is present in the running process.
    ///
    /// A `SecItemCopyMatching` read-probe was used previously, but it
    /// is unreliable: `SecItemCopyMatching` returns `errSecItemNotFound`
    /// on both entitled *and* unentitled processes when no item exists,
    /// so the probe cannot distinguish a Developer ID release build from
    /// an ad-hoc dev build. `SecItemAdd` / `SecItemUpdate` do enforce the
    /// entitlement and fail with `errSecMissingEntitlement` (-34018) on
    /// ad-hoc builds — which is exactly the production bug this probe is
    /// meant to prevent. Checking the entitlement directly is accurate
    /// and involves no keychain I/O.
    private static func dataProtectionKeychainAvailable() -> Bool {
        guard let task = SecTaskCreateFromSelf(nil) else { return false }
        let value = SecTaskCopyValueForEntitlement(
            task, "keychain-access-groups" as CFString, nil)
        return value != nil
    }

    /// Default file-backend location: a `0600` file in the shared
    /// `Boss` Application Support directory (the same dir the engine
    /// control token lives in — see `BossEnginePaths`).
    private static func defaultFilePath() -> String {
        let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
        return "\(home)/Library/Application Support/Boss/anthropic-api-key"
    }

    // MARK: - Keychain backend (Developer ID release)

    private static func keychainRead() -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: anthropicApiKeyAccount,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
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

    private static func keychainSave(_ data: Data) throws {
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

    private static func keychainClear() throws {
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

    // MARK: - File backend (ad-hoc dev)

    private static func fileRead(_ path: String) -> String? {
        guard let data = FileManager.default.contents(atPath: path),
              let value = String(data: data, encoding: .utf8)?
                  .trimmingCharacters(in: .whitespacesAndNewlines),
              !value.isEmpty
        else {
            return nil
        }
        return value
    }

    private static func fileSave(_ data: Data, at path: String) throws {
        let url = URL(fileURLWithPath: path)
        let fileManager = FileManager.default
        // Create the parent dir 0700 so even the brief atomic-write temp
        // file is unreadable by other local users. Existing dirs keep
        // their perms (createDirectory only applies attrs to new dirs),
        // which is fine — the engine already owns this dir 0700.
        do {
            try fileManager.createDirectory(
                at: url.deletingLastPathComponent(),
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            try data.write(to: url, options: [.atomic])
            // .atomic renames a temp file into place, so the final file
            // inherits umask perms — force 0600 explicitly.
            try fileManager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: path)
        } catch {
            throw APIKeyStoreError.fileError(error)
        }
    }

    private static func fileClear(_ path: String) throws {
        let fileManager = FileManager.default
        guard fileManager.fileExists(atPath: path) else {
            return
        }
        do {
            try fileManager.removeItem(atPath: path)
        } catch {
            throw APIKeyStoreError.fileError(error)
        }
    }
}

enum APIKeyStoreError: Error, LocalizedError {
    case empty
    case encodingFailed
    case keychainStatus(OSStatus)
    case fileError(Error)

    var errorDescription: String? {
        switch self {
        case .empty:
            return "API key cannot be empty."
        case .encodingFailed:
            return "Could not encode API key as UTF-8."
        case .keychainStatus(let status):
            let message = SecCopyErrorMessageString(status, nil) as String?
            return "Keychain error \(status): \(message ?? "unknown")"
        case .fileError(let error):
            return "Could not access API key file: \(error.localizedDescription)"
        }
    }
}
