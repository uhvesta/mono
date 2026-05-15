import XCTest
@testable import Boss

/// Covers `ChatViewModel.archiveProject(id:)` — the model method
/// wired to the right-click "Archive" context menu on navigator
/// project rows.
///
/// The acceptance criteria ask that:
///   - calling `archiveProject(id:)` sends `update_work_item` with
///     `status = "archived"` for the named project id.
///   - the outbound payload carries no extra keys beyond `type`,
///     `id`, and `patch`.
@MainActor
final class ProjectArchiveContextMenuTests: XCTestCase {

    /// Invoking `archiveProject` fires an `update_work_item` patch
    /// with `status = "archived"` on the wire.
    func testArchiveProjectSendsArchivedStatusPatch() {
        let model = makeModel()
        let recorder = installRecorder(on: model)

        model.archiveProject(id: "proj_abc")

        XCTAssertEqual(recorder.value.count, 1, "expected exactly one outbound message")
        let payload = recorder.value[0]
        XCTAssertEqual(payload["type"] as? String, "update_work_item")
        XCTAssertEqual(payload["id"] as? String, "proj_abc")
        let patch = payload["patch"] as? [String: Any]
        XCTAssertEqual(patch?["status"] as? String, "archived")
    }

    /// Archiving a different project sends the correct id.
    func testArchiveProjectUsesSuppliedID() {
        let model = makeModel()
        let recorder = installRecorder(on: model)

        model.archiveProject(id: "proj_xyz")

        XCTAssertEqual(recorder.value.first?["id"] as? String, "proj_xyz")
    }

    // MARK: - Helpers

    private func makeModel() -> ChatViewModel {
        ChatViewModel(socketPath: "/tmp/boss-archive-test-\(UUID().uuidString).sock")
    }

    private final class PayloadRecorder {
        var value: [[String: Any]] = []
    }

    private func installRecorder(on model: ChatViewModel) -> PayloadRecorder {
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in recorder.value.append(payload) }
        return recorder
    }
}
