import AppKit
import Foundation
import SwiftUI
import Textual

// Adds a GeometryReader to measure height and propagate StructuredTextHeightKey
// preference up to the parent LayerPane. Using overlay ensures the preference
// properly bubbles up the view hierarchy, unlike .background().
private extension View {
    func publishHeight() -> some View {
        self.overlay(alignment: .topLeading) {
            GeometryReader { geo in
                Color.clear.preference(
                    key: StructuredTextHeightKey.self,
                    value: geo.size.height
                )
            }
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
                .publishHeight()
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
                .publishHeight()
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
                    .publishHeight()
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
                    .publishHeight()
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

// MARK: - L6–L9 stubs

// MARK: ChatViewModelStub (L6+)

/// Passive stub mirroring ChatViewModel's @Published surface. Has ~20
/// @Published vars covering the main structural properties that views
/// read from the injected chatModel EnvironmentObject in production.
/// No timer — this is the "dead environment" baseline for L6.
///
/// Mirrors: tools/boss/app-macos/Sources/ChatViewModel.swift
@MainActor
final class ChatViewModelStub: ObservableObject {
    @Published var navigationMode: Int = 0
    @Published var isConnected: Bool = true
    @Published var hasConnectedOnce: Bool = true
    @Published var products: [String] = []
    @Published var selectedWorkProductID: String? = nil
    @Published var selectedProjectFilterIDs: Set<String> = []
    @Published var includeChores: Bool = true
    @Published var showBlockedOnly: Bool = false
    @Published var showArchivedProjects: Bool = false
    @Published var selectedWorkCardID: String? = nil
    @Published var revealHighlightID: String? = nil
    @Published var pendingWorkCreateRequest: Bool = false
    @Published var pendingWorkEditRequest: Bool = false
    @Published var workErrorMessage: String? = nil
    @Published var workSearchText: String = ""
    @Published var isBossPanelCollapsed: Bool = false
    @Published var bossPanelWidth: CGFloat = 380
    @Published var liveStatusDisabledSlotIDs: Set<Int> = []
    @Published var engineHealthIssues: [String] = []
    @Published var featureFlags: [String] = []
}

// MARK: SiblingPublisherStub (L7+)

/// Publishes objectWillChange on a ~500 ms timer. Mirrors the combined
/// publish cadence of production's kanban view-models, live-status pollers,
/// and engine-subscription Combine graph that fire while the design-doc
/// window is open. Start/stop are idempotent and safe to call repeatedly.
@MainActor
final class SiblingPublisherStub: ObservableObject {
    @Published var tickCount: Int = 0

    private var publishTask: Task<Void, Never>?

    func start() {
        guard publishTask == nil else { return }
        publishTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(500))
                self?.tickCount += 1
            }
        }
    }

    func stop() {
        publishTask?.cancel()
        publishTask = nil
    }
}

// MARK: ExtraViewModelStub (L9)

/// Additional ObservableObject stubs mirroring ContentView's other
/// @StateObject instances: WorkersWorkspaceModel and BossPaneModel in
/// production. Each fires objectWillChange on its own timer so L9 has
/// multiple independent publish sources — the full multi-publisher
/// load present in the production scene tree.
@MainActor
final class ExtraViewModelStub: ObservableObject {
    @Published var workerCount: Int = 0
    @Published var bossIsExpanded: Bool = true
    @Published var bossWidth: CGFloat = 380

    private var publishTask: Task<Void, Never>?

    func start() {
        guard publishTask == nil else { return }
        publishTask = Task { [weak self] in
            var tick = 0
            while !Task.isCancelled {
                // Workers model fires every ~700 ms; Boss pane fires every ~1 s.
                // Interleave at ~350 ms to approximate combined cadence.
                try? await Task.sleep(for: .milliseconds(350))
                tick += 1
                if tick % 2 == 0 { self?.workerCount = tick / 2 }
                else { self?.bossIsExpanded = tick.isMultiple(of: 3) }
            }
        }
    }

    func stop() {
        publishTask?.cancel()
        publishTask = nil
    }
}

// MARK: EventMonitorManager (L8+)

/// Manages local NSEvent monitors matching CommentLayer.installMonitors().
/// Three monitors: keyDown, rightMouseDown, leftMouseUp — all pass events
/// through unchanged. Stored nonisolated(unsafe) matching CommentLayer's
/// own pattern. Removed in deinit as a safety net; callers should also
/// call remove() on disappear to avoid any cross-layer leakage.
final class EventMonitorManager {
    nonisolated(unsafe) private var keyMonitor: Any?
    nonisolated(unsafe) private var rightClickMonitor: Any?
    nonisolated(unsafe) private var mouseUpMonitor: Any?

