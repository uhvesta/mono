import XCTest
@testable import Boss

final class DispatchEventDecoderTests: XCTestCase {
    func testDecodesOkEvent() {
        let line = #"{"ts_epoch_ms":1778539061840,"stage":"request_recorded","outcome":"ok","execution_id":"exec_18ae9d258b5872e8_48","work_item_id":"task_18ae9d2104410c10_3d","worker_id":null,"cube_repo_id":null,"cube_lease_id":null,"cube_workspace_id":null,"details":{"preferred_workspace_id":null}}"#
        let event = DispatchEventDecoder.decode(line: line)
        XCTAssertNotNil(event)
        XCTAssertEqual(event?.stage, "request_recorded")
        XCTAssertEqual(event?.outcome, "ok")
        XCTAssertEqual(event?.executionId, "exec_18ae9d258b5872e8_48")
        XCTAssertEqual(event?.workItemId, "task_18ae9d2104410c10_3d")
        XCTAssertNil(event?.workerId)
        XCTAssertNil(event?.errorMessage)
    }

    func testDecodesErrorEvent() {
        let line = #"{"ts_epoch_ms":1778539065203,"stage":"run_started","outcome":"error","execution_id":"exec_x","work_item_id":"task_y","worker_id":"worker-3","cube_repo_id":"mono","cube_lease_id":"abc","cube_workspace_id":"mono-agent-001","error_message":"execution is not ready","details":null}"#
        let event = DispatchEventDecoder.decode(line: line)
        XCTAssertEqual(event?.outcome, "error")
        XCTAssertEqual(event?.workerId, "worker-3")
        XCTAssertEqual(event?.errorMessage, "execution is not ready")
        XCTAssertNil(event?.detailsJSON)
    }

    func testReturnsNilForBlankLine() {
        XCTAssertNil(DispatchEventDecoder.decode(line: ""))
        XCTAssertNil(DispatchEventDecoder.decode(line: "   "))
    }

    func testReturnsNilForMalformedJSON() {
        XCTAssertNil(DispatchEventDecoder.decode(line: "{not json"))
    }

    func testReturnsNilForMissingRequiredField() {
        XCTAssertNil(DispatchEventDecoder.decode(line: #"{"stage":"x","outcome":"ok"}"#))
    }

    func testShortIdReturnsTrailingSegment() {
        XCTAssertEqual(DispatchEvent.shortId("exec_18ae9d258b5872e8_48"), "48")
        XCTAssertEqual(DispatchEvent.shortId("task_abcdef_12"), "12")
        XCTAssertEqual(DispatchEvent.shortId("noprefix"), "noprefix")
    }
}

@MainActor
final class DispatchEventsViewerModelFilterTests: XCTestCase {
    private func event(
        stage: String,
        outcome: String = "ok",
        executionId: String = "exec_aaa_1",
        workItem: String? = nil,
        worker: String? = nil,
        error: String? = nil,
        ts: UInt64 = 0
    ) -> DispatchEvent {
        DispatchEvent(
            tsEpochMs: ts,
            stage: stage,
            outcome: outcome,
            executionId: executionId,
            workItemId: workItem,
            workerId: worker,
            cubeRepoId: nil,
            cubeLeaseId: nil,
            cubeWorkspaceId: nil,
            errorMessage: error,
            detailsJSON: nil
        )
    }

    func testNoFiltersReturnsAll() {
        let model = DispatchEventsViewerModel()
        model.events = [
            event(stage: "run_started"),
            event(stage: "pane_spawned", outcome: "error"),
        ]
        XCTAssertEqual(model.filtered.count, 2)
    }

    func testStageFilter() {
        let model = DispatchEventsViewerModel()
        model.events = [
            event(stage: "run_started"),
            event(stage: "pane_spawned"),
        ]
        model.toggleStage("run_started")
        XCTAssertEqual(model.filtered.map(\.stage), ["run_started"])
    }

    func testOutcomeFilter() {
        let model = DispatchEventsViewerModel()
        model.events = [
            event(stage: "run_started", outcome: "ok"),
            event(stage: "run_started", outcome: "error"),
        ]
        model.toggleOutcome("error")
        XCTAssertEqual(model.filtered.map(\.outcome), ["error"])
    }

    func testSearchMatchesErrorMessage() {
        let model = DispatchEventsViewerModel()
        model.events = [
            event(stage: "run_started", outcome: "error", error: "SlotBusy"),
            event(stage: "run_started", outcome: "ok"),
        ]
        model.searchText = "slotbusy"
        XCTAssertEqual(model.filtered.count, 1)
        XCTAssertEqual(model.filtered.first?.errorMessage, "SlotBusy")
    }

    func testClearFiltersResetsAll() {
        let model = DispatchEventsViewerModel()
        model.events = [event(stage: "run_started")]
        model.toggleStage("pane_spawned")
        model.toggleOutcome("error")
        model.searchText = "x"
        XCTAssertTrue(model.filtered.isEmpty)
        model.clearFilters()
        XCTAssertEqual(model.filtered.count, 1)
    }
}
