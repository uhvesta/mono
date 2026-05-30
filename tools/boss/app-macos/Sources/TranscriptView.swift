import SwiftUI
import Textual
import os

// MARK: - Render probe (laziness spike instrumentation)

/// Test seam for the MarkdownUI-laziness spike called out in
/// `transcript-viewer.md` Risks. The renderer puts one `StructuredText`
/// per `List` row; the performance argument rests on `List` realizing —
/// and thus parsing the markdown of — only the rows near the viewport,
/// not all of them. This counter records which segment bodies were
/// actually built during a layout pass so a hosting test can assert the
/// `List` did not eagerly materialize all ~500 segments' ASTs.
///
/// Production callers never pass a probe (the parameter defaults to nil),
/// so this is inert outside tests — the same shape as the existing
/// `EngineClient.outboundRecorder` / `ChatViewModel.urlOpener` seams.
final class TranscriptRenderProbe: Sendable {
    private let state = OSAllocatedUnfairLock(initialState: Set<Int>())

    func recordRender(seq: Int) {
        state.withLock { _ = $0.insert(seq) }
    }

    /// Distinct segment `seq`s whose body has been built so far.
    var renderedSeqs: Set<Int> {
        state.withLock { $0 }
    }

    var renderedCount: Int {
        state.withLock { $0.count }
    }
}

// MARK: - TranscriptView

/// The lazy, segmented transcript renderer (transcript-viewer.md task 4).
///
/// One `List` row per segment, each rendered with the *existing*
/// `StructuredText(...).bossMarkdown()` component (the same `Textual` +
/// Boss-theme stack `DesignRendererView` / `MarkdownViewerView` use — no
/// second markdown renderer, per the design constraint). Because each
/// segment carries its own pre-rendered markdown body and `List` realizes
/// rows lazily, the markdown AST for a segment is only built when its row
/// scrolls into view — keeping a several-hundred-message transcript
/// responsive on open and scroll.
///
/// Collapsible segments (verbose thinking, large/truncated tool results)
/// render inside a `DisclosureGroup` honouring the engine's
/// `defaultCollapsed`; truncated results show a "showing N of M"
/// affordance. A jump-to-turn menu scrolls to any user turn.
struct TranscriptView: View {
    let doc: TranscriptDoc
    /// Invoked by the "Refresh" affordance shown for a live (still-running)
    /// execution. Nil hides the button.
    var onRefresh: (() -> Void)?
    /// Laziness-spike instrumentation; nil in production.
    var renderProbe: TranscriptRenderProbe?

    /// Per-segment expansion overrides keyed by `seq`. An absent entry
    /// means the segment uses its engine-supplied `defaultCollapsed`.
    @State private var expandedOverrides: [Int: Bool] = [:]
    /// Set by the jump menu; observed by the `ScrollViewReader` to scroll.
    @State private var jumpTarget: Int?

    init(
        doc: TranscriptDoc,
        onRefresh: (() -> Void)? = nil,
        renderProbe: TranscriptRenderProbe? = nil
    ) {
        self.doc = doc
        self.onRefresh = onRefresh
        self.renderProbe = renderProbe
    }

