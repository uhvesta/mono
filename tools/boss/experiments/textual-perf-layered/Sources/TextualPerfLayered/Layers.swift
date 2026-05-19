import Foundation
import SwiftUI
import Textual

// Shared GeometryReader that publishes the StructuredText's measured
// height into `StructuredTextHeightKey`. The outer `LayerPane` listens
// for the first non-zero value to compute `parse_end duration_ms`.
private struct HeightProbe: View {
    var body: some View {
        GeometryReader { geo in
            Color.clear.preference(
                key: StructuredTextHeightKey.self,
                value: geo.size.height
            )
        }
    }
}

// MARK: - L0 · Textual only

/// Baseline: same shape as the textual-perf rig in PR #686. No Boss
/// wrappers, no styles, no comments, no view model.
struct L0_TextualOnly: View {
    let source: String

    var body: some View {
        ScrollView {
            StructuredText(markdown: source)
                .padding()
                .background(HeightProbe())
        }
    }
}

// MARK: - L1 · + bossMarkdown()

/// Adds Boss's `bossMarkdown()` style stack — `BossHeadingStyle`,
/// `BossCodeBlockStyle`, `BossBlockQuoteStyle`, `BossTableStyle`, and
/// the boss inline style. Everything else is identical to L0.
struct L1_BossMarkdown: View {
    let source: String

    var body: some View {
        ScrollView {
            StructuredText(markdown: source)
                .bossMarkdown()
                .padding()
                .background(HeightProbe())
        }
    }
}

// MARK: - L2 · + Boss inner wrappers

/// Mirrors `MarkdownViewerScrollContent`'s outer shape minus the comments
/// modifier and view-model state machine — title text, divider, double
/// padding, `.frame(maxWidth: .infinity)` on the StructuredText and the
/// VStack, `.textual.textSelection(.enabled)` on the StructuredText, and
/// `.textSelection(.enabled)` on the outer ScrollView.
struct L2_BossWrappers: View {
    let source: String

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text("L2 · Boss inner wrappers")
                    .font(.title3.weight(.semibold))
                    .fixedSize(horizontal: false, vertical: true)
                Divider()
                StructuredText(markdown: source)
                    .bossMarkdown()
                    .textual.textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(HeightProbe())
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .textSelection(.enabled)
    }
}

// MARK: - L3 · + .withComments()

/// Adds an HStack-wrapping `.withComments()`-equivalent: a
/// `@StateObject` observable mirroring `CommentLayer`'s `@Published`
/// surface, environment injections for `commentedTexts` and
/// `commentFlashText`, the hidden ⌘⇧K keyboard-shortcut button, and the
/// add-comment overlay. NSEvent monitors are intentionally not
/// installed — adding global monitors from a benchmark rig is a hazard
/// and the monitors don't fire during render.
struct L3_BossWithComments: View {
    let source: String

    var body: some View {
        L2_BossWrappers(source: source)
            .withCommentsStub()
    }
}

// MARK: - L4 · + view-model

/// Mirrors `AsyncMarkdownViewerView` + `AsyncMarkdownViewerViewModel`:
/// the wrapper view reads from a `@StateObject` view-model, switches on
/// `.loading` vs `.loaded`, and stamps a fresh `.id(renderContentID)`
/// per content load so SwiftUI rebuilds the inner view from scratch
/// (matching Boss's behaviour when the same window is reused across
/// docs). Unlike L5, the state transitions synchronously in
/// `onAppear` — there is no `Task` boundary, so no `.loading` flash.
struct L4_BossViewModel: View {
    let source: String

    @StateObject private var vm = ViewerVMStub()

    var body: some View {
        Group {
            switch vm.state {
            case .loading:
                ProgressView("Loading…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .loaded(let title, let markdown):
                MarkdownViewerStub(title: title, source: markdown)
                    .id(vm.renderContentID)
                    .navigationTitle(title)
            }
        }
        .onAppear {
            if case .loading = vm.state {
                vm.renderContentID = UUID()
                vm.state = .loaded(title: "L4 · view-model", markdown: source)
            }
        }
    }
}

// MARK: - L5 · + async fetch

/// Adds the async-fetch boundary on top of L4. The view-model starts
/// `.loading`, an `.task` reads the source string off the main actor
/// (mimicking Boss's `Task.detached` file read), then assigns
/// `.loaded`. Captures any spinner→content rebuild thrash.
struct L5_BossAsyncFetch: View {
    let source: String

