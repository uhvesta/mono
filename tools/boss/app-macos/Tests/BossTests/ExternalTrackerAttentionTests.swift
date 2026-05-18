import XCTest
@testable import Boss

/// Tests for the external-tracker attention-item presentation model (chore 16
/// of `external-issue-tracker-sync-github-projects.md`). Covers each of the
/// four reason codes the engine reconciler emits, plus filtering and open/resolved
/// state.
///
/// These are pure model tests — no host app required.
final class ExternalTrackerAttentionTests: XCTestCase {

    // MARK: - Fixtures

    private func makeItem(
        kind: String,
        status: String = "open",
        title: String = "Test attention",
        body: String = "Test body"
    ) -> WorkAttentionItem {
        WorkAttentionItem(
            id: "attn_test",
            executionID: nil,
            workItemID: "prod_test",
            kind: kind,
            status: status,
            title: title,
            bodyMarkdown: body,
            createdAt: "2026-05-18T00:00:00Z",
            resolvedAt: nil
        )
    }

    // MARK: - forItem factory

    func testForItemReturnsNilForUnrecognisedKind() {
        let item = makeItem(kind: "cube_workspace_lease_failed")
        XCTAssertNil(ExternalTrackerAttentionPresentation.forItem(item))
    }

    func testForItemReturnsNilForNonExternalTrackerKind() {
        let item = makeItem(kind: "repo_unresolved")
        XCTAssertNil(ExternalTrackerAttentionPresentation.forItem(item))
    }

    func testForItemReturnsNonNilForKnownKind() {
        let item = makeItem(kind: "external_tracker_auth_failed")
        XCTAssertNotNil(ExternalTrackerAttentionPresentation.forItem(item))
    }

    // MARK: - Reason 1: auth_failed

    func testAuthFailedReasonCode() {
        let item = makeItem(kind: "external_tracker_auth_failed")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertEqual(p?.reasonCode, "auth_failed")
    }

    func testAuthFailedDisplayTitleMatchesItemTitle() {
        let item = makeItem(
            kind: "external_tracker_auth_failed",
            title: "External tracker auth failed for product prod_abc"
        )
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertEqual(p?.displayTitle, "External tracker auth failed for product prod_abc")
    }

    func testAuthFailedRemediationHintMentionsGhAuthLogin() {
        let item = makeItem(kind: "external_tracker_auth_failed")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertTrue(
            p?.remediationHint.contains("gh auth login") ?? false,
            "remediation hint should mention `gh auth login`; got: \(p?.remediationHint ?? "(nil)")"
        )
    }

    func testAuthFailedIsOpenWhenStatusOpen() {
        let item = makeItem(kind: "external_tracker_auth_failed", status: "open")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertEqual(p?.isOpen, true)
    }

    func testAuthFailedIsClosedWhenStatusResolved() {
        let item = makeItem(kind: "external_tracker_auth_failed", status: "resolved")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertEqual(p?.isOpen, false)
    }

    // MARK: - Reason 2: transient_errors

    func testTransientErrorsReasonCode() {
        let item = makeItem(kind: "external_tracker_transient_errors")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertEqual(p?.reasonCode, "transient_errors")
    }

    func testTransientErrorsRemediationHintMentionsRetry() {
        let item = makeItem(kind: "external_tracker_transient_errors")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertTrue(
            p?.remediationHint.lowercased().contains("retry") ?? false,
            "remediation hint should mention retry; got: \(p?.remediationHint ?? "(nil)")"
        )
    }

    func testTransientErrorsIconName() {
        let item = makeItem(kind: "external_tracker_transient_errors")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertFalse(p?.iconName.isEmpty ?? true, "iconName should not be empty")
    }

    // MARK: - Reason 3: removed_upstream

    func testRemovedUpstreamReasonCode() {
        let item = makeItem(kind: "external_tracker_removed_upstream")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertEqual(p?.reasonCode, "removed_upstream")
    }

    func testRemovedUpstreamRemediationHintMentionsLinkExternal() {
        let item = makeItem(kind: "external_tracker_removed_upstream")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertTrue(
            p?.remediationHint.contains("link-external") ?? false,
            "remediation hint should mention link-external command; got: \(p?.remediationHint ?? "(nil)")"
        )
    }

    func testRemovedUpstreamDisplayTitleMatchesItemTitle() {
        let item = makeItem(
            kind: "external_tracker_removed_upstream",
            title: "Upstream binding for spinyfin/mono#560 cleared"
        )
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertEqual(p?.displayTitle, "Upstream binding for spinyfin/mono#560 cleared")
    }

    // MARK: - Reason 4: permission_denied

    func testPermissionDeniedReasonCode() {
        let item = makeItem(kind: "external_tracker_permission_denied")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertEqual(p?.reasonCode, "permission_denied")
    }

    func testPermissionDeniedRemediationHintMentionsScopeRepo() {
        let item = makeItem(kind: "external_tracker_permission_denied")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertTrue(
            p?.remediationHint.contains("--scopes repo") ?? false,
            "remediation hint should mention `--scopes repo`; got: \(p?.remediationHint ?? "(nil)")"
        )
    }

    func testPermissionDeniedIsOpenWhenStatusOpen() {
        let item = makeItem(kind: "external_tracker_permission_denied", status: "open")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertEqual(p?.isOpen, true)
    }

    // MARK: - Unknown reason code degrades gracefully

    func testUnknownReasonCodeReturnsPresentation() {
        let item = makeItem(kind: "external_tracker_future_reason")
        let p = ExternalTrackerAttentionPresentation.forItem(item)
        XCTAssertNotNil(p, "unknown external_tracker_ kinds should still produce a presentation")
        XCTAssertEqual(p?.reasonCode, "future_reason")
    }

    // MARK: - Codable round-trip for WorkAttentionItem

    func testWorkAttentionItemDecodesFromWire() throws {
        let wire = """
        {
          "id": "attn_abc123",
          "work_item_id": "prod_xyz",
          "kind": "external_tracker_auth_failed",
          "status": "open",
          "title": "Auth failed",
          "body_markdown": "Run gh auth login.",
          "created_at": "2026-05-18T10:00:00Z"
        }
        """
        let decoder = JSONDecoder()
        let item = try decoder.decode(WorkAttentionItem.self, from: Data(wire.utf8))
        XCTAssertEqual(item.id, "attn_abc123")
        XCTAssertEqual(item.workItemID, "prod_xyz")
        XCTAssertEqual(item.kind, "external_tracker_auth_failed")
        XCTAssertEqual(item.status, "open")
        XCTAssertNil(item.executionID)
        XCTAssertNil(item.resolvedAt)
    }

    func testWorkAttentionItemDecodesWithExecutionID() throws {
        let wire = """
        {
          "id": "attn_exec1",
          "execution_id": "exec_abc",
          "kind": "pane_spawn_failed",
          "status": "open",
          "title": "Pane spawn failed",
          "body_markdown": "See logs.",
          "created_at": "2026-05-18T10:00:00Z"
        }
        """
        let decoder = JSONDecoder()
        let item = try decoder.decode(WorkAttentionItem.self, from: Data(wire.utf8))
        XCTAssertEqual(item.executionID, "exec_abc")
        XCTAssertNil(item.workItemID)
    }
}