    /// User turns, used to populate the jump-to-turn menu.
    private var userTurns: [TranscriptSegmentVM] {
        doc.segments.filter { $0.role == .user }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            transcriptList
        }
    }

    // MARK: Header (live banner + jump nav + refresh)

    @ViewBuilder
    private var header: some View {
        HStack(spacing: 12) {
            if doc.isLive {
                Label("Still running — partial transcript", systemImage: "dot.radiowaves.left.and.right")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .labelStyle(.titleAndIcon)
            }
            Text("\(doc.segments.count) segments")
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer()
            if !userTurns.isEmpty {
                jumpMenu
            }
            if doc.isLive, let onRefresh {
                Button(action: onRefresh) {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .controlSize(.small)
                .help("Reload the latest transcript for this running execution")
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private var jumpMenu: some View {
        Menu {
            ForEach(Array(userTurns.enumerated()), id: \.element.seq) { index, turn in
                Button("Turn \(index + 1): \(turnPreview(turn))") {
                    jumpTarget = turn.seq
                }
            }
        } label: {
            Label("Jump to turn", systemImage: "list.number")
        }
        .menuStyle(.borderlessButton)
        .fixedSize()
        .controlSize(.small)
    }

    private func turnPreview(_ seg: TranscriptSegmentVM) -> String {
        let firstLine = seg.markdown
            .split(separator: "\n", maxSplits: 1, omittingEmptySubsequences: true)
            .first
            .map(String.init) ?? ""
        let trimmed = firstLine.trimmingCharacters(in: .whitespaces)
        return trimmed.count > 48 ? String(trimmed.prefix(48)) + "…" : trimmed
    }

    // MARK: Transcript list

    // A `ScrollView { LazyVStack }`, not a `List`: the laziness spike
    // (TranscriptViewTests) measured a plain `List` building the markdown
    // AST for ALL ~500 rows on open (variable-height rows force `List` to
    // measure every row), defeating the performance goal. `LazyVStack`
    // only realizes — and parses — rows near the viewport, which is the
    // "manual windowing" fallback the design's Risk #1 calls for. The
    // transcript pane has no per-row selection, so nothing is lost.
    private var transcriptList: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(doc.segments) { seg in
                        SegmentRowView(
                            seg: seg,
                            isExpanded: expansionBinding(for: seg),
                            probe: renderProbe
                        )
                        .id(seg.seq)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 6)
                        Divider()
                    }
                }
            }
            .textSelection(.enabled)
            .onChange(of: jumpTarget) { _, target in
                guard let target else { return }
                withAnimation { proxy.scrollTo(target, anchor: .top) }
                // Reset so re-selecting the same turn scrolls again.
                jumpTarget = nil
            }
        }
    }

    /// Binding for a collapsible segment's disclosure state: a user
    /// override if present, else the engine's `defaultCollapsed` (thinking
    /// and large/truncated results start collapsed).
    private func expansionBinding(for seg: TranscriptSegmentVM) -> Binding<Bool> {
        Binding(
            get: { expandedOverrides[seg.seq] ?? !seg.defaultCollapsed },
            set: { expandedOverrides[seg.seq] = $0 }
        )
    }
}

// MARK: - Segment row

/// One transcript segment. Collapsible segments wrap their body in a
/// `DisclosureGroup`; everything else stacks the header above the body.
struct SegmentRowView: View {
    let seg: TranscriptSegmentVM
    @Binding var isExpanded: Bool
    var probe: TranscriptRenderProbe?

    var body: some View {
        content
            .padding(.vertical, 2)
    }

    /// Evaluated by SwiftUI only when `List` realizes this row, so the
    /// markdown parse below (and the probe tick) happen lazily per visible
    /// row — the crux of the laziness goal. Implemented as a plain
    /// computed property (not a `@ViewBuilder` body) so the probe call is
    /// an ordinary statement.
    private var content: some View {
        probe?.recordRender(seq: seg.seq)
        return Group {
            if seg.collapsible {
                DisclosureGroup(isExpanded: $isExpanded) {
                    segmentBody
                } label: {
                    SegmentHeaderView(seg: seg)
                }
            } else {
                VStack(alignment: .leading, spacing: 6) {
                    SegmentHeaderView(seg: seg)
                    segmentBody
                }
            }
        }
    }

    @ViewBuilder
    private var segmentBody: some View {
        VStack(alignment: .leading, spacing: 6) {
            if !seg.markdown.isEmpty {
                StructuredText(markdown: seg.markdown)
                    .bossMarkdown()
                    .textual.textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if let truncated = seg.truncated {
                Text("Showing \(Self.formatBytes(truncated.shownBytes)) of \(Self.formatBytes(truncated.totalBytes))")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
    }

    static func formatBytes(_ bytes: Int) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }
}

// MARK: - Segment header

/// Compact header row: role-coloured label, optional model, timestamp.
struct SegmentHeaderView: View {
    let seg: TranscriptSegmentVM

    var body: some View {
        HStack(spacing: 8) {
            Text(seg.label)
                .font(.caption.weight(.semibold))
                .foregroundStyle(roleColor)
                .lineLimit(1)
            if let model = seg.model, !model.isEmpty {
                Text(model)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            if let ts = seg.timestamp, let formatted = Self.formatTimestamp(ts) {
                Text(formatted)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    private var roleColor: Color {
        switch seg.role {
        case .user: .blue
        case .assistant: .primary
        case .thinking: .purple
        case .tool: .teal
        case .system: .secondary
        }
    }

    /// Render an ISO-8601 timestamp as a short local time, tolerating the
    /// fractional-seconds form Claude Code writes. Returns nil (header
    /// omits the field) when the string doesn't parse.
    static func formatTimestamp(_ iso: String) -> String? {
        let withFraction = ISO8601DateFormatter()
        withFraction.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let plain = ISO8601DateFormatter()
        let date = withFraction.date(from: iso) ?? plain.date(from: iso)
        guard let date else { return nil }
        let display = DateFormatter()
        display.dateStyle = .none
        display.timeStyle = .medium
        return display.string(from: date)
    }
}