    @StateObject private var vm = ViewerVMStub()

    var body: some View {
        Group {
            switch vm.state {
            case .loading:
                ProgressView("Loading…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .loaded(let title, let markdown):
                MarkdownViewerStub(title: title, source: markdown)
                    .id(vm.renderContentID)
                    .navigationTitle(title)
            }
        }
        .task(id: ObjectIdentifier(vm)) {
            guard case .loading = vm.state else { return }
            let body = source
            let loaded: String = await Task.detached {
                return body
            }.value
            vm.renderContentID = UUID()
            vm.state = .loaded(title: "L5 · async fetch", markdown: loaded)
        }
    }
}

// MARK: - Shared inner viewer (used by L4 and L5)

/// Mirrors `MarkdownViewerScrollContent` (with the comments stub) so L4
/// and L5 share the same inner rendering surface.
private struct MarkdownViewerStub: View {
    let title: String
    let source: String

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text(title)
                    .font(.title3.weight(.semibold))
                    .fixedSize(horizontal: false, vertical: true)
                Divider()
                StructuredText(markdown: source)
                    .bossMarkdown()
                    .textual.textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(HeightProbe())
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .textSelection(.enabled)
        .withCommentsStub()
    }
}

// MARK: - View-model stub (L4 / L5)

enum ViewerLoadState {
    case loading
    case loaded(title: String, markdown: String)
}

@MainActor
final class ViewerVMStub: ObservableObject {
    @Published var state: ViewerLoadState = .loading
    @Published var renderContentID: UUID? = nil
}

// MARK: - Comments stub (L3 / L4 / L5)

/// Observable stub mirroring the @Published surface of
/// `CommentLayer`. The body of `WithCommentsStub` rebuilds when any of
/// these change, so the rig captures the same rebuild characteristics
/// without installing NSEvent monitors.
@MainActor
final class CommentLayerStub: ObservableObject {
    @Published var comments: [String] = []
    @Published var isShowingPopover: Bool = false
    @Published var pendingQuotedText: String = ""
    @Published var pendingFirstChar: Character? = nil
    @Published var flashingText: String? = nil
}

struct WithCommentsStub: ViewModifier {
    @StateObject private var layer = CommentLayerStub()

    func body(content: Content) -> some View {
        let commentedTexts = layer.comments
        let flashingText = layer.flashingText

        HStack(spacing: 0) {
            content
                .environment(\.stubCommentedTexts, commentedTexts)
                .environment(\.stubCommentFlashText, flashingText)
        }
        .overlay(alignment: .topTrailing) {
            // Match the AddComment button placement; the action is a no-op.
            Button {
                // no-op in the rig
            } label: {
                Label("Add Comment", systemImage: "bubble.left.and.text.bubble.right")
                    .font(.callout)
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .padding(.trailing, 16)
            .padding(.top, 20)
        }
        .background {
            Button("") { /* no-op */ }
                .keyboardShortcut("k", modifiers: [.command, .shift])
                .frame(width: 0, height: 0)
                .hidden()
        }
    }
}

extension View {
    func withCommentsStub() -> some View {
        modifier(WithCommentsStub())
    }
}

private struct StubCommentedTextsKey: EnvironmentKey {
    static var defaultValue: [String] { [] }
}

private struct StubCommentFlashTextKey: EnvironmentKey {
    static var defaultValue: String? { nil }
}

extension EnvironmentValues {
    var stubCommentedTexts: [String] {
        get { self[StubCommentedTextsKey.self] }
        set { self[StubCommentedTextsKey.self] = newValue }
    }
    var stubCommentFlashText: String? {
        get { self[StubCommentFlashTextKey.self] }
        set { self[StubCommentFlashTextKey.self] = newValue }
    }
}
