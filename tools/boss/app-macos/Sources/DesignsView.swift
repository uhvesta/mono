import Foundation
import os
import SwiftUI
import Textual

private let designDocTimingLog = Logger(subsystem: "com.boss.app", category: "DesignDocTiming")

// MARK: - Repo path resolution

enum DesignRepoPath {
    /// Derive the repo basename from a git remote URL. Handles:
    ///   git@github.com:spinyfin/mono.git
    ///   https://github.com/spinyfin/mono.git
    ///   https://github.com/spinyfin/mono
    static func basename(from remoteURL: String) -> String? {
        var trimmed = remoteURL.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        if trimmed.hasSuffix(".git") {
            trimmed = String(trimmed.dropLast(4))
        }
        let parts = trimmed.split(whereSeparator: { $0 == "/" || $0 == ":" })
        guard let last = parts.last else { return nil }
        let name = String(last)
        return name.isEmpty ? nil : name
    }

    /// Canonical local clone path for the given remote URL, rooted at
    /// `~/Documents/dev/<basename>`. Returns nil if the URL is empty or
    /// can't be parsed into a basename.
    static func localCloneURL(for remoteURL: String) -> URL? {
        guard let name = basename(from: remoteURL) else { return nil }
        let home = FileManager.default.homeDirectoryForCurrentUser
        return home.appendingPathComponent("Documents/dev/\(name)", isDirectory: true)
    }
}

// MARK: - File enumeration

struct DesignFileNode: Identifiable, Hashable {
    let id: String
    let name: String
    let url: URL
    let isDirectory: Bool
    var children: [DesignFileNode]?

    var hasChildren: Bool {
        (children?.isEmpty == false)
    }
}

enum DesignFileLoader {
    /// Directory names skipped when walking a repo. These are
    /// build/dependency outputs; recursing into them is just noise and
    /// can be very slow (node_modules, target).
    static let skippedDirectoryNames: Set<String> = [
        "node_modules",
        "target",
        ".git",
        "build",
        "dist",
        ".next",
    ]

    /// Directory name prefixes skipped when walking a repo. Bazel emits
    /// `bazel-bin`, `bazel-out`, `bazel-testlogs`, `bazel-<workspace>`
    /// symlinks at the repo root — match by prefix so we don't have to
    /// hardcode the workspace name.
    static let skippedDirectoryPrefixes: [String] = ["bazel-"]

    static func shouldSkip(directoryName name: String) -> Bool {
        if skippedDirectoryNames.contains(name) {
            return true
        }
        for prefix in skippedDirectoryPrefixes {
            if name.hasPrefix(prefix) {
                return true
            }
        }
        return false
    }

    /// Recursively enumerate `.md` files under `root`, returning a tree
    /// of directories and files. Empty directories (no markdown
    /// descendants) are pruned so the sidebar doesn't fill with noise.
    static func loadTree(at root: URL) -> [DesignFileNode] {
        guard let nodes = enumerate(directory: root) else { return [] }
        return nodes
    }