    func install() {
        guard keyMonitor == nil else { return }
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { $0 }
        rightClickMonitor = NSEvent.addLocalMonitorForEvents(matching: .rightMouseDown) { $0 }
        mouseUpMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseUp) { $0 }
    }

    func remove() {
        [keyMonitor, rightClickMonitor, mouseUpMonitor].compactMap { $0 }.forEach {
            NSEvent.removeMonitor($0)
        }
        keyMonitor = nil
        rightClickMonitor = nil
        mouseUpMonitor = nil
    }

    deinit { remove() }
}

// MARK: - L6 · + WindowGroup EnvironmentObject

/// Adds a passive ChatViewModelStub as an @EnvironmentObject — mirroring
/// production's async-markdown-viewer Window scene receiving chatModel
/// via .environmentObject(chatModel) from BossMacApp. Reading
/// `envStub.isConnected` in body ensures SwiftUI creates the subscription;
/// since the stub never publishes, any difference from L5 isolates the
/// cost of the EnvironmentObject subscription graph itself.
struct L6_WindowGroupEnvObj: View {
    let source: String
    @EnvironmentObject private var envStub: ChatViewModelStub

    var body: some View {
        // Read one property so SwiftUI creates the subscription to
        // envStub.objectWillChange. The value is unused visually.
        let _ = envStub.isConnected
        L5_BossAsyncFetch(source: source)
    }
}

// MARK: - L7 · + sibling publisher

/// Adds a SiblingPublisherStub that fires objectWillChange every ~500 ms
/// while this layer is visible — mirroring the kanban view-models and
/// live-status pollers that publish alongside the design-doc Window in
/// production. L7 = L6 + active sibling publishing. The `tickCount`
/// read in body ensures SwiftUI re-evaluates this view on every tick.
struct L7_SiblingPublisher: View {
    let source: String
    @EnvironmentObject private var envStub: ChatViewModelStub
    @EnvironmentObject private var sibling: SiblingPublisherStub

    var body: some View {
        let _ = envStub.isConnected
        let _ = sibling.tickCount
        L5_BossAsyncFetch(source: source)
            .onAppear { sibling.start() }
            .onDisappear { sibling.stop() }
    }
}

// MARK: - L8 · + NSEvent monitors

/// Adds the three local NSEvent monitors that production's CommentLayer
/// installs on appear (keyDown, rightMouseDown, leftMouseUp). L8 = L7 +
/// active monitors. Handlers are pass-through no-ops. Monitors are
/// unregistered on disappear via EventMonitorManager.remove() and its
/// deinit safety net, so no monitor leaks across layer switches.
struct L8_EventMonitor: View {
    let source: String
    @EnvironmentObject private var envStub: ChatViewModelStub
    @EnvironmentObject private var sibling: SiblingPublisherStub

    // Class-based manager so monitor tokens survive body re-evaluations.
    @StateObject private var monitors = _EventMonitorHost()

    var body: some View {
        let _ = envStub.isConnected
        let _ = sibling.tickCount
        L5_BossAsyncFetch(source: source)
            .onAppear {
                sibling.start()
                monitors.manager.install()
            }
            .onDisappear {
                sibling.stop()
                monitors.manager.remove()
            }
    }
}

/// ObservableObject wrapper around EventMonitorManager so it can be
/// stored as a @StateObject (EventMonitorManager is not Observable).
@MainActor
private final class _EventMonitorHost: ObservableObject {
    let manager = EventMonitorManager()
}

// MARK: - L9 · Full production scaffold

/// Adds ExtraViewModelStub (mirroring WorkersWorkspaceModel + BossPaneModel)
/// on top of L8 — completing the set of simultaneously-active observables
/// present in production while the design-doc window renders. L9 = L8 +
/// extra publishers at a ~350 ms cadence. If L9 reproduces but L8 does
/// not, the combined publish load (not any single publisher) is the cause.
struct L9_FullScaffold: View {
    let source: String
    @EnvironmentObject private var envStub: ChatViewModelStub
    @EnvironmentObject private var sibling: SiblingPublisherStub
    @EnvironmentObject private var extra: ExtraViewModelStub

    @StateObject private var monitors = _EventMonitorHost()

    var body: some View {
        let _ = envStub.isConnected
        let _ = sibling.tickCount
        let _ = extra.workerCount
        L5_BossAsyncFetch(source: source)
            .onAppear {
                sibling.start()
                extra.start()
                monitors.manager.install()
            }
            .onDisappear {
                sibling.stop()
                extra.stop()
                monitors.manager.remove()
            }
    }
}
