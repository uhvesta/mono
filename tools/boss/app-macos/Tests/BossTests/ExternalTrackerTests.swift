import XCTest
@testable import Boss

/// Round-trip tests for the external-tracker protocol types. The Swift
/// mirrors must encode to the same wire JSON the Rust serde types produce
/// (snake_case keys, `skip_serializing_if = "Option::is_none"` behaviour).
final class ExternalTrackerCodableTests: XCTestCase {
    private let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return e
    }()
    private let decoder = JSONDecoder()

    private func roundtrip<T: Codable & Equatable>(_ value: T) throws -> T {
        let data = try encoder.encode(value)
        return try decoder.decode(T.self, from: data)
    }

    // MARK: WorkItemExternalRef

    func testWorkItemExternalRefRoundtripsAllFields() throws {
        let wire = """
        {
          "kind": "github",
          "canonical_id": "spinyfin/mono#560",
          "raw": {"issue_number": 560, "project_item_id": "PVTI_abc"},
          "web_url": "https://github.com/spinyfin/mono/issues/560",
          "synced_at": "1747000100",
          "unbound_at": null
        }
        """
        let ref = try decoder.decode(WorkItemExternalRef.self, from: Data(wire.utf8))
        XCTAssertEqual(ref.kind, "github")
        XCTAssertEqual(ref.canonicalID, "spinyfin/mono#560")
        XCTAssertEqual(ref.webURL, "https://github.com/spinyfin/mono/issues/560")
        XCTAssertEqual(ref.syncedAt, "1747000100")
        XCTAssertNil(ref.unboundAt)
        XCTAssertTrue(ref.raw.contains("560"))
    }

    func testWorkItemExternalRefDecodesWithoutOptionalFields() throws {
        let wire = """
        {
          "kind": "github",
          "canonical_id": "spinyfin/mono#560",
          "raw": {},
          "web_url": "https://github.com/spinyfin/mono/issues/560"
        }
        """
        let ref = try decoder.decode(WorkItemExternalRef.self, from: Data(wire.utf8))
        XCTAssertNil(ref.syncedAt)
        XCTAssertNil(ref.unboundAt)
    }

    func testWorkItemExternalRefEncodesWithoutNilOptionals() throws {
        let ref = WorkItemExternalRef(
            kind: "github",
            canonicalID: "spinyfin/mono#560",
            raw: "{}",
            webURL: "https://github.com/spinyfin/mono/issues/560"
        )
        let data = try encoder.encode(ref)
        let json = String(data: data, encoding: .utf8) ?? ""
        XCTAssertFalse(json.contains("synced_at"), "nil synced_at must be omitted")
        XCTAssertFalse(json.contains("unbound_at"), "nil unbound_at must be omitted")
        XCTAssertTrue(json.contains("canonical_id"))
        XCTAssertTrue(json.contains("web_url"))
    }

    func testWorkItemExternalRefRoundtripsWithUnboundAt() throws {
        let ref = WorkItemExternalRef(
            kind: "github",
            canonicalID: "spinyfin/mono#560",
            raw: "{\"n\":1}",
            webURL: "https://github.com/spinyfin/mono/issues/560",
            syncedAt: "1747000100",
            unboundAt: "1747001000"
        )
        let data = try encoder.encode(ref)
        let back = try decoder.decode(WorkItemExternalRef.self, from: data)
        XCTAssertEqual(back.kind, ref.kind)
        XCTAssertEqual(back.canonicalID, ref.canonicalID)
        XCTAssertEqual(back.webURL, ref.webURL)
        XCTAssertEqual(back.syncedAt, ref.syncedAt)
        XCTAssertEqual(back.unboundAt, ref.unboundAt)
    }

    // MARK: SetProductExternalTrackerInput

    func testSetProductExternalTrackerInputRoundtrips() throws {
        let input = SetProductExternalTrackerInput(
            productID: "prod_1",
            kind: "github",
            config: "{\"org\":\"spinyfin\",\"repo\":\"mono\",\"project_number\":1}",
            unset: false
        )
        let data = try encoder.encode(input)
        let json = String(data: data, encoding: .utf8) ?? ""
        XCTAssertTrue(json.contains("\"product_id\":\"prod_1\""))
        XCTAssertTrue(json.contains("\"kind\":\"github\""))
        XCTAssertTrue(json.contains("\"config\""))
        XCTAssertTrue(json.contains("\"unset\":false"))

        let back = try decoder.decode(SetProductExternalTrackerInput.self, from: data)
        XCTAssertEqual(back.productID, "prod_1")
        XCTAssertEqual(back.kind, "github")
        XCTAssertFalse(back.unset)
        XCTAssertNotNil(back.config)
    }

    func testSetProductExternalTrackerInputUnsetOmitsKindAndConfig() throws {
        let input = SetProductExternalTrackerInput(
            productID: "prod_1",
            kind: nil,
            config: nil,
            unset: true
        )
        let data = try encoder.encode(input)
        let json = String(data: data, encoding: .utf8) ?? ""
        XCTAssertFalse(json.contains("\"kind\""), "nil kind must be omitted")
        XCTAssertFalse(json.contains("\"config\""), "nil config must be omitted")
        XCTAssertTrue(json.contains("\"unset\":true"))
    }

    func testSetProductExternalTrackerInputDecodesFromRustWire() throws {
        let wire = """
        {
          "product_id": "prod_1",
          "kind": "github",
          "config": {"org": "spinyfin", "repo": "mono", "project_number": 1},
          "unset": false
        }
        """
        let input = try decoder.decode(SetProductExternalTrackerInput.self, from: Data(wire.utf8))
        XCTAssertEqual(input.productID, "prod_1")
        XCTAssertEqual(input.kind, "github")
        XCTAssertFalse(input.unset)
        XCTAssertNotNil(input.config)
        XCTAssertTrue(input.config?.contains("spinyfin") ?? false)
    }

    // MARK: LinkExternalRefInput

    func testLinkExternalRefInputRoundtrips() throws {
        let input = LinkExternalRefInput(
            workItemID: "task_1",
            kind: "github",
            canonicalID: "spinyfin/mono#560"
        )
        let data = try encoder.encode(input)
        let json = String(data: data, encoding: .utf8) ?? ""
        XCTAssertTrue(json.contains("\"work_item_id\":\"task_1\""))
        XCTAssertTrue(json.contains("\"kind\":\"github\""))
        XCTAssertTrue(json.contains("\"canonical_id\":\"spinyfin/mono#560\""))

        let back = try decoder.decode(LinkExternalRefInput.self, from: data)
        XCTAssertEqual(back, input)
    }

    func testLinkExternalRefInputDecodesFromRustWire() throws {
        let wire = """
        {
          "work_item_id": "task_1",
          "kind": "github",
          "canonical_id": "spinyfin/mono#560"
        }
        """
        let input = try decoder.decode(LinkExternalRefInput.self, from: Data(wire.utf8))
        XCTAssertEqual(input.workItemID, "task_1")
        XCTAssertEqual(input.kind, "github")
        XCTAssertEqual(input.canonicalID, "spinyfin/mono#560")
    }
}
