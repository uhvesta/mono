import XCTest
@testable import Boss

/// Tests for `ChatViewModel.openQuestionGroupsForDocPath` — the filter used by
/// `DesignRendererView` to find open question groups matching the on-screen doc.
///
/// No host app required.
@MainActor
final class OpenQuestionGroupsForDocPathTests: XCTestCase {

    private func makeGroup(
        id: String,
        kind: String = "question",
        state: String = "open",
        sourceDocPath: String? = nil,
        productID: String = "prod_1"
    ) -> AttentionGroup {
        AttentionGroup(
            id: id,
            productID: productID,
            shortID: nil,
            kind: kind,
            associationProjectID: "proj_1",
            associationTaskID: nil,
            sourceKind: "design_doc",
            sourceTaskID: nil,
            sourceRunID: nil,
            sourceDocPath: sourceDocPath,
            sourceDocRepoRemoteURL: nil,
            sourceDocBranch: nil,
            groupingKey: "k_\(id)",
            generation: 0,
            state: state,
            producedArtifactKind: nil,
            producedArtifactRef: nil,
            createdAt: "2026-06-01T00:00:00Z",
            actionedAt: nil,
            dismissedAt: nil
        )
    }

    private func makeModel(groups: [AttentionGroup], productID: String = "prod_1") -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.attentionGroupsByProductID[productID] = groups
        return model
    }

    func testMatchesAbsolutePathWithRepoRelativeSuffix() {
        let docPath = "tools/boss/docs/designs/attentions.md"
        let group = makeGroup(id: "atg_1", sourceDocPath: docPath)
        let model = makeModel(groups: [group])

        let absolutePath = "/Users/alice/workspaces/mono-agent-001/\(docPath)"
        let result = model.openQuestionGroupsForDocPath(absolutePath)

        XCTAssertEqual(result.map(\.id), ["atg_1"])
    }

    func testExactPathMatchAlsoWorks() {
        let docPath = "tools/boss/docs/designs/attentions.md"
        let group = makeGroup(id: "atg_1", sourceDocPath: docPath)
        let model = makeModel(groups: [group])

        let result = model.openQuestionGroupsForDocPath(docPath)

        XCTAssertEqual(result.map(\.id), ["atg_1"])
    }

    func testNonMatchingPathReturnsEmpty() {
        let group = makeGroup(id: "atg_1", sourceDocPath: "tools/boss/docs/designs/foo.md")
        let model = makeModel(groups: [group])

        let result = model.openQuestionGroupsForDocPath(
            "/workspaces/mono/tools/boss/docs/designs/bar.md"
        )

        XCTAssertTrue(result.isEmpty)
    }

    func testPartialSuffixDoesNotMatchMidName() {
        // "attentions.md" must be preceded by "/" to not match a different
        // file like "meta-attentions.md".
        let group = makeGroup(id: "atg_1", sourceDocPath: "attentions.md")
        let model = makeModel(groups: [group])

        let result = model.openQuestionGroupsForDocPath(
            "/workspaces/mono/tools/boss/docs/designs/meta-attentions.md"
        )

        XCTAssertTrue(result.isEmpty,
            "a bare suffix match should not fire when the separator isn't present")
    }

    func testOnlyOpenGroupsAreReturned() {
        let docPath = "tools/boss/docs/designs/attentions.md"
        let openGroup = makeGroup(id: "atg_open", state: "open", sourceDocPath: docPath)
        let partialGroup = makeGroup(id: "atg_partial", state: "partially_answered", sourceDocPath: docPath)
        let actionedGroup = makeGroup(id: "atg_actioned", state: "actioned", sourceDocPath: docPath)
        let dismissedGroup = makeGroup(id: "atg_dismissed", state: "dismissed", sourceDocPath: docPath)
        let model = makeModel(groups: [openGroup, partialGroup, actionedGroup, dismissedGroup])

        let absolutePath = "/workspaces/mono/\(docPath)"
        let result = model.openQuestionGroupsForDocPath(absolutePath)

        XCTAssertEqual(Set(result.map(\.id)), ["atg_open", "atg_partial"])
    }

    func testOnlyQuestionGroupsAreReturned() {
        let docPath = "tools/boss/docs/designs/attentions.md"
        let questionGroup = makeGroup(id: "atg_q", kind: "question", sourceDocPath: docPath)
        let followupGroup = makeGroup(id: "atg_f", kind: "followup", sourceDocPath: docPath)
        let model = makeModel(groups: [questionGroup, followupGroup])

        let result = model.openQuestionGroupsForDocPath("/workspaces/mono/\(docPath)")

        XCTAssertEqual(result.map(\.id), ["atg_q"])
    }

    func testGroupWithNilSourceDocPathIsSkipped() {
        let group = makeGroup(id: "atg_1", sourceDocPath: nil)
        let model = makeModel(groups: [group])

        let result = model.openQuestionGroupsForDocPath(
            "/workspaces/mono/tools/boss/docs/designs/attentions.md"
        )

        XCTAssertTrue(result.isEmpty)
    }

    func testSearchesAcrossAllProducts() {
        let docPath = "tools/boss/docs/designs/attentions.md"
        let group1 = makeGroup(id: "atg_prod1", sourceDocPath: docPath, productID: "prod_1")
        let group2 = makeGroup(id: "atg_prod2", sourceDocPath: docPath, productID: "prod_2")
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.attentionGroupsByProductID["prod_1"] = [group1]
        model.attentionGroupsByProductID["prod_2"] = [group2]

        let result = model.openQuestionGroupsForDocPath("/workspaces/mono/\(docPath)")

        XCTAssertEqual(Set(result.map(\.id)), ["atg_prod1", "atg_prod2"])
    }
}
