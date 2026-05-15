import Charts
import SwiftUI

/// Debug pane that lists every engine counter and gauge and shows
/// their current values, refreshed every 5 seconds while the window
/// is open. A small sparkline renders value history collected
/// in-session so short-term trends are visible without a persistent
/// data store. The pane is read-only — counters are monotonic and
/// only the engine increments them.
struct MetricsViewer: View {
    @EnvironmentObject private var chatModel: ChatViewModel
    @AppStorage("boss.metricsViewer.visible") private var isOpen = false
    @State private var searchText: String = ""
    /// Rolling sample buffer per metric name: (sample time, value).
    /// Capped at 60 samples (~5 min at the 5s poll cadence). Session-only;
    /// not persisted — the sparkline shows within-session trends.
    @State private var history: [String: [(Date, Int64)]] = [:]

    private let pollTimer = Timer.publish(every: 5, on: .main, in: .common).autoconnect()

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            content
        }
        .onAppear {
            chatModel.refreshMetrics()
            isOpen = true
        }
        .onDisappear { isOpen = false }
        .onReceive(pollTimer) { _ in
            chatModel.refreshMetrics()
        }
        .onChange(of: chatModel.engineMetrics) { _, newMetrics in
            appendSamples(newMetrics)
        }
        .frame(minWidth: 680, minHeight: 440)
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text("Metrics")
                .font(.headline)
            Spacer()
            TextField("Filter by name or description", text: $searchText)
                .textFieldStyle(.roundedBorder)
                .frame(width: 240)
            Text("\(chatModel.engineMetrics.count) metric\(chatModel.engineMetrics.count == 1 ? "" : "s")")
                .font(.caption)
                .foregroundStyle(.secondary)
            Button(action: { chatModel.refreshMetrics() }) {
                Image(systemName: "arrow.clockwise")
            }
            .buttonStyle(.borderless)
            .help("Re-read metrics from the engine")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    @ViewBuilder
    private var content: some View {
        if chatModel.engineMetrics.isEmpty {
            emptyState
        } else if filteredMetrics.isEmpty {
            noResultsState
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(filteredMetrics) { metric in
                        MetricRow(metric: metric, samples: history[metric.name] ?? [])
                        if metric.id != filteredMetrics.last?.id {
                            Divider().padding(.leading, 14)
                        }
                    }
                }
                .padding(.vertical, 4)
            }
        }
    }

    private var filteredMetrics: [EngineMetric] {
        let q = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !q.isEmpty else { return chatModel.engineMetrics }
        return chatModel.engineMetrics.filter {
            $0.name.localizedCaseInsensitiveContains(q)
                || $0.description.localizedCaseInsensitiveContains(q)
        }
    }

    private var emptyState: some View {
        VStack(spacing: 10) {
            Text("No metrics registered")
                .font(.headline)
            Text("The engine returned an empty metric set. This is unexpected — the registry should contain at least one entry. Try Refresh, or check the engine log.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 440)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var noResultsState: some View {
        VStack(spacing: 10) {
            Text("No matching metrics")
                .font(.headline)
            Text("Try a different search term.")
                .foregroundStyle(.secondary)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func appendSamples(_ metrics: [EngineMetric]) {
        let now = Date()
        for metric in metrics {
            var samples = history[metric.name] ?? []
            samples.append((now, metric.value))
            if samples.count > 60 {
                samples.removeFirst(samples.count - 60)
            }
            history[metric.name] = samples
        }
    }
}

private struct MetricRow: View {
    let metric: EngineMetric
    let samples: [(Date, Int64)]

    var body: some View {
        HStack(alignment: .center, spacing: 14) {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(metric.name)
                        .font(.system(.body, design: .monospaced).weight(.semibold))
                        .textSelection(.enabled)
                    KindPill(kind: metric.kind)
                    if metric.stale {
                        StalePill()
                    }
                }
                Text(metric.description)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if metric.timestampMs > 0 {
                    Text(lastUpdatedText)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }
            Spacer(minLength: 12)
            VStack(alignment: .trailing, spacing: 6) {
                Text("\(metric.value)")
                    .font(.system(.title2, design: .monospaced).weight(.bold))
                    .foregroundStyle(.primary)
                    .monospacedDigit()
                if samples.count >= 3 {
                    MetricSparkline(samples: samples)
                        .frame(width: 90, height: 26)
                }
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private var lastUpdatedText: String {
        let ms = metric.timestampMs
        guard ms > 0 else { return "never updated" }
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000.0)
        let interval = Date().timeIntervalSince(date)
        if interval < 60 {
            return "updated \(Int(interval))s ago"
        } else if interval < 3600 {
            return "updated \(Int(interval / 60))m ago"
        } else {
            return "updated \(Int(interval / 3600))h ago"
        }
    }
}

private struct KindPill: View {
    let kind: String

    private var color: Color { kind == "counter" ? .blue : .purple }

    var body: some View {
        Text(kind)
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, 5)
            .padding(.vertical, 2)
            .background(color.opacity(0.15))
            .foregroundStyle(color)
            .clipShape(RoundedRectangle(cornerRadius: 4, style: .continuous))
    }
}

private struct StalePill: View {
    var body: some View {
        Text("stale")
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, 5)
            .padding(.vertical, 2)
            .background(Color.orange.opacity(0.15))
            .foregroundStyle(Color.orange)
            .clipShape(RoundedRectangle(cornerRadius: 4, style: .continuous))
    }
}

/// Inline sparkline showing the last N samples of a metric. Uses
/// Swift Charts so the line respects the system accent color and
/// renders with anti-aliasing at any size.
private struct MetricSparkline: View {
    let samples: [(Date, Int64)]

    var body: some View {
        Chart {
            ForEach(Array(samples.enumerated()), id: \.offset) { idx, sample in
                LineMark(
                    x: .value("t", idx),
                    y: .value("v", sample.1)
                )
                .foregroundStyle(Color.accentColor)

                AreaMark(
                    x: .value("t", idx),
                    yStart: .value("min", minValue),
                    yEnd: .value("v", sample.1)
                )
                .foregroundStyle(
                    LinearGradient(
                        colors: [Color.accentColor.opacity(0.25), Color.accentColor.opacity(0.0)],
                        startPoint: .top,
                        endPoint: .bottom
                    )
                )
            }
        }
        .chartXAxis(.hidden)
        .chartYAxis(.hidden)
        .chartLegend(.hidden)
        .chartXScale(domain: 0...(samples.count - 1))
    }

    private var minValue: Int64 {
        samples.map(\.1).min() ?? 0
    }
}