    private static func enumerate(directory: URL) -> [DesignFileNode]? {
        let fm = FileManager.default
        let contents: [URL]
        do {
            contents = try fm.contentsOfDirectory(
                at: directory,
                includingPropertiesForKeys: [.isDirectoryKey],
                options: [.skipsHiddenFiles]
            )
        } catch {
            return nil
        }

        var directoryNodes: [DesignFileNode] = []
        var fileNodes: [DesignFileNode] = []

        for url in contents {
            let isDir = (try? url.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) ?? false
            let name = url.lastPathComponent
            if isDir {
                if shouldSkip(directoryName: name) { continue }
                if let children = enumerate(directory: url), !children.isEmpty {
                    directoryNodes.append(
                        DesignFileNode(
                            id: url.path,
                            name: name,
                            url: url,
                            isDirectory: true,
                            children: children
                        )
                    )
                }
            } else {
                guard url.pathExtension.lowercased() == "md" else { continue }
                fileNodes.append(
                    DesignFileNode(
                        id: url.path,
                        name: name,
                        url: url,
                        isDirectory: false,
                        children: nil
                    )
                )
            }
        }

        directoryNodes.sort { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
        fileNodes.sort { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
        let combined = directoryNodes + fileNodes
        return combined.isEmpty ? nil : combined
    }
}

// MARK: - View model

@MainActor
final class DesignsViewModel: ObservableObject {
    @Published var selectedProductID: String?
    @Published var fileTree: [DesignFileNode] = []
    @Published var selectedFile: DesignFileNode?
    @Published var resolvedRoot: URL?
    @Published var rootExists: Bool = false

    private let defaults = UserDefaults.standard
    private let selectedProductDefaultsKey = "boss.designs.selectedProductID"

    init() {
        selectedProductID = defaults.string(forKey: selectedProductDefaultsKey)
    }

    func selectProduct(_ productID: String?, products: [WorkProduct]) {
        selectedProductID = productID
        if let productID {
            defaults.set(productID, forKey: selectedProductDefaultsKey)
        }
        selectedFile = nil
        refresh(products: products)
    }

    /// Resolve the local clone path for the currently selected product
    /// and reload the file tree. If the local clone is missing,
    /// `rootExists` flips to false and the detail pane shows an empty
    /// state with the resolved path — we don't try to clone anything.
    func refresh(products: [WorkProduct]) {
        guard let product = products.first(where: { $0.id == selectedProductID })
            ?? products.first
        else {
            resolvedRoot = nil
            rootExists = false
            fileTree = []
            return
        }
        if selectedProductID == nil {
            selectedProductID = product.id
        }

        guard let remote = product.repoRemoteURL,
              let root = DesignRepoPath.localCloneURL(for: remote) else {
            resolvedRoot = nil
            rootExists = false
            fileTree = []
            return
        }

        resolvedRoot = root

        var isDir: ObjCBool = false
        let exists = FileManager.default.fileExists(atPath: root.path, isDirectory: &isDir) && isDir.boolValue
        rootExists = exists
        if exists {
            fileTree = DesignFileLoader.loadTree(at: root)
        } else {
            fileTree = []
        }
    }
}

// MARK: - Top-level view

struct DesignsView: View {
    @ObservedObject var chat: ChatViewModel
    @StateObject private var model = DesignsViewModel()

    var body: some View {
        NavigationSplitView {
            sidebar
                .navigationSplitViewColumnWidth(min: 240, ideal: 320, max: 460)
        } detail: {
            detail
                .background(Color(nsColor: .windowBackgroundColor))
        }
        // Re-resolve the design tree whenever the active product set
        // changes. A `.task(id:)` runs the refresh after the render
        // commits — keying it on `activeProducts` replaces the old
        // `.task` + `.onChange` pair, whose `.onChange` mutated the
        // view model's @Published state synchronously during the update
        // ChatViewModel triggered ("Publishing changes from within view
        // updates").
        .task(id: chat.activeProducts) {
            model.refresh(products: chat.activeProducts)
        }
    }

    @ViewBuilder
    private var sidebar: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                if chat.activeProducts.isEmpty {
                    Text("No products")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                } else {
                    SidebarProductPicker(
                        selection: Binding(
                            get: { model.selectedProductID ?? chat.activeProducts.first?.id },
                            set: { newValue in
                                model.selectProduct(newValue, products: chat.activeProducts)
                            }
                        ),
                        products: chat.activeProducts
                    )
                    .frame(maxWidth: .infinity)
                }

                Button {
                    model.refresh(products: chat.activeProducts)
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .help("Reload markdown files")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)

            Divider()

            if chat.activeProducts.isEmpty {
                Spacer()
                Text("Create a product to browse its design docs.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(20)
                Spacer()
            } else if !model.rootExists {
                emptyState
            } else if model.fileTree.isEmpty {
                Spacer()
                Text("No markdown files found.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Spacer()
            } else {
                List(selection: Binding(
                    get: { model.selectedFile?.id },
                    set: { newID in
                        if let newID, let node = findNode(id: newID, in: model.fileTree),
                           !node.isDirectory {
                            model.selectedFile = node
                        }
                    }
                )) {
                    OutlineGroup(model.fileTree, children: \.children) { node in
                        DesignFileRow(node: node)
                            .tag(node.id)
                    }
                }
                .listStyle(.sidebar)
            }
        }
    }

    @ViewBuilder
    private var emptyState: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Local clone not found")
                .font(.callout.weight(.semibold))
            if let root = model.resolvedRoot {
                Text("Expected at:")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Text(root.path)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                Text("This product has no `repo_remote_url` configured.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(20)
    }

    @ViewBuilder
    private var detail: some View {
        if let file = model.selectedFile {
            MarkdownDocumentView(fileURL: file.url)
                .id(file.id)
        } else {
            VStack(alignment: .center, spacing: 8) {
                Image(systemName: "doc.text")
                    .font(.system(size: 28, weight: .light))
                    .foregroundStyle(.secondary)
                Text("Select a markdown file")
                    .font(.title3)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func findNode(id: String, in nodes: [DesignFileNode]) -> DesignFileNode? {
        for node in nodes {
            if node.id == id { return node }
            if let children = node.children, let hit = findNode(id: id, in: children) {
                return hit
            }
        }
        return nil
    }
}

private struct DesignFileRow: View {
    let node: DesignFileNode

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: node.isDirectory ? "folder" : "doc.text")
                .foregroundStyle(node.isDirectory ? .secondary : .primary)
                .font(.system(size: 12))
                .frame(width: 14)
            Text(node.name)
                .font(.body)
                .lineLimit(1)
                .truncationMode(.middle)
        }
    }
}

// MARK: - Markdown rendering

private struct MarkdownDocumentView: View {
    let fileURL: URL

    @State private var loadError: String?
    @State private var source: String = ""

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                HStack(alignment: .firstTextBaseline) {
                    Text(fileURL.lastPathComponent)
                        .font(.title3.weight(.semibold))
                    Spacer()
                    Text(fileURL.path)
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help(fileURL.path)
                }
                Divider()

                if let loadError {
                    Text(loadError)
                        .foregroundStyle(.red)
                        .font(.callout)
                } else {
                    StructuredText(
                        markdown: source,
                        baseURL: fileURL.deletingLastPathComponent()
                    )
                    .bossMarkdown()
                    .textual.textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .textSelection(.enabled)
        .task(id: fileURL) {
            await load()
        }
    }

    private func load() async {
        let url = fileURL
        let result: Result<String, Error> = await Task.detached {
            do {
                let raw = try String(contentsOf: url, encoding: .utf8)
                return .success(raw)
            } catch {
                return .failure(error)
            }
        }.value

        switch result {
        case .success(let text):
            self.loadError = nil
            self.source = text
        case .failure(let error):
            self.loadError = "Failed to read file: \(error.localizedDescription)"
            self.source = ""
        }
    }
}

/// Payload passed to the `"markdown-viewer"` WindowGroup scene via
/// `openWindow(id:value:)`. Codable for state restoration; Hashable so
/// macOS can track one window per unique title+markdown pair.
struct MarkdownViewerContent: Codable, Hashable {
    let title: String
    let markdown: String
}

/// Stand-alone scrolling viewer for long task / chore descriptions.
/// Rendered inside the `"markdown-viewer"` WindowGroup scene. The
/// chrome matches `MarkdownDocumentView` so the "Read full description"
/// affordance lands in a layout that visually mirrors the Designs file
/// viewer.
///
/// The view is split into an outer wrapper that applies `.withComments()` and
/// an inner `MarkdownViewerContent` that reads the comment-environment values
/// injected by `WithCommentsModifier` and feeds them to `HighlightingMarkdownParser`.
struct MarkdownViewerView: View {
    let title: String
    let source: String
    /// Project short-ID for timing logs. Empty string when called outside
    /// the async-markdown-viewer context (e.g. tests, design-doc browser).
    var projectShortID: String = ""
    /// Wall-clock time of the user's click that triggered this open, for
    /// the `phase=interactive` total. Nil outside the async-markdown-viewer
    /// flow (e.g. design-doc browser) — interactive is only meaningful for
    /// the click-to-first-paint user journey.
    var clickStartTime: Date? = nil

    var body: some View {
        MarkdownViewerScrollContent(
            title: title,
            source: source,
            projectShortID: projectShortID,
            clickStartTime: clickStartTime
        )
        .withComments()
    }
}

/// Preference key used to detect when `StructuredText` has been laid out
/// for the first time, signalling that Textual has completed parsing.
private struct StructuredTextHeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}

/// Inner content view that reads comment state from the environment and uses
/// HighlightingMarkdownParser to paint persistent yellow highlights on commented spans.
private struct MarkdownViewerScrollContent: View {
    let title: String
    let source: String
    let projectShortID: String
    /// Click→first-paint anchor passed in from `MarkdownViewerView`. When
    /// non-nil and layout completes, we additionally emit `phase=interactive`
    /// so the unified log carries a single end-to-end number alongside the
    /// per-stage spans.
    let clickStartTime: Date?

    @Environment(\.commentedAnchors) private var commentedAnchors
    @Environment(\.commentFlashAnchor) private var commentFlashAnchor
    @State private var parseStartTime: Date? = nil
    @State private var parseLogged = false
    /// Monotonically-increasing counter bumped whenever the highlight state
    /// changes. Used as the `.id()` for `StructuredText` to force a fresh
    /// parse when comments are added/removed or the flash text changes.
    /// A counter avoids hash collisions that can occur with XOR-combined
    /// hashValues and guarantees identity changes on every highlight update.
    @State private var parseVersion: Int = 0

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text(title)
                    .font(.title3.weight(.semibold))
                    .fixedSize(horizontal: false, vertical: true)
                Divider()
                StructuredText(source, parser: markdownParser)
                    .bossMarkdown()
                    .textual.textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    // Force StructuredText recreation when highlight state changes so the
                    // new HighlightingMarkdownParser instance is used to re-parse the source.
                    // StructuredText only re-parses on markup changes; the id() change is the
                    // trigger that ensures highlight updates are reflected immediately.
                    // A monotonic counter is used instead of a hashValue-based key to avoid
                    // hash collisions and guarantee a new identity on every comment update.
                    .id(parseVersion)
                    .onChange(of: commentedAnchors) { _, _ in parseVersion &+= 1 }
                    .onChange(of: commentFlashAnchor) { _, _ in parseVersion &+= 1 }
                    .background(
                        GeometryReader { geo in
                            Color.clear.preference(
                                key: StructuredTextHeightKey.self,
                                value: geo.size.height
                            )
                        }
                    )
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .textSelection(.enabled)
        .onAppear {
            parseStartTime = Date()
            parseLogged = false
        }
        .onPreferenceChange(StructuredTextHeightKey.self) { height in
            guard !parseLogged, height > 0, let start = parseStartTime,
                  !projectShortID.isEmpty else { return }
            let ms = Int(Date().timeIntervalSince(start) * 1000)
            let bytes = source.utf8.count
            designDocTimingLog.info("phase=parse project=\(projectShortID, privacy: .public) duration_ms=\(ms, privacy: .public) bytes=\(bytes, privacy: .public)")
            if let clickStart = clickStartTime {
                let totalMs = Int(Date().timeIntervalSince(clickStart) * 1000)
                designDocTimingLog.info("phase=interactive project=\(projectShortID, privacy: .public) duration_ms=\(totalMs, privacy: .public)")
            }
            DispatchQueue.main.async {
                parseLogged = true
                parseStartTime = nil
            }
        }
    }

    private var markdownParser: any MarkupParser {
        if commentedAnchors.isEmpty && commentFlashAnchor == nil {
            return AttributedStringMarkdownParser.markdown()
        }
        return HighlightingMarkdownParser(
            highlightedAnchors: commentedAnchors,
            flashingAnchor: commentFlashAnchor
        )
    }
}

// MARK: - Window menu registration

/// Zero-size `NSView` that, when inserted into a SwiftUI view hierarchy,
/// accesses its hosting `NSWindow` and sets `isExcludedFromWindowsMenu =
/// false`. SwiftUI's `Window` scene (single-instance utility windows)
/// opts windows OUT of the auto-managed Window menu by default; inserting
/// this view in the content tree reverses that so the window appears as a
/// named, titled entry at the bottom of the menu — matching the behaviour
/// of `WindowGroup`-backed windows.
///
/// The exclusion flag is re-applied in `updateNSView` (called on every
/// SwiftUI layout pass) so it survives any NSWindow re-configuration
/// SwiftUI performs internally. A deferred `DispatchQueue.main.async` is
/// used in `makeNSView` because the view is not yet attached to a window
/// at the point `makeNSView` is called; one runloop tick later it is.
private struct WindowMenuRegistrar: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async {
            view.window?.isExcludedFromWindowsMenu = false
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        nsView.window?.isExcludedFromWindowsMenu = false
    }
}

