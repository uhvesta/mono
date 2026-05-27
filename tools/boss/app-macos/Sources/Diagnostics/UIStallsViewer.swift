import AppKit
import SwiftUI

/// In-app surface for the main-thread stall log (the shake's "reviewing
/// stalls shouldn't require attaching Instruments"). Lists recent stalls
/// recorded by [[MainThreadStallMonitor]] with a since-window filter, an
/// expandable backtrace per row, and a "Copy UI stall log" button that
/// drops the plain-text report onto the pasteboard.
///
/// The log is a plain `@unchecked Sendable` class rather than an
/// `ObservableObject`, so this view polls a snapshot on a timer (the
/// same approach [[MetricsViewer]] uses for engine counters).
struct UIStallsViewer: View {
    @AppStorage("boss.uiStalls.visible") private var isOpen = false
    @State private var records: [StallRecord] = []
    @State private var window: SinceWindow = .fiveMinutes
    @State private var expanded: Set<UUID> = []

    private let pollTimer = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

    enum SinceWindow: String, CaseIterable, Identifiable {
        case oneMinute = "1m"
        case fiveMinutes = "5m"
        case fifteenMinutes = "15m"
        case all = "All"
        var id: String { rawValue }
        var seconds: TimeInterval? {
            switch self {
            case .oneMinute: 60
            case .fiveMinutes: 300
            case .fifteenMinutes: 900
            case .all: nil
            }
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            content
        }
        .frame(minWidth: 620, minHeight: 420)
        .onAppear { isOpen = true; refresh() }
        .onDisappear { isOpen = false }
        .onReceive(pollTimer) { _ in refresh() }
        .onChange(of: window) { _, _ in refresh() }
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text("UI Stalls")
                .font(.headline)
            Picker("Since", selection: $window) {
                ForEach(SinceWindow.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented)
            .fixedSize()
            Spacer()
            Text("\(records.count) stall\(records.count == 1 ? "" : "s")")
                .font(.caption)
                .foregroundStyle(.secondary)
            Button {
                copyReport()
            } label: {
                Label("Copy UI stall log", systemImage: "doc.on.clipboard")
            }
            .help("Copy the plain-text stall report for the selected window")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    @ViewBuilder
    private var content: some View {
        if records.isEmpty {
            emptyState
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    // Newest first.
                    ForEach(records.reversed()) { rec in
                        StallRow(
                            record: rec,
                            isExpanded: expanded.contains(rec.id)
                        ) { toggle(rec.id) }
                        Divider().padding(.leading, 14)
                    }
                }
                .padding(.vertical, 4)
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: 10) {
            Image(systemName: "checkmark.seal")
                .font(.largeTitle)
                .foregroundStyle(.green)
            Text("No stalls in the last \(window.rawValue)")
                .font(.headline)
            Text("The main-thread watchdog records here whenever a heartbeat lands more than \(Int(MainThreadStallMonitor.shared.thresholdMs)) ms late. An empty list means the UI stayed responsive.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 420)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func refresh() {
        let log = MainThreadStallMonitor.shared.log
        if let seconds = window.seconds {
            records = log.recent(since: Date().addingTimeInterval(-seconds))
        } else {
            records = log.snapshot()
        }
    }

    private func toggle(_ id: UUID) {
        if expanded.contains(id) { expanded.remove(id) } else { expanded.insert(id) }
    }

    private func copyReport() {
        let text = StallLog.formattedDump(records)
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(text, forType: .string)
    }
}

private struct StallRow: View {
    let record: StallRecord
    let isExpanded: Bool
    let onToggle: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button(action: onToggle) {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Image(systemName: isExpanded ? "chevron.down" : "chevron.right")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                    Text(String(format: "≥%.0f ms", record.durationMs))
                        .font(.system(.body, design: .monospaced).weight(.semibold))
                        .foregroundStyle(durationColor)
                    Text(record.context)
                        .font(.callout)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer()
                    Text(timeText)
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                        .monospacedDigit()
                }
            }
            .buttonStyle(.plain)

            if isExpanded {
                if record.backtrace.isEmpty {
                    Text("No backtrace captured (the frame walk failed — see MainThreadBacktrace).")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .padding(.leading, 24)
                } else {
                    Text(record.backtrace.joined(separator: "\n"))
                        .font(.system(.caption, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(8)
                        .background(Color.secondary.opacity(0.08))
                        .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
                        .padding(.leading, 24)
                }
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .contentShape(Rectangle())
    }

    private var durationColor: Color {
        if record.durationMs >= 1000 { return .red }
        if record.durationMs >= 500 { return .orange }
        return .yellow
    }

    private var timeText: String {
        let date = Date(timeIntervalSince1970: Double(record.tsEpochMs) / 1000.0)
        let f = DateFormatter()
        f.dateFormat = "HH:mm:ss.SSS"
        return f.string(from: date)
    }
}
