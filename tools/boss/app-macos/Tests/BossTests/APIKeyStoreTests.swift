import XCTest
@testable import Boss

/// Covers the file backend of [[APIKeyStore]] — the path ad-hoc dev
/// builds take when the data-protection keychain is unavailable (issue
/// #784). Setting `BOSS_API_KEY_FILE` forces the file backend at a temp
/// path, so the suite never probes the keychain, never depends on the
/// test host's code signature, and never reads or clobbers the real
/// user's stored key (the #705 production-path hazard).
final class APIKeyStoreTests: XCTestCase {
    private var keyPath: String!

    override func setUp() {
        super.setUp()
        // A path two levels deep that does NOT yet exist, so saving also
        // exercises intermediate-directory creation.
        let dir = NSTemporaryDirectory()
            + "boss-apikeystore-tests-\(UUID().uuidString)/nested"
        keyPath = dir + "/anthropic-api-key"
        setenv(APIKeyStore.fileOverrideEnvVar, keyPath, 1)
    }

    override func tearDown() {
        unsetenv(APIKeyStore.fileOverrideEnvVar)
        if let keyPath {
            // Remove the whole temp tree, not just the file.
            let root = URL(fileURLWithPath: keyPath)
                .deletingLastPathComponent()
                .deletingLastPathComponent()
            try? FileManager.default.removeItem(at: root)
        }
        keyPath = nil
        super.tearDown()
    }

    func testReadReturnsNilWhenNoFileExists() {
        XCTAssertNil(APIKeyStore.readAnthropicApiKey())
    }

    func testSaveThenReadRoundTrips() throws {
        try APIKeyStore.saveAnthropicApiKey("sk-ant-test-123")
        XCTAssertEqual(APIKeyStore.readAnthropicApiKey(), "sk-ant-test-123")
    }

    func testSaveCreatesIntermediateDirectories() throws {
        XCTAssertFalse(FileManager.default.fileExists(atPath: keyPath))
        try APIKeyStore.saveAnthropicApiKey("sk-ant-test-123")
        XCTAssertTrue(FileManager.default.fileExists(atPath: keyPath))
    }

    func testSaveTrimsSurroundingWhitespace() throws {
        try APIKeyStore.saveAnthropicApiKey("  sk-ant-test-123\n")
        XCTAssertEqual(APIKeyStore.readAnthropicApiKey(), "sk-ant-test-123")
    }

    func testSaveRejectsEmptyValue() {
        XCTAssertThrowsError(try APIKeyStore.saveAnthropicApiKey("")) { error in
            guard case APIKeyStoreError.empty = error else {
                return XCTFail("expected .empty, got \(error)")
            }
        }
    }

    func testSaveRejectsWhitespaceOnlyValue() {
        XCTAssertThrowsError(try APIKeyStore.saveAnthropicApiKey("   \n\t")) { error in
            guard case APIKeyStoreError.empty = error else {
                return XCTFail("expected .empty, got \(error)")
            }
        }
    }

    func testSaveOverwritesExistingValue() throws {
        try APIKeyStore.saveAnthropicApiKey("first")
        try APIKeyStore.saveAnthropicApiKey("second")
        XCTAssertEqual(APIKeyStore.readAnthropicApiKey(), "second")
    }

    func testClearRemovesStoredValue() throws {
        try APIKeyStore.saveAnthropicApiKey("sk-ant-test-123")
        try APIKeyStore.clearAnthropicApiKey()
        XCTAssertNil(APIKeyStore.readAnthropicApiKey())
        XCTAssertFalse(FileManager.default.fileExists(atPath: keyPath))
    }

    func testClearIsIdempotentWhenNoFileExists() {
        XCTAssertNoThrow(try APIKeyStore.clearAnthropicApiKey())
    }

    func testSavedFileIsOwnerReadWriteOnly() throws {
        try APIKeyStore.saveAnthropicApiKey("sk-ant-test-123")
        let attrs = try FileManager.default.attributesOfItem(atPath: keyPath)
        let perms = (attrs[.posixPermissions] as? NSNumber)?.intValue
        XCTAssertEqual(perms.map { $0 & 0o777 }, 0o600)
    }
}
