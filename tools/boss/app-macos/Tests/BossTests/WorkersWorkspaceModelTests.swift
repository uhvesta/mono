import XCTest
@testable import Boss

@MainActor
final class WorkersWorkspaceModelSendTests: XCTestCase {
    func testSendToUnknownSlotReturnsUnknownSlot() {
        // Mirrors `focusWorkerPane` / `interruptWorkerPane`: a
        // `SendToPane` for a slot that the workers grid does not host
        // must surface `.unknownSlot` so the engine can decide whether
        // to requeue (probe injection) or surface a `WorkError` (the
        // `agents send` CLI path). Silently no-op'ing here was the
        // shape of the original intervene bug — a missing slot looked
        // like a successful injection, the engine moved on, and the
        // prompt was lost.
        let model = WorkersWorkspaceModel()
        let result = model.sendToPane(slotId: 99, text: "echo hello")
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for nonexistent slot, got \(result)")
            return
        }
    }

    func testSendToIdleSlotReturnsUnknownSlot() {
        // An allocated slot with no session attached is the same
        // class of failure as a nonexistent slot — the app has no
        // surface to write to. Matches the equivalent
        // `focusWorkerPane` test so the engine's failure-handling
        // path stays uniform across the three pane verbs.
        let model = WorkersWorkspaceModel()
        let result = model.sendToPane(slotId: 1, text: "echo hello")
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for idle slot, got \(result)")
            return
        }
    }
}

@MainActor
final class GhosttyTerminalHostSubmissionPlanTests: XCTestCase {
    func testPreservesBodyAndAlwaysSubmitsWhenNoTrailingNewline() {
        // The bug we are fixing: the prompt landed in the worker's
        // input buffer but was never submitted. The writer must
        // always follow the paste with a Return keystroke, regardless
        // of whether the caller bothered to terminate the text.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "echo hello")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "echo hello", sendReturn: true))
    }

    func testStripsSingleTrailingNewlineBeforeSubmitting() {
        // Earlier revisions of `bossctl agents send` appended `\n`
        // to the payload in the belief that libghostty's paste path
        // would treat it as Enter. It does not — the `\n` lands as a
        // literal newline character in the input field, leaving the
        // prompt with a trailing blank line when the writer adds its
        // own Return. Strip the trailing newline so the submitted
        // prompt matches what the human meant to type.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "echo hello\n")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "echo hello", sendReturn: true))
    }

    func testStripsTrailingCRLFAndRepeatedNewlines() {
        // Heredoc-quoted prompts coming through shells can carry
        // `\r\n` line endings or a couple of trailing newlines.
        // Strip them all — they would otherwise pollute the input
        // field with stray blank lines before the Return keystroke
        // submits.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "first\nsecond\r\n\n")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "first\nsecond", sendReturn: true))
    }

    func testInternalNewlinesArePreserved() {
        // Multi-line prompts (e.g. a Stop-boundary probe asking the
        // worker to "explain what you're blocked on" across two
        // sentences) must keep their internal newlines so the paste
        // delivers the full body. Only the *trailing* newline gets
        // stripped before the Return submits.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "line one\nline two")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "line one\nline two", sendReturn: true))
    }

    func testEmptyPayloadStillSubmits() {
        // A degenerate "press enter" intervene (empty body) is rare
        // but well-defined: submit whatever the human had already
        // typed into the input field. The writer should still
        // synthesize Return — the body just has nothing to paste.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "", sendReturn: true))
    }

    func testWhitespaceOnlyPayloadKeepsLeadingSpaces() {
        // Trailing newlines come off; other whitespace stays. A
        // human who explicitly typed a leading space (e.g. quoting
        // shell input) should see that space preserved in the
        // submitted prompt.
        let plan = GhosttyTerminalHostView.submissionPlan(for: "  spaced\n")
        XCTAssertEqual(plan, PaneSubmissionPlan(body: "  spaced", sendReturn: true))
    }
}

@MainActor
final class WorkersWorkspaceModelFocusTests: XCTestCase {
    func testFocusUnknownSlotReturnsUnknownSlot() {
        let model = WorkersWorkspaceModel()
        // Workers grid is 1...8; 99 has no slot at all.
        let result = model.focusWorkerPane(slotId: 99)
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for nonexistent slot, got \(result)")
            return
        }
    }

    func testFocusIdleSlotReturnsUnknownSlot() {
        let model = WorkersWorkspaceModel()
        // All slots start without a session attached. Focusing an
        // idle slot should fail the same way as an unknown one — the
        // app has nothing to raise. Mirrors the
        // `release_worker_pane` semantics for idle slots so the engine
        // can treat both cases the same way.
        let result = model.focusWorkerPane(slotId: 1)
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for idle slot, got \(result)")
            return
        }
    }
}

@MainActor
final class WorkersWorkspaceModelSpawnTests: XCTestCase {
    private func makeRequest(slot: Int, runId: String = "run-test") -> EngineSpawnRequest {
        EngineSpawnRequest(
            runId: runId,
            workspacePath: "/tmp/ws",
            slotId: slot,
            initialInput: "claude\n",
            env: [],
            summary: nil
        )
    }

    func testSpawnHonorsEngineClaimedSlot() {
        // Engine asked for slot 5. The app must host the pane in
        // slot 5 — not the lowest free slot, not a random one. This
        // is the contract that replaces the old firstIndex(where:)
        // heuristic.
        let model = WorkersWorkspaceModel()
        let result = model.spawnWorkerPane(makeRequest(slot: 5))
        guard case .success(let slotId, _) = result else {
            XCTFail("expected .success, got \(result)")
            return
        }
        XCTAssertEqual(slotId, 5, "app must honor the engine-supplied slot")
        XCTAssertNotNil(
            model.slots.first(where: { $0.slotId == 5 })?.session,
            "slot 5 should now host a session"
        )
        XCTAssertNil(
            model.slots.first(where: { $0.slotId == 1 })?.session,
            "no other slot should be touched when the engine asked for slot 5"
        )
    }

    func testSpawnIntoOccupiedSlotReturnsSlotBusy() {
        // Engine and app disagree about whether slot 3 is free. The
        // app must surface .slotBusy rather than silently picking a
        // different slot — that would re-introduce the dual
        // allocator the engine-owns-slots refactor exists to remove.
        let model = WorkersWorkspaceModel()
        _ = model.spawnWorkerPane(makeRequest(slot: 3, runId: "run-first"))
        let result = model.spawnWorkerPane(makeRequest(slot: 3, runId: "run-second"))
        guard case .failure(.slotBusy) = result else {
            XCTFail("expected .slotBusy when engine asks for an occupied slot, got \(result)")
            return
        }
    }

    func testSpawnRejectsOutOfRangeSlot() {
        let model = WorkersWorkspaceModel()
        let zeroResult = model.spawnWorkerPane(makeRequest(slot: 0))
        guard case .failure(.internalFailure) = zeroResult else {
            XCTFail("expected .internalFailure for slot 0, got \(zeroResult)")
            return
        }
        let highResult = model.spawnWorkerPane(makeRequest(slot: 99))
        guard case .failure(.internalFailure) = highResult else {
            XCTFail("expected .internalFailure for slot 99, got \(highResult)")
            return
        }
    }
}
