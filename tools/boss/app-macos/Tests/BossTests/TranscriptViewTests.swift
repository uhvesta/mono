import XCTest
import SwiftUI
@testable import Boss

/// Tests for the lazy segmented transcript renderer (transcript-viewer.md
/// task 4). Three concerns:
///
/// 1. **Wire decode** — `TranscriptSegmentVM` decodes the engine's
///    snake_cased `execution_transcript` segments (roles, collapse flags,
///    truncation) so the view stays a thin reflection of engine values.
/// 2. **The MarkdownUI-laziness spike** (design Risks) — hosting a
///    ~500-segment transcript must NOT build every segment's markdown AST
///    on open. A render probe confirms `List` realizes only a bounded set
///    of rows; if this regresses, the design's windowing/paging fallback
///    is needed.
/// 3. **RPC wiring** — `loadExecutions` / `loadTranscript` put the right
///    wire fields on the socket (the execution-list field-name regression
///    that left the viewer's left pane spinning, plus the transcript fetch).
@MainActor
final class TranscriptViewTests: XCTestCase {

    // MARK: - Wire decode

    func testSegmentDecodesThinkingWithCollapseFlags() throws {
        let json = """
        {
          "seq": 3,
          "role": "thinking",
          "label": "💭 Thinking",
          "timestamp": "2026-05-29T00:00:00.000Z",
          "model": "claude-opus-4-8",
          "markdown": "> reasoning",
          "collapsible": true,
          "default_collapsed": true,
          "truncated": null
        }
        """.data(using: .utf8)!
        let seg = try JSONDecoder().decode(TranscriptSegmentVM.self, from: json)
        XCTAssertEqual(seg.seq, 3)
        XCTAssertEqual(seg.id, 3)
        XCTAssertEqual(seg.role, .thinking)
        XCTAssertEqual(seg.label, "💭 Thinking")
        XCTAssertEqual(seg.model, "claude-opus-4-8")
        XCTAssertTrue(seg.collapsible)
        XCTAssertTrue(seg.defaultCollapsed)
        XCTAssertNil(seg.truncated)
    }

    func testSegmentDecodesTruncatedToolResult() throws {
        let json = """
        {
          "seq": 0,
          "role": "tool",
          "label": "↳ result",
          "timestamp": null,
          "model": null,
          "markdown": "```\\nx\\n```",
          "collapsible": true,
          "default_collapsed": false,
          "truncated": { "shown_bytes": 1024, "total_bytes": 20000 }
        }
        """.data(using: .utf8)!
        let seg = try JSONDecoder().decode(TranscriptSegmentVM.self, from: json)
        XCTAssertEqual(seg.role, .tool)
        XCTAssertNil(seg.timestamp)
        XCTAssertNil(seg.model)
        XCTAssertTrue(seg.collapsible)
        XCTAssertFalse(seg.defaultCollapsed)
        XCTAssertEqual(seg.truncated?.shownBytes, 1024)
        XCTAssertEqual(seg.truncated?.totalBytes, 20000)
    }

    func testSegmentDecodesAllRoles() throws {
        for raw in ["user", "assistant", "thinking", "tool", "system"] {
            let json = """
            {"seq":1,"role":"\(raw)","label":"L","timestamp":null,"model":null,"markdown":"x","collapsible":false,"default_collapsed":false,"truncated":null}
            """.data(using: .utf8)!
            let seg = try JSONDecoder().decode(TranscriptSegmentVM.self, from: json)
            XCTAssertEqual(seg.role.rawValue, raw)
        }
    }

    func testTruncationByteFormatting() {
        // Sanity-check the affordance string renders human-readable sizes.
        let s = SegmentRowView.formatBytes(20_000)
        XCTAssertFalse(s.isEmpty)
        XCTAssertTrue(s.contains("KB") || s.contains("bytes"), "got: \(s)")
    }

    // MARK: - MarkdownUI-laziness spike (design Risks)

