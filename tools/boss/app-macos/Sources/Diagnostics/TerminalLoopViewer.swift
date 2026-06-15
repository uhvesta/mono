import AppKit
import SwiftUI

/// In-app surface for the embedded-terminal event-loop diagnostics — the
/// readable-without-a-debugger counterpart to the os.Logger warnings and
/// the on-disk JSONL mirror. Shows the live app-loop wakeup/tick rate, the
/// verify/refute verdict derived from the latest sample, a per-pane
/// pty/EOF/pid liveness table, and the recent tab-switch relayout costs.
///
/// Like [[UIStallsViewer]], the log is a plain `@unchecked Sendable` class
/// rather than an `ObservableObject`, so this view polls a snapshot on a
/// timer (the approach [[MetricsViewer]] uses for engine counters).
struct TerminalLoopViewer: View {
    @AppStorage("boss.terminalLoopViewer.visible") private var isOpen = false
    @AppStorage("boss.terminalLoop.sampling") private var samplingEnabled = true
    @AppStorage("boss.terminalLoop.deepContent") private var deepContent = false

    @State private var latest: LoopSample?
    @State private var tabSwitches: [TabSwitchSample] = []
    @State private var window: SinceWindow = .fiveMinutes

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
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    liveSection
                    paneSection
                    tabSwitchSection
                }
                .padding(14)
            }
        }
        .frame(minWidth: 640, minHeight: 460)
        .onAppear { isOpen = true; refresh() }
        .onDisappear { isOpen = false }
        .onReceive(pollTimer) { _ in refresh() }
        .onChange(of: window) { _, _ in refresh() }
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text("Terminal Loop")
                .font(.headline)
            Picker("Since", selection: $window) {
                ForEach(SinceWindow.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented)
            .fixedSize()
            Spacer()
            Toggle("Sample", isOn: $samplingEnabled)
                .toggleStyle(.switch)
                .help("Run the 1 Hz event-loop sampler. Off fully silences the instrumentation.")
            Toggle("Deep", isOn: $deepContent)
                .toggleStyle(.switch)
                .help("Also read each pane's viewport per sample to detect output flow (flood vs spin). Slightly heavier.")
            Button {
                copyReport()
            } label: {
                Label("Copy", systemImage: "doc.on.clipboard")
            }
            .help("Copy the plain-text terminal-loop report for the selected window")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    // MARK: Live rates + verdict

    @ViewBuilder
    private var liveSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("App event loop")
                .font(.subheadline.weight(.semibold))
            HStack(spacing: 28) {
                rateStat("wakeups/s", value: latest?.wakeupsPerSec, hot: isHot)
                rateStat("ticks/s", value: latest?.ticksPerSec, hot: false)
                rateStat("panes", count: latest?.panes.count)
            }
            ForEach(Array(verdicts.enumerated()), id: \.offset) { _, v in
                Label(v, systemImage: "exclamationmark.triangle.fill")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if verdicts.isEmpty, latest != nil {
                Label("No spin signature — loop quiet, no dead/EOF panes.", systemImage: "checkmark.seal")
                    .font(.callout)
                    .foregroundStyle(.green)
            }
            if latest == nil {
                Text(samplingEnabled
                    ? "Waiting for the first sample…"
                    : "Sampling is off — flip the Sample switch to collect data.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var isHot: Bool {
        (latest?.wakeupsPerSec ?? 0) >= TerminalLoopMonitor.shared.wakeupWarnPerSec
    }

    private var verdicts: [String] {
        guard let latest else { return [] }
        return TerminalLoopThresholds.warnings(
            for: latest,
            wakeupWarnPerSec: TerminalLoopMonitor.shared.wakeupWarnPerSec
        ).map(\.message)
    }

    private func rateStat(_ label: String, value: Double?, hot: Bool) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(value.map { String(format: "%.0f", $0) } ?? "—")
                .font(.system(.title2, design: .rounded).weight(.semibold))
                .foregroundStyle(hot ? Color.red : Color.primary)
                .monospacedDigit()
            Text(label).font(.caption).foregroundStyle(.secondary)
        }
    }

    private func rateStat(_ label: String, count: Int?) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(count.map(String.init) ?? "—")
                .font(.system(.title2, design: .rounded).weight(.semibold))
                .monospacedDigit()
            Text(label).font(.caption).foregroundStyle(.secondary)
        }
    }

    // MARK: Per-pane liveness

    @ViewBuilder
    private var paneSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Panes")
                .font(.subheadline.weight(.semibold))
            if let panes = latest?.panes, !panes.isEmpty {
                VStack(spacing: 0) {
                    ForEach(panes) { pane in
                        PaneLoopRow(pane: pane)
                        Divider()
                    }
                }
                .background(Color.secondary.opacity(0.05))
                .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
            } else {
                Text("No live panes.").font(.callout).foregroundStyle(.secondary)
            }
        }
    }

    // MARK: Tab switches

    @ViewBuilder
    private var tabSwitchSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Tab switches")
                .font(.subheadline.weight(.semibold))
            if tabSwitches.isEmpty {
                Text("None in the last \(window.rawValue).")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(tabSwitches.reversed()) { t in
                    HStack(spacing: 10) {
                        Text("\(t.from) → \(t.to)")
                            .font(.callout)
                        Spacer()
                        Text("\(t.panesRelaidOut) relaid-out")
                            .font(.caption).monospacedDigit()
                            .foregroundStyle(t.panesRelaidOut > 0 ? .primary : .secondary)
                        Text("\(t.panesTornDown) torn-down")
                            .font(.caption).monospacedDigit()
                            .foregroundStyle(t.panesTornDown > 0 ? .red : .secondary)
                        Text(String(format: "%.0f ms", t.settleMs))
                            .font(.caption).monospacedDigit()
                            .foregroundStyle(.secondary)
                    }
                    .padding(.vertical, 3)
                    Divider()
                }
            }
        }
    }

    // MARK: Data

    private func refresh() {
        let log = TerminalLoopMonitor.shared.log
        latest = log.loopSnapshot().last
        if let seconds = window.seconds {
            tabSwitches = log.recentTabSwitches(since: Date().addingTimeInterval(-seconds))
        } else {
            tabSwitches = log.tabSnapshot()
        }
    }

    private func copyReport() {
        let log = TerminalLoopMonitor.shared.log
        let loops: [LoopSample]
        let tabs: [TabSwitchSample]
        if let seconds = window.seconds {
            let since = Date().addingTimeInterval(-seconds)
            loops = log.recentLoops(since: since)
            tabs = log.recentTabSwitches(since: since)
        } else {
            loops = log.loopSnapshot()
            tabs = log.tabSnapshot()
        }
        let text = TerminalLoopLog.formattedDump(loops: loops, tabSwitches: tabs)
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(text, forType: .string)
    }
}

private struct PaneLoopRow: View {
    let pane: PaneLoopSample

    var body: some View {
        HStack(spacing: 10) {
            Circle()
                .fill(statusColor)
                .frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 1) {
                Text(pane.title)
                    .font(.callout)
                    .lineLimit(1)
                Text(subtitle)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            Text(statusText)
                .font(.caption.weight(.medium))
                .foregroundStyle(statusColor)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .help(pane.describe)
    }

    private var subtitle: String {
        var bits: [String] = []
        if let slotId = pane.slotId { bits.append("slot \(slotId)") }
        else { bits.append(pane.role) }
        if let runId = pane.runId { bits.append("run \(runId)") }
        bits.append(pane.foregroundPid == 0 ? "no pid" : "pid \(pane.foregroundPid)")
        if let changed = pane.contentChanged {
            bits.append(changed ? "output flowing" : "output frozen")
        }
        return bits.joined(separator: " · ")
    }

    private var statusText: String {
        if pane.processExited { return "EOF" }
        if pane.foregroundPid != 0 && !pane.pidAlive { return "DEAD PID" }
        return "live"
    }

    private var statusColor: Color {
        pane.isDeadOrEof ? .red : .green
    }
}
