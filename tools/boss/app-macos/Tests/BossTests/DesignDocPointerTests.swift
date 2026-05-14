import XCTest
@testable import Boss

/// Round-trip tests for the design-doc-pointer protocol types. The
/// Swift mirrors must encode to the same wire JSON the Rust serde
/// types produce — internally-tagged enums (`{"type": "...", ...}`)
/// and snake_case keys — so engine ↔ app payloads stay in sync.
final class DesignDocPointerCodableTests: XCTestCase {
    private let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = [.sortedKeys]
        return e
    }()
    private let decoder = JSONDecoder()

    private func roundtrip<T: Codable & Equatable>(_ value: T) throws -> T {
        let data = try encoder.encode(value)
        return try decoder.decode(T.self, from: data)
    }

    func testSetProjectDesignDocInputRoundtrip() throws {
        let input = SetProjectDesignDocInput(
            projectID: "proj_1",
            designDocRepoRemoteURL: "https://github.com/foo/bar.git",
            designDocBranch: "main",
            designDocPath: "tools/boss/docs/designs/foo.md",
            unset: false
        )
        let back = try roundtrip(input)
        XCTAssertEqual(back, input)
    }

    func testSetProjectDesignDocInputUnsetClears() throws {
        let input = SetProjectDesignDocInput(
            projectID: "proj_1",
            designDocRepoRemoteURL: nil,
            designDocBranch: nil,
            designDocPath: nil,
            unset: true
        )
        let data = try encoder.encode(input)
        let json = String(data: data, encoding: .utf8) ?? ""
        XCTAssertTrue(json.contains("\"project_id\":\"proj_1\""))
        XCTAssertTrue(json.contains("\"unset\":true"))
    }

    func testResolvedDesignDocKindSameProductRoundtrip() throws {
        let kind = ResolvedDesignDocKind.sameProduct(productID: "prod_1")
        let data = try encoder.encode(kind)
        let json = String(data: data, encoding: .utf8) ?? ""
        XCTAssertEqual(json, "{\"product_id\":\"prod_1\",\"type\":\"same_product\"}")
        let back = try roundtrip(kind)
        XCTAssertEqual(back, kind)
    }

    func testResolvedDesignDocKindOtherProductRoundtrip() throws {
        let back = try roundtrip(ResolvedDesignDocKind.otherProduct(productID: "prod_2"))
        XCTAssertEqual(back, .otherProduct(productID: "prod_2"))
    }

    func testResolvedDesignDocKindExternalRoundtrip() throws {
        let kind = ResolvedDesignDocKind.external
        let data = try encoder.encode(kind)
        let json = String(data: data, encoding: .utf8) ?? ""
        XCTAssertEqual(json, "{\"type\":\"external\"}")
        let back = try roundtrip(kind)
        XCTAssertEqual(back, .external)
    }

    func testResolvedDesignDocDecodesFromRustWire() throws {
        let wire = """
        {
          "repo_remote_url": "https://github.com/foo/bar.git",
          "branch": "main",
          "path": "docs/x.md",
          "kind": {"type": "same_product", "product_id": "prod_1"}
        }
        """
        let resolved = try decoder.decode(ResolvedDesignDoc.self, from: Data(wire.utf8))
        XCTAssertEqual(resolved.repoRemoteURL, "https://github.com/foo/bar.git")
        XCTAssertEqual(resolved.branch, "main")
        XCTAssertEqual(resolved.path, "docs/x.md")
        XCTAssertEqual(resolved.kind, .sameProduct(productID: "prod_1"))
    }

    func testProjectDesignDocStateNotSetRoundtrip() throws {
        let state = ProjectDesignDocState.notSet
        let data = try encoder.encode(state)
        let json = String(data: data, encoding: .utf8) ?? ""
        XCTAssertEqual(json, "{\"type\":\"not_set\"}")
        XCTAssertEqual(try roundtrip(state), .notSet)
    }

    func testProjectDesignDocStateResolvedRoundtrip() throws {
        let state = ProjectDesignDocState.resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "docs/x.md",
                kind: .sameProduct(productID: "prod_1")
            ),
            workspacePath: "/Users/me/Documents/dev/workspaces/mono-agent-001",
            webURL: "https://github.com/foo/bar/blob/main/docs/x.md",
            rawContentURL: "https://raw.githubusercontent.com/foo/bar/main/docs/x.md"
        )
        let back = try roundtrip(state)
        XCTAssertEqual(back, state)
    }

    /// The engine omits `workspace_path` from the wire JSON when no
    /// workspace is leased for the resolved repo (serde's
    /// `skip_serializing_if = "Option::is_none"`). The Swift decoder
    /// must accept that shape too — otherwise every project without a
    /// leased workspace would 422 on the kanban side.
    func testProjectDesignDocStateResolvedDecodesWithoutWorkspacePath() throws {
        let wire = """
        {
          "type": "resolved",
          "resolved": {
            "repo_remote_url": "https://github.com/foo/bar.git",
            "branch": "main",
            "path": "docs/x.md",
            "kind": {"type": "external"}
          },
          "web_url": "https://github.com/foo/bar/blob/main/docs/x.md"
        }
        """
        let state = try decoder.decode(ProjectDesignDocState.self, from: Data(wire.utf8))
        if case let .resolved(_, workspacePath, _, _) = state {
            XCTAssertNil(workspacePath)
        } else {
            XCTFail("expected .resolved, got \(state)")
        }
    }

    func testProjectDesignDocStateBrokenRoundtrip() throws {
        let state = ProjectDesignDocState.broken(reason: "no repo to resolve against")
        let back = try roundtrip(state)
        XCTAssertEqual(back, state)
    }

    func testResolveProjectDesignDocOutputRoundtrip() throws {
        let output = ResolveProjectDesignDocOutput(
            projectID: "proj_1",
            state: .resolved(
                resolved: ResolvedDesignDoc(
                    repoRemoteURL: "https://github.com/foo/bar.git",
                    branch: "main",
                    path: "docs/x.md",
                    kind: .external
                ),
                workspacePath: nil,
                webURL: "https://github.com/foo/bar/blob/main/docs/x.md",
                rawContentURL: "https://raw.githubusercontent.com/foo/bar/main/docs/x.md"
            )
        )
        let back = try roundtrip(output)
        XCTAssertEqual(back, output)
    }

    /// Engine sends `resolved` state without `raw_content_url` (older
    /// engine or non-GitHub repo). The Swift decoder must accept the
    /// shape and produce `rawContentURL == nil`.
    func testProjectDesignDocStateResolvedDecodesWithoutRawContentURL() throws {
        let wire = """
        {
          "type": "resolved",
          "resolved": {
            "repo_remote_url": "https://github.com/foo/bar.git",
            "branch": "main",
            "path": "docs/x.md",
            "kind": {"type": "external"}
          },
          "web_url": "https://github.com/foo/bar/blob/main/docs/x.md"
        }
        """
        let state = try decoder.decode(ProjectDesignDocState.self, from: Data(wire.utf8))
        if case let .resolved(_, _, _, rawContentURL) = state {
            XCTAssertNil(rawContentURL)
        } else {
            XCTFail("expected .resolved, got \(state)")
        }
    }

    /// Engine sends `resolved` state with `raw_content_url` (GitHub
    /// repo, branch set). The Swift decoder must surface the URL.
    func testProjectDesignDocStateResolvedDecodesRawContentURL() throws {
        let wire = """
        {
          "type": "resolved",
          "resolved": {
            "repo_remote_url": "https://github.com/foo/bar.git",
            "branch": "boss/design-pr",
            "path": "tools/boss/docs/designs/foo.md",
            "kind": {"type": "same_product", "product_id": "prod_1"}
          },
          "web_url": "https://github.com/foo/bar/blob/boss/design-pr/tools/boss/docs/designs/foo.md",
          "raw_content_url": "https://raw.githubusercontent.com/foo/bar/boss/design-pr/tools/boss/docs/designs/foo.md"
        }
        """
        let state = try decoder.decode(ProjectDesignDocState.self, from: Data(wire.utf8))
        if case let .resolved(_, _, _, rawContentURL) = state {
            XCTAssertEqual(
                rawContentURL,
                "https://raw.githubusercontent.com/foo/bar/boss/design-pr/tools/boss/docs/designs/foo.md"
            )
        } else {
            XCTFail("expected .resolved, got \(state)")
        }
    }
}
