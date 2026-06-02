import XCTest
@testable import Boss

/// Covers the Review column ordering rule (boss issue #1250): cards in the
/// Review column are ordered by creation time, newest at the top, with name
/// then id as deterministic tiebreakers. The generic board sort keys on
/// `ordinal` (which review-phase tasks rarely carry), so Review needs its own
/// `reviewBoardSort` to avoid an apparently-random order.
@MainActor
final class ReviewColumnOrderingTests: XCTestCase {

    func testReviewColumnSortsNewestFirst() {
        let model = makeModel()
        let older = makeReviewItem(id: "task_old", name: "Older", createdAt: "2026-05-01T00:00:00Z")
        let newest = makeReviewItem(id: "task_new", name: "Newest", createdAt: "2026-05-30T00:00:00Z")
        let middle = makeReviewItem(id: "task_mid", name: "Middle", createdAt: "2026-05-15T00:00:00Z")
        // Insert in non-sorted order to prove the sort, not insertion order.
        model.choresByProductID = ["prod_test": [older, newest, middle]]

        let ids = model.workItems(in: .review).map(\.id)
        XCTAssertEqual(ids, ["task_new", "task_mid", "task_old"],
                       "Review column should be ordered newest-first by createdAt")
    }

    func testReviewColumnTieBreaksByNameThenID() {
        let model = makeModel()
        let sameTime = "2026-05-10T00:00:00Z"
        let bravo = makeReviewItem(id: "task_z", name: "Bravo", createdAt: sameTime)
        let alphaZ = makeReviewItem(id: "task_z2", name: "Alpha", createdAt: sameTime)
        let alphaA = makeReviewItem(id: "task_a", name: "Alpha", createdAt: sameTime)
        model.choresByProductID = ["prod_test": [bravo, alphaZ, alphaA]]

        let ids = model.workItems(in: .review).map(\.id)
        // Equal createdAt → name ascending ("Alpha" before "Bravo"), then id
        // ascending among the two "Alpha" cards ("task_a" before "task_z2").
        XCTAssertEqual(ids, ["task_a", "task_z2", "task_z"],
                       "Ties should break by name then id for a deterministic order")
    }

    // MARK: - Helpers

    private func makeReviewItem(id: String, name: String, createdAt: String) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: name,
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: createdAt,
            updatedAt: createdAt,
            autostart: true
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [
            WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-13T00:00:00Z",
                updatedAt: "2026-05-13T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
