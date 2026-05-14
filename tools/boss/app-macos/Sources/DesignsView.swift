import Foundation
import SwiftUI
import Textual

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
        // Only show the sidebar-toggle when Designs is the active tab. On other
        // tabs (Agents, Work, Engine), the NavigationSplitView's injected toggle
        // would be an orphan since those tabs don't have a sidebar.
        .toolbar(removing: chat.navigationMode != .designs ? .sidebarToggle : nil)
        .task {
            model.refresh(products: chat.activeProducts)
        }
        .onChange(of: chat.activeProducts) { _, newProducts in
            model.refresh(products: newProducts)
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
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
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
struct MarkdownViewerView: View {
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
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

