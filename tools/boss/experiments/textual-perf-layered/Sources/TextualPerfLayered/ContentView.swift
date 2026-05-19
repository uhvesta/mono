import Foundation
import os.log
import SwiftUI
import Textual

/// Bisection layers, ordered from "Textual only" to "full Boss async fetch
/// flow". Each layer adds exactly one wrapper class on top of the previous.
/// Re-clicking a layer in the picker forces a fresh render (the pane is
/// keyed by `.id(layer)`), so you can capture multiple samples and average
/// them. The os.log subsystem `com.boss.textualperf` / category `Render`
/// matches the textual-perf rig in PR #686 so numbers compare directly.
enum Layer: String, CaseIterable, Identifiable {
    /// L0: Same shape as the textual-perf rig in #686. Sets the baseline.
    case textualOnly

    /// L1: Add `.bossMarkdown()` only. Isolates the cost of Boss's
    /// `BossStructuredTextStyle` (custom heading / code-block / table /
    /// blockquote / inline styles).
    case bossMarkdown

    /// L2: Add Boss's inner wrappers around `StructuredText`:
    /// `.bossMarkdown()`, `.textual.textSelection(.enabled)`,
    /// `.frame(maxWidth: .infinity, alignment: .leading)`, the title +
    /// divider VStack, double padding, outer `.textSelection(.enabled)`
    /// on the ScrollView. Mirrors `MarkdownViewerScrollContent` minus
    /// the comments layer and view-model state machine.
    case bossWrappers

    /// L3: Add the `.withComments()` wrapper: an HStack around the
    /// content, a `@StateObject` `CommentLayer`-shaped observable,
    /// `.environment(...)` injections for `commentedTexts` and
    /// `commentFlashText`, plus a hidden ⌘⇧K button. NSEvent monitors
    /// are intentionally not installed in the rig.
    case bossWithComments

    /// L4: Add a viewmodel state machine that flips from `.loading` to
    /// `.loaded(title, markdown)`, with `.id(renderContentID)` forcing a
    /// fresh view per content load. Mirrors `AsyncMarkdownViewerView` +
    /// `AsyncMarkdownViewerViewModel`.
    case bossViewModel

    /// L5: Add an async load that mimics the click-to-first-paint flow:
    /// state starts `.loading`, a `.task { ... }` reads the markdown
    /// off-main, then transitions to `.loaded`. Captures any rebuild
    /// thrash between the spinner and the rendered doc.
    case bossAsyncFetch

    var id: String { rawValue }

    var label: String {
        switch self {
        case .textualOnly: "L0 · Textual only"
        case .bossMarkdown: "L1 · + bossMarkdown()"
        case .bossWrappers: "L2 · + Boss inner wrappers"
        case .bossWithComments: "L3 · + .withComments()"
        case .bossViewModel: "L4 · + view-model"
        case .bossAsyncFetch: "L5 · + async fetch"
        }
    }

    var shortName: String {
        switch self {
        case .textualOnly: "L0"
        case .bossMarkdown: "L1"
        case .bossWrappers: "L2"
        case .bossWithComments: "L3"
        case .bossViewModel: "L4"
        case .bossAsyncFetch: "L5"
        }
    }
}

/// Preference key used to detect when `StructuredText` has been laid out
/// for the first time. Matches the shape Boss uses in
/// `MarkdownViewerScrollContent` so the parse-end signal is the same.
struct StructuredTextHeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = nextValue()
    }
}

struct ContentView: View {
    @State private var selected: Layer = .textualOnly
    @State private var sample: SampleSource = SampleSource.load()

    var body: some View {
        VStack(spacing: 0) {
            Picker("Layer", selection: $selected) {
                ForEach(Layer.allCases) { layer in
                    Text(layer.label).tag(layer)
                }
            }
            .pickerStyle(.segmented)
            .padding(8)

            if let error = sample.errorMessage {
                Text(error)
                    .font(.callout)
                    .foregroundStyle(.red)
                    .padding(8)
            }

            Divider()

            LayerPane(layer: selected, source: sample.text)
                .id(selected)
        }
        .frame(minWidth: 800, minHeight: 600)
    }
}

/// Switches on `layer` to render the matching wrapper stack, and emits
/// the parse_start / parse_end / interactive timing logs from a single
/// place so each layer is measured identically.
struct LayerPane: View {
    let layer: Layer
    let source: String

    @State private var parseStart: Date?
    @State private var renderMs: Int?
    @State private var interactiveMs: Int?

    var body: some View {
        ZStack {
            switch layer {
            case .textualOnly:
                L0_TextualOnly(source: source)
            case .bossMarkdown:
                L1_BossMarkdown(source: source)
            case .bossWrappers:
                L2_BossWrappers(source: source)
            case .bossWithComments:
                L3_BossWithComments(source: source)
            case .bossViewModel:
                L4_BossViewModel(source: source)
            case .bossAsyncFetch:
                L5_BossAsyncFetch(source: source)
            }
        }
        .onPreferenceChange(StructuredTextHeightKey.self) { h in
            guard h > 0, renderMs == nil, let start = parseStart else { return }
            let now = Date.now
            let rMs = Int(now.timeIntervalSince(start) * 1000)
            let iMs = Int(now.timeIntervalSince(processStartTime) * 1000)
            renderMs = rMs
            interactiveMs = iMs
            renderLog.info(
                "phase=parse_end layer=\(layer.shortName, privacy: .public) duration_ms=\(rMs, privacy: .public) bytes=\(source.utf8.count, privacy: .public)"
            )
            renderLog.info(
                "phase=interactive layer=\(layer.shortName, privacy: .public) duration_ms=\(iMs, privacy: .public)"
            )
        }
        .onAppear {
            parseStart = Date.now
            renderMs = nil
            interactiveMs = nil
            renderLog.info("phase=parse_start layer=\(layer.shortName, privacy: .public)")
        }
        .overlay(alignment: .bottomTrailing) {
            timingOverlay
        }
    }

    @ViewBuilder
    private var timingOverlay: some View {
        if let rMs = renderMs {
            VStack(alignment: .trailing, spacing: 2) {
                Text("parse_end: \(rMs) ms")
                    .font(.caption.monospacedDigit())
                if let iMs = interactiveMs {
                    Text("interactive: \(iMs) ms")
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
            }
            .padding(8)
            .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 6))
            .padding(12)
        } else {
            Text("measuring…")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
                .padding(8)
                .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 6))
                .padding(12)
        }
    }
}