private extension View {
    /// Ensures the hosting NSWindow appears in the macOS Window menu.
    ///
    /// Apply to any view inside a SwiftUI `Window` scene whose window
    /// should be navigable from the per-window list at the bottom of the
    /// Window menu. (`WindowGroup`-backed windows are already registered
    /// automatically; this modifier is only needed for `Window` scenes.)
    func registeredInWindowMenu() -> some View {
        background(WindowMenuRegistrar().frame(width: 0, height: 0))
    }
}

/// Loading state for the `"async-markdown-viewer"` Window scene, which
/// opens immediately on click and resolves content asynchronously.
enum MarkdownDocLoadState {
    case loading
    case loaded(title: String, markdown: String)
    case failed(title: String, message: String)
}

/// Shared observable model for the `"async-markdown-viewer"` Window
/// scene. Owned by [[ChatViewModel]] and injected via EnvironmentObject
/// so the window can observe state transitions without content having to
/// pass through the `openWindow` value type (which can't be updated
/// after the window opens).
@MainActor
final class AsyncMarkdownViewerViewModel: ObservableObject {
    @Published var state: MarkdownDocLoadState = .loading
    /// Set by the fetch path just before transitioning to `.loaded` so the
    /// render-complete log entry can report the full parse+layout duration.
    var renderStartTime: Date? = nil
    var pendingRenderProjectShortID: String? = nil
    /// Stamped alongside `renderStartTime`; applied as `.id()` to
    /// `MarkdownViewerView` so SwiftUI recreates the view on each content
    /// load, ensuring `.onAppear` fires even when the window is reused.
    var renderContentID: UUID? = nil
    /// Wall-clock time `openProjectDesignDoc` first dispatched the
    /// rawContentURL path for this click. Read by
    /// `MarkdownViewerScrollContent` to emit a single
    /// `phase=interactive` line covering the full click→first-paint
    /// budget. Each click overwrites it, and the inner content's
    /// `parseLogged` flag guards against double-emission on a single
    /// content load — so we don't need to null it out after consumption.
    var clickStartTime: Date? = nil
}