    /// Build a synthetic ~500-segment transcript, host the real
    /// `TranscriptView` at a bounded height, and confirm `List` does not
    /// build the markdown AST for every segment — only the rows it
    /// realizes near the viewport. The companion small-transcript test
    /// proves the probe fires when rows DO render, so a `renderedCount`
    /// strictly below the total here is meaningful evidence of laziness
    /// rather than "nothing rendered".
    func testLargeTranscriptDoesNotEagerlyRenderEverySegment() {
        let segments = (0..<500).map { i in
            TranscriptSegmentVM(
                seq: i,
                role: .assistant,
                label: "Assistant",
                timestamp: nil,
                model: "claude-opus-4-8",
                markdown: "Segment \(i)\n\nA paragraph with **bold** and `code` so there is a real AST to build.",
                collapsible: false,
                defaultCollapsed: false,
                truncated: nil
            )
        }
        let doc = TranscriptDoc(executionId: "exec_big", segments: segments, isLive: false, complete: true)
        let probe = TranscriptRenderProbe()

        hostAndLayout(TranscriptView(doc: doc, renderProbe: probe), height: 640)

        let rendered = probe.renderedCount
        // Visible into the test log so the spike's measured number is on record.
        print("[laziness-spike] rendered \(rendered) of \(segments.count) segment bodies in a 640pt host")
        // Spike result (transcript-viewer.md Risk #1): a plain `List` of these
        // variable-height rows rendered ALL 500 here (eager — fails the perf
        // goal), so the renderer uses `ScrollView { LazyVStack }`, which builds
        // only the rows near the 640pt viewport (~17 observed). Bound well below
        // the total to catch a regression to an eager container, with generous
        // headroom over the observed count for viewport/overscan variance.
        XCTAssertLessThan(
            rendered, 100,
            "expected only viewport-near rows to build their markdown AST (~17 observed for a "
                + "640pt host); built \(rendered) of \(segments.count) — laziness regressed, fall "
                + "back to windowing/paging per transcript-viewer.md Risk #1."
        )
    }

    /// Confirms the render probe actually fires when rows render: a small
    /// transcript that fits the host realizes (at least some of) its rows,
    /// so the large-transcript bound above is a real laziness signal, not
    /// a vacuous "0 < 500".
    func testSmallTranscriptRendersItsRows() {
        let segments = (0..<4).map { i in
            TranscriptSegmentVM(
                seq: i,
                role: .user,
                label: "User",
                timestamp: nil,
                model: nil,
                markdown: "Hello \(i)",
                collapsible: false,
                defaultCollapsed: false,
                truncated: nil
            )
        }
        let doc = TranscriptDoc(executionId: "exec_small", segments: segments, isLive: false, complete: true)
        let probe = TranscriptRenderProbe()

        hostAndLayout(TranscriptView(doc: doc, renderProbe: probe), height: 800)

        XCTAssertGreaterThan(
            probe.renderedCount, 0,
            "a small transcript that fits the host should render at least one row; "
                + "if zero, the laziness probe is not observing realization in this harness"
        )
    }

    // MARK: - RPC wiring

    /// Regression: the execution list keys on `work_item_id` on the wire
    /// (the engine's `ListExecutions`/`ExecutionsList`). Sending `task_id`
    /// left the filter unset and the reply dropped, so the viewer's left
    /// pane spun forever. Pin the wire field.
    func testLoadExecutionsSendsWorkItemId() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var captured: [[String: Any]] = []
        model.outboundRecorder = { captured.append($0) }

        model.loadExecutions(taskId: "task_abc")

        let payload = captured.first { ($0["type"] as? String) == "list_executions" }
        XCTAssertNotNil(payload, "expected a list_executions payload on the wire")
        XCTAssertEqual(payload?["work_item_id"] as? String, "task_abc")
        XCTAssertNil(payload?["task_id"], "must not send the stale task_id field")
    }

    func testLoadTranscriptSendsExecutionTranscriptRequest() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var captured: [[String: Any]] = []
        model.outboundRecorder = { captured.append($0) }

        model.loadTranscript(executionId: "exec_42")

        let payload = captured.first { ($0["type"] as? String) == "execution_transcript" }
        XCTAssertNotNil(payload, "expected an execution_transcript payload on the wire")
        XCTAssertEqual(payload?["execution_id"] as? String, "exec_42")
        // The store flips to .loading so the viewer shows a spinner.
        if case .loading = model.transcriptsByExecutionID["exec_42"] {} else {
            XCTFail("loadTranscript should mark the execution as loading")
        }
    }

    func testLoadTranscriptIsIdempotentUntilRefresh() {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        var sends = 0
        model.outboundRecorder = { payload in
            if (payload["type"] as? String) == "execution_transcript" { sends += 1 }
        }

        model.loadTranscript(executionId: "exec_7")
        model.loadTranscript(executionId: "exec_7")  // already requested → no re-send
        XCTAssertEqual(sends, 1, "re-selecting an execution must not re-hit the engine")

        model.refreshTranscript(executionId: "exec_7")  // explicit refresh re-sends
        XCTAssertEqual(sends, 2)
    }

    // MARK: - Helpers

    /// Host a view in an offscreen window and drive a layout pass + a short
    /// run-loop turn so a `List`'s backing `NSTableView` performs its lazy
    /// row realization. Mirrors the `NSHostingView` hosting pattern in
    /// `DesignsTests`, with a window so cell realization actually runs.
    private func hostAndLayout(_ view: some View, height: CGFloat) {
        let frame = NSRect(x: 0, y: 0, width: 900, height: height)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = frame
        let window = NSWindow(
            contentRect: frame,
            styleMask: [.titled],
            backing: .buffered,
            defer: false
        )
        window.contentView = hosting
        hosting.layoutSubtreeIfNeeded()
        RunLoop.current.run(until: Date().addingTimeInterval(0.4))
        window.orderOut(nil)
    }
}
