import XCTest
@testable import Boss

/// Coverage for the project-less doc-link icon on investigation cards
/// (the T1705 fix). Investigations have no project, so their doc-link
/// state can't come from `designDocStateByProjectID`; the engine resolves
/// the task's own `doc_*` columns and delivers a `ProjectDesignDocState`
/// directly on `WorkTask.docLinkState`. The card feeds that into the same
/// `ProjectDesignDocAffordancePresentation` design cards use, and taps
/// route to `openTaskDoc` (the task-level analogue of
/// `openProjectDesignDoc`).
@MainActor
final class InvestigationDocLinkAffordanceTests: XCTestCase {
    // MARK: - Presentation (icon renders from the per-task state)

    /// An investigation carrying a resolved `docLinkState` must produce a
    /// non-nil affordance presentation — the same `doc.text` icon a design
    /// card shows. This is the icon the Review-lane card renders.
    func testInvestigationWithResolvedDocLinkStateProducesDocIcon() {
        let task = makeInvestigation(docLinkState: resolvedState(rawContentURL: nil))
        let presentation = ProjectDesignDocAffordancePresentation.from(state: task.docLinkState!)
        XCTAssertEqual(presentation?.systemImage, "doc.text")
        XCTAssertEqual(presentation?.kind, .resolved)
    }

    /// An investigation with no per-task pointer carries `nil` —
    /// the card hides the affordance (parity with `.notSet`).
    func testInvestigationWithoutDocLinkStateHasNoState() {
        let task = makeInvestigation(docLinkState: nil)
        XCTAssertNil(task.docLinkState, "no pointer -> no doc-link state -> hidden affordance")
    }

    // MARK: - openTaskDoc dispatch

    /// A resolved state with a `rawContentURL` (the in-review PR-head-branch
    /// case) opens the async markdown viewer immediately and fetches the
    /// content — never a local file. Mirrors the design path's behaviour.
    func testOpenTaskDocResolvedWithRawContentURLOpensViewer() async {
        let model = makeModel()
        var openedLocalFiles: [URL] = []
        model.urlOpener = { if $0.isFileURL { openedLocalFiles.append($0) } }
        var asyncWindowOpens = 0
        model.asyncMarkdownViewerOpener = { asyncWindowOpens += 1 }
        let fetched = XCTestExpectation(description: "rawContentFetcher called")
        model.rawContentFetcher = { _ in
            fetched.fulfill()
            return "# Investigation"
        }

        let rawURL = "https://raw.githubusercontent.com/spinyfin/mono/docs/investigations/x.md?ref=boss%2Fexec_1"
        let task = makeInvestigation(docLinkState: resolvedState(rawContentURL: rawURL))
        model.openTaskDoc(task)

        XCTAssertEqual(asyncWindowOpens, 1, "viewer window must open immediately on click")
        if case .loading = model.asyncMarkdownViewerVM.state {} else {
            XCTFail("expected .loading immediately after click; got \(model.asyncMarkdownViewerVM.state)")
        }
        await fulfillment(of: [fetched], timeout: 1.0)
        XCTAssertTrue(openedLocalFiles.isEmpty, "must not open a local file when rawContentURL is present")
        XCTAssertNil(model.workErrorMessage)
    }

    /// A resolved state with no `rawContentURL` (non-GitHub repo / older
    /// engine) falls back to opening the GitHub web URL.
    func testOpenTaskDocResolvedWithoutRawContentFallsBackToWebURL() {
        let model = makeModel()
        var openedURLs: [URL] = []
        model.urlOpener = { openedURLs.append($0) }
        let task = makeInvestigation(docLinkState: resolvedState(rawContentURL: nil))
        model.openTaskDoc(task)
        XCTAssertEqual(
            openedURLs.map(\.absoluteString),
            ["https://github.com/spinyfin/mono/blob/main/docs/investigations/x.md"]
        )
        XCTAssertNil(model.workErrorMessage)
    }

    /// A broken pointer surfaces the engine's reason as a work error so the
    /// user can act on it rather than getting a silent no-op.
    func testOpenTaskDocBrokenSurfacesError() {
        let model = makeModel()
        let task = makeInvestigation(docLinkState: .broken(reason: "no repo to resolve against"))
        model.openTaskDoc(task)
        XCTAssertEqual(model.workErrorMessage, "Doc pointer is broken: no repo to resolve against")
    }

    /// A nil / `.notSet` state is a no-op — the affordance should not have
    /// been clickable, but the dispatcher holds the line either way.
    func testOpenTaskDocWithoutStateIsNoOp() {
        let model = makeModel()
        model.openTaskDoc(makeInvestigation(docLinkState: nil))
        model.openTaskDoc(makeInvestigation(docLinkState: .notSet))
        XCTAssertNil(model.workErrorMessage)
    }

    // MARK: - Helpers

    private func resolvedState(rawContentURL: String?) -> ProjectDesignDocState {
        .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "git@github.com:spinyfin/mono.git",
                branch: rawContentURL == nil ? "main" : "boss/exec_1",
                path: "docs/investigations/x.md",
                kind: .sameProduct(productID: "prod_test")
            ),
            workspacePath: nil,
            webURL: "https://github.com/spinyfin/mono/blob/main/docs/investigations/x.md",
            rawContentURL: rawContentURL
        )
    }

    private func makeInvestigation(docLinkState: ProjectDesignDocState?) -> WorkTask {
        WorkTask(
            id: "task_inv",
            productID: "prod_test",
            projectID: nil,
            kind: "investigation",
            name: "Investigate the thing",
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: "https://github.com/spinyfin/mono/pull/1506",
            deletedAt: nil,
            createdAt: "2026-06-14T00:00:00Z",
            updatedAt: "2026-06-14T00:00:00Z",
            docLinkState: docLinkState
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        // Trap the production opener so a missing stub can't pop the browser.
        model.urlOpener = { url in
            XCTFail("urlOpener was invoked with \(url) — install a recording stub first.")
        }
        return model
    }
}