/// Content view for the `"async-markdown-viewer"` Window scene. Shows a
/// spinner while [[ChatViewModel.asyncMarkdownViewerVM]] is in the
/// `.loading` state, swaps to the rendered markdown when `.loaded`, and
/// shows an error affordance when `.failed` — matching the browser-tab
/// model of open-immediately, then fill.
struct AsyncMarkdownViewerView: View {
    // Observe the viewer view-model *directly* rather than reaching it through
    // `chatModel`. The window previously declared only `@EnvironmentObject
    // chatModel` and read `chatModel.asyncMarkdownViewerVM.state`; because
    // `asyncMarkdownViewerVM` is a nested ObservableObject that `chatModel`
    // does not republish, a `.loading -> .loaded` transition was *not*
    // observed here — the loaded view only mounted on the next incidental
    // `chatModel` publish (an engine event). Under main-thread contention that
    // gap stretched to tens of seconds, which is exactly the window
    // `phase=render` measures. Observing the VM directly mounts the loaded
    // view the instant the state flips, independent of `chatModel`'s publish
    // timing. (See `tools/boss/experiments/textual-perf-layered` L10 for the
    // measured buggy-vs-fixed mount-latency contrast.)
    @EnvironmentObject private var vm: AsyncMarkdownViewerViewModel

