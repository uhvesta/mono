import XCTest
@testable import Boss

/// Covers the ChatViewModel dispatch arm that turns a raw
/// `engine_health_result` payload into the `engineHealthIssues` /
/// `engineAnthropicApiKeyPresent` state that the top-of-window
/// `EngineHealthBanner` and the Settings-pane warning bind to.
/// Introduced after #699 where a missing `ANTHROPIC_API_KEY` silently
/// broke summarization with no UI affordance — the banner is the UI
/// affordance, so its source of truth must be tested.
@MainActor
final class EngineHealthBannerTests: XCTestCase {

    /// The healthy case: engine reports the key is present with no
    /// issues. The banner-driving array must end up empty and the
    /// presence bit must flip to `true`.
    func testHealthyEngineLeavesIssueListEmpty() {
        let model = makeModel()

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: []
        ))

        XCTAssertTrue(model.engineAnthropicApiKeyPresent)
        XCTAssertTrue(model.engineHealthIssues.isEmpty)
    }

    /// The chore's headline case: engine reports the key is missing
    /// and returns the `missing_anthropic_api_key` issue. The banner's
    /// source of truth must be populated so the chrome strip renders.
    func testMissingApiKeySurfacesIssueAndClearsPresenceBit() {
        let model = makeModel()
        let issue = EngineHealthIssue(
            kind: "missing_anthropic_api_key",
            severity: "warning",
            title: "ANTHROPIC_API_KEY is not set",
            body: "Live worker summaries are disabled. Set the env var and relaunch Boss."
        )

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: false,
            issues: [issue]
        ))

        XCTAssertFalse(model.engineAnthropicApiKeyPresent)
        XCTAssertEqual(model.engineHealthIssues, [issue])
    }

    /// A subsequent healthy report must clear a previously-surfaced
    /// issue. Otherwise the banner would stick around after the user
    /// restarted Boss with the env var set — exactly the affordance
    /// the chore wants to be reactive.
    func testHealthyReportClearsPreviouslySurfacedIssue() {
        let model = makeModel()
        let issue = EngineHealthIssue(
            kind: "missing_anthropic_api_key",
            severity: "warning",
            title: "ANTHROPIC_API_KEY is not set",
            body: "..."
        )
        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: false,
            issues: [issue]
        ))
        XCTAssertFalse(model.engineHealthIssues.isEmpty)

        model.applyEventForTest(.engineHealthResult(
            apiKeyPresent: true,
            issues: []
        ))

        XCTAssertTrue(model.engineAnthropicApiKeyPresent)
        XCTAssertTrue(model.engineHealthIssues.isEmpty)
    }

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
    }
}
