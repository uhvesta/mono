import XCTest
@testable import Boss

/// Tests for `ChatViewModel.applyAttentionGroupsList` — the function that
/// replaces a product's group set from a full `list_attention_groups` reply.
///
/// The function buckets members by `groupID` (content-based, not positional),
/// so the order groups arrive in the response must not affect the result.
/// These tests lock in that behaviour and cover the empty-response fast path.
///
/// No host app required.
@MainActor
final class ApplyAttentionGroupsListTests: XCTestCase {

    // MARK: - Fixtures

    private func makeGroup(id: String, state: String, productID: String = "prod_1") -> AttentionGroup {
        AttentionGroup(
            id: id,
            productID: productID,
            shortID: nil,
            kind: "question",
            associationProjectID: nil,
            associationTaskID: "tsk_1",
            sourceKind: "task_transcript",
            sourceTaskID: nil,
            sourceRunID: nil,
            sourceDocPath: nil,
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

    private func makeMember(id: String, groupID: String, ordinal: Int) -> Attention {
        Attention(
            id: id,
            groupID: groupID,
            ordinal: ordinal,
            sourceAnchor: nil,
            answerState: "open",
            createdAt: "2026-06-01T00:00:00Z",
            answeredAt: nil,
            questionType: "yes_no",
            promptText: "Question \(id)?",
            choiceOptions: nil,
            answer: nil,
            proposedName: nil,
            proposedDescription: nil,
            proposedEffort: nil,
            proposedWorkKind: nil,
            rationale: nil,
            confidenceSource: "structured"
        )
    }

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }

    // MARK: - Empty response

    func testEmptyResponseClearsExistingGroupsForProduct() {
        let model = makeModel()
        let prior = makeGroup(id: "atg_prior", state: "open")
        let priorMember = makeMember(id: "atn_1", groupID: "atg_prior", ordinal: 1)
        model.applyAttentionGroupsList(productID: "prod_1", groups: [prior], members: [priorMember])
        XCTAssertEqual(model.attentionGroupsByProductID["prod_1"]?.count, 1)

        model.applyAttentionGroupsList(productID: "prod_1", groups: [], members: [])

        XCTAssertTrue(model.attentionGroupsByProductID["prod_1"]?.isEmpty ?? true)
        XCTAssertNil(model.attentionMembersByGroupID["atg_prior"], "stale member entry should be pruned")
    }

    func testEmptyResponseLeavesOtherProductsUntouched() {
        let model = makeModel()
        let other = makeGroup(id: "atg_other", state: "open", productID: "prod_2")
        model.applyAttentionGroupsList(productID: "prod_2", groups: [other], members: [])

        model.applyAttentionGroupsList(productID: "prod_1", groups: [], members: [])

        XCTAssertEqual(model.attentionGroupsByProductID["prod_2"]?.count, 1)
    }

    // MARK: - Open-first then dismissed ordering

    func testOpenFirstThenDismissedBucketsMembersCorrectly() {
        let model = makeModel()
        let openGroup = makeGroup(id: "atg_open", state: "open")
        let dismissedGroup = makeGroup(id: "atg_dismissed", state: "dismissed")

        let openMember1 = makeMember(id: "atn_o1", groupID: "atg_open", ordinal: 1)
        let openMember2 = makeMember(id: "atn_o2", groupID: "atg_open", ordinal: 2)
        let dismissedMember = makeMember(id: "atn_d1", groupID: "atg_dismissed", ordinal: 1)

        // open group comes first in the groups array
        model.applyAttentionGroupsList(
            productID: "prod_1",
            groups: [openGroup, dismissedGroup],
            members: [openMember1, openMember2, dismissedMember]
        )

        let openMembers = model.attentionMembersByGroupID["atg_open"] ?? []
        let dismissedMembers = model.attentionMembersByGroupID["atg_dismissed"] ?? []

        XCTAssertEqual(openMembers.map(\.id), ["atn_o1", "atn_o2"])
        XCTAssertEqual(dismissedMembers.map(\.id), ["atn_d1"])
        XCTAssertEqual(model.attentionGroupsByProductID["prod_1"]?.map(\.id), ["atg_open", "atg_dismissed"])
    }

    // MARK: - Dismissed-first then open ordering

    func testDismissedFirstThenOpenProducesSameResult() {
        let model = makeModel()
        let openGroup = makeGroup(id: "atg_open", state: "open")
        let dismissedGroup = makeGroup(id: "atg_dismissed", state: "dismissed")

        let openMember1 = makeMember(id: "atn_o1", groupID: "atg_open", ordinal: 1)
        let openMember2 = makeMember(id: "atn_o2", groupID: "atg_open", ordinal: 2)
        let dismissedMember = makeMember(id: "atn_d1", groupID: "atg_dismissed", ordinal: 1)

        // dismissed group comes first in the groups array
        model.applyAttentionGroupsList(
            productID: "prod_1",
            groups: [dismissedGroup, openGroup],
            members: [openMember1, openMember2, dismissedMember]
        )

        let openMembers = model.attentionMembersByGroupID["atg_open"] ?? []
        let dismissedMembers = model.attentionMembersByGroupID["atg_dismissed"] ?? []

        XCTAssertEqual(openMembers.map(\.id), ["atn_o1", "atn_o2"],
            "open group members should be correct regardless of group arrival order")
        XCTAssertEqual(dismissedMembers.map(\.id), ["atn_d1"],
            "dismissed group members should be correct regardless of group arrival order")
    }

    func testMembersArrivedOutOfOrdinalOrderAreSorted() {
        let model = makeModel()
        let group = makeGroup(id: "atg_1", state: "open")

        // members arrive with ordinal 3, 1, 2 — out of order
        let m3 = makeMember(id: "atn_3", groupID: "atg_1", ordinal: 3)
        let m1 = makeMember(id: "atn_1", groupID: "atg_1", ordinal: 1)
        let m2 = makeMember(id: "atn_2", groupID: "atg_1", ordinal: 2)

        model.applyAttentionGroupsList(productID: "prod_1", groups: [group], members: [m3, m1, m2])

        let members = model.attentionMembersByGroupID["atg_1"] ?? []
        XCTAssertEqual(members.map(\.id), ["atn_1", "atn_2", "atn_3"],
            "members should be sorted by ordinal regardless of arrival order")
    }

    // MARK: - Stale group pruning

    func testGroupsRemovedFromResponseHaveTheirMembersPruned() {
        let model = makeModel()
        let groupA = makeGroup(id: "atg_a", state: "open")
        let groupB = makeGroup(id: "atg_b", state: "open")
        let memberA = makeMember(id: "atn_a1", groupID: "atg_a", ordinal: 1)
        let memberB = makeMember(id: "atn_b1", groupID: "atg_b", ordinal: 1)

        model.applyAttentionGroupsList(
            productID: "prod_1",
            groups: [groupA, groupB],
            members: [memberA, memberB]
        )
        XCTAssertNotNil(model.attentionMembersByGroupID["atg_b"])

        // second response omits atg_b
        model.applyAttentionGroupsList(
            productID: "prod_1",
            groups: [groupA],
            members: [memberA]
        )

        XCTAssertNil(model.attentionMembersByGroupID["atg_b"],
            "members for a group absent from the new response should be pruned")
        XCTAssertEqual(model.attentionMembersByGroupID["atg_a"]?.map(\.id), ["atn_a1"])
    }
}