    var body: some View {
        // Wrap in Group so `.registeredInWindowMenu()` is applied once
        // at the top level rather than inside each case branch. The
        // modifier inserts a zero-size NSViewRepresentable that marks the
        // hosting NSWindow as included in the Window menu — necessary
        // because SwiftUI's `Window` scene (unlike `WindowGroup`) sets
        // `isExcludedFromWindowsMenu = true` on its NSWindow by default.
        Group {
            switch vm.state {
            case .loading:
                ProgressView("Loading…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .loaded(let title, let markdown):
                MarkdownViewerView(
                    title: title,
                    source: markdown,
                    projectShortID: vm.pendingRenderProjectShortID ?? "",
                    clickStartTime: vm.clickStartTime
                )
                // .id() forces SwiftUI to destroy and recreate MarkdownViewerView on each
                // content load, so .onAppear fires even when the window is reused across
                // documents (stable case identity would otherwise suppress it).
                .id(vm.renderContentID)
                .navigationTitle(title)
                .onAppear {
                    if let start = vm.renderStartTime,
                       let shortID = vm.pendingRenderProjectShortID {
                        let ms = Int(Date().timeIntervalSince(start) * 1000)
                        designDocTimingLog.info("phase=render project=\(shortID, privacy: .public) duration_ms=\(ms, privacy: .public)")
                        vm.renderStartTime = nil
                        vm.pendingRenderProjectShortID = nil
                    }
                    // clickStartTime is consumed by MarkdownViewerScrollContent's
                    // layout-complete handler. It is not cleared here on purpose —
                    // SwiftUI may rebuild AsyncMarkdownViewerView before layout
                    // completes, and the next click re-stamps it.
                }
            case .failed(let title, let message):
                VStack(spacing: 16) {
                    Image(systemName: "exclamationmark.triangle")
                        .font(.largeTitle)
                        .foregroundStyle(.orange)
                    Text("Failed to load \u{201C}\(title)\u{201D}")
                        .font(.headline)
                    Text(message)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                }
                .padding()
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .navigationTitle(title)
            }
        }
        .registeredInWindowMenu()
    }
}

