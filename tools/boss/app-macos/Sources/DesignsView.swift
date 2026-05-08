import AppKit
import Foundation
import SwiftUI

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
        .task {
            model.refresh(products: chat.products)
        }
        .onChange(of: chat.products) { _, newProducts in
            model.refresh(products: newProducts)
        }
    }

    @ViewBuilder
    private var sidebar: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                if chat.products.isEmpty {
                    Text("No products")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                } else {
                    SidebarProductPicker(
                        selection: Binding(
                            get: { model.selectedProductID ?? chat.products.first?.id },
                            set: { newValue in
                                model.selectProduct(newValue, products: chat.products)
                            }
                        ),
                        products: chat.products
                    )
                    .frame(maxWidth: .infinity)
                }

                Button {
                    model.refresh(products: chat.products)
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .help("Reload markdown files")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)

            Divider()

            if chat.products.isEmpty {
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
    @State private var blocks: [MarkdownBlock] = []

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
                    ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                        MarkdownBlockView(block: block)
                    }
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
            self.blocks = MarkdownParser.parse(text)
        case .failure(let error):
            self.loadError = "Failed to read file: \(error.localizedDescription)"
            self.blocks = []
        }
    }
}

// MARK: - Markdown blocks

enum MarkdownBlock: Hashable {
    case heading(level: Int, text: String)
    case paragraph(text: String)
    case codeBlock(language: String?, lines: [String])
    case unorderedList(items: [String])
    case orderedList(items: [String])
    case blockquote(text: String)
    case rule
    case table(header: [String], rows: [[String]])
}

private struct MarkdownBlockView: View {
    let block: MarkdownBlock

    var body: some View {
        switch block {
        case .heading(let level, let text):
            Text(MarkdownInline.render(text))
                .font(headingFont(level))
                .padding(.top, level <= 2 ? 8 : 4)
        case .paragraph(let text):
            Text(MarkdownInline.render(text))
                .font(.body)
                .fixedSize(horizontal: false, vertical: true)
        case .codeBlock(_, let lines):
            CodeBlockView(lines: lines)
        case .unorderedList(let items):
            VStack(alignment: .leading, spacing: 4) {
                ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text("•").foregroundStyle(.secondary)
                        Text(MarkdownInline.render(item))
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        case .orderedList(let items):
            VStack(alignment: .leading, spacing: 4) {
                ForEach(Array(items.enumerated()), id: \.offset) { idx, item in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text("\(idx + 1).").foregroundStyle(.secondary).frame(minWidth: 18, alignment: .trailing)
                        Text(MarkdownInline.render(item))
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        case .blockquote(let text):
            HStack(alignment: .top, spacing: 10) {
                Rectangle()
                    .fill(Color.accentColor.opacity(0.6))
                    .frame(width: 3)
                Text(MarkdownInline.render(text))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.vertical, 2)
        case .rule:
            Divider().padding(.vertical, 4)
        case .table(let header, let rows):
            MarkdownTableView(header: header, rows: rows)
        }
    }

    private func headingFont(_ level: Int) -> Font {
        switch level {
        case 1: return .system(size: 26, weight: .bold)
        case 2: return .system(size: 22, weight: .semibold)
        case 3: return .system(size: 18, weight: .semibold)
        case 4: return .system(size: 16, weight: .semibold)
        default: return .system(size: 14, weight: .semibold)
        }
    }
}

private struct CodeBlockView: View {
    let lines: [String]

    var body: some View {
        Text(lines.joined(separator: "\n"))
            .font(.system(.callout, design: .monospaced))
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
            .background(
                RoundedRectangle(cornerRadius: 8)
                    .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.18))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 8)
                    .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
            )
    }
}

private struct MarkdownTableView: View {
    let header: [String]
    let rows: [[String]]

    var body: some View {
        let colCount = max(header.count, rows.map(\.count).max() ?? 0)
        VStack(alignment: .leading, spacing: 0) {
            row(cells: header, columns: colCount, isHeader: true)
            Divider()
            ForEach(Array(rows.enumerated()), id: \.offset) { _, cells in
                row(cells: cells, columns: colCount, isHeader: false)
                Divider().opacity(0.4)
            }
        }
        .overlay(
            RoundedRectangle(cornerRadius: 6)
                .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
        )
    }

    @ViewBuilder
    private func row(cells: [String], columns: Int, isHeader: Bool) -> some View {
        HStack(alignment: .top, spacing: 0) {
            ForEach(0..<columns, id: \.self) { col in
                let cell = col < cells.count ? cells[col] : ""
                Text(MarkdownInline.render(cell))
                    .font(isHeader ? .body.weight(.semibold) : .body)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.vertical, 6)
                    .padding(.horizontal, 10)
                if col < columns - 1 {
                    Divider().opacity(0.4)
                }
            }
        }
    }
}

// MARK: - Inline markdown helpers

enum MarkdownInline {
    /// Convert a single markdown source line/paragraph to an
    /// `AttributedString` using Foundation's markdown parser. Falls
    /// back to a plain `AttributedString` if parsing fails (e.g. the
    /// line contains an unbalanced `[` that confuses the parser).
    static func render(_ source: String) -> AttributedString {
        if let attr = try? AttributedString(
            markdown: source,
            options: .init(
                allowsExtendedAttributes: true,
                interpretedSyntax: .inlineOnlyPreservingWhitespace
            )
        ) {
            return attr
        }
        return AttributedString(source)
    }
}

// MARK: - Markdown parser

enum MarkdownParser {
    static func parse(_ text: String) -> [MarkdownBlock] {
        var blocks: [MarkdownBlock] = []
        let lines = text.components(separatedBy: "\n")
        var index = 0

        var paragraphBuffer: [String] = []

        func flushParagraph() {
            guard !paragraphBuffer.isEmpty else { return }
            let joined = paragraphBuffer.joined(separator: " ")
            let trimmed = joined.trimmingCharacters(in: .whitespaces)
            if !trimmed.isEmpty {
                blocks.append(.paragraph(text: trimmed))
            }
            paragraphBuffer.removeAll(keepingCapacity: true)
        }

        while index < lines.count {
            let line = lines[index]
            let trimmed = line.trimmingCharacters(in: .whitespaces)

            if trimmed.isEmpty {
                flushParagraph()
                index += 1
                continue
            }

            if trimmed.hasPrefix("```") || trimmed.hasPrefix("~~~") {
                flushParagraph()
                let fence = String(trimmed.prefix(3))
                let language = trimmed.count > 3
                    ? String(trimmed.dropFirst(3)).trimmingCharacters(in: .whitespaces)
                    : nil
                index += 1
                var codeLines: [String] = []
                while index < lines.count {
                    let codeLine = lines[index]
                    if codeLine.trimmingCharacters(in: .whitespaces).hasPrefix(fence) {
                        index += 1
                        break
                    }
                    codeLines.append(codeLine)
                    index += 1
                }
                blocks.append(.codeBlock(language: language?.isEmpty == true ? nil : language, lines: codeLines))
                continue
            }

            if let level = headingLevel(trimmed) {
                flushParagraph()
                let text = String(trimmed.drop(while: { $0 == "#" })).trimmingCharacters(in: .whitespaces)
                blocks.append(.heading(level: level, text: text))
                index += 1
                continue
            }

            if isHorizontalRule(trimmed) {
                flushParagraph()
                blocks.append(.rule)
                index += 1
                continue
            }

            if trimmed.hasPrefix(">") {
                flushParagraph()
                var quoteLines: [String] = []
                while index < lines.count {
                    let q = lines[index].trimmingCharacters(in: .whitespaces)
                    guard q.hasPrefix(">") else { break }
                    let content = String(q.dropFirst()).trimmingCharacters(in: .whitespaces)
                    quoteLines.append(content)
                    index += 1
                }
                blocks.append(.blockquote(text: quoteLines.joined(separator: " ")))
                continue
            }

            if let bullet = bulletPrefix(trimmed) {
                flushParagraph()
                var items: [String] = []
                items.append(String(trimmed.dropFirst(bullet.count)).trimmingCharacters(in: .whitespaces))
                index += 1
                while index < lines.count {
                    let next = lines[index].trimmingCharacters(in: .whitespaces)
                    if let nb = bulletPrefix(next) {
                        items.append(String(next.dropFirst(nb.count)).trimmingCharacters(in: .whitespaces))
                        index += 1
                    } else {
                        break
                    }
                }
                blocks.append(.unorderedList(items: items))
                continue
            }

            if let (digits, _) = orderedPrefix(trimmed) {
                flushParagraph()
                var items: [String] = []
                items.append(String(trimmed.dropFirst(digits.count + 2)).trimmingCharacters(in: .whitespaces))
                index += 1
                while index < lines.count {
                    let next = lines[index].trimmingCharacters(in: .whitespaces)
                    if let (nd, _) = orderedPrefix(next) {
                        items.append(String(next.dropFirst(nd.count + 2)).trimmingCharacters(in: .whitespaces))
                        index += 1
                    } else {
                        break
                    }
                }
                blocks.append(.orderedList(items: items))
                continue
            }

            if isTableHeader(line: trimmed, next: index + 1 < lines.count ? lines[index + 1].trimmingCharacters(in: .whitespaces) : "") {
                flushParagraph()
                let header = splitTableRow(trimmed)
                index += 2
                var rows: [[String]] = []
                while index < lines.count {
                    let next = lines[index].trimmingCharacters(in: .whitespaces)
                    if next.contains("|") && !next.isEmpty {
                        rows.append(splitTableRow(next))
                        index += 1
                    } else {
                        break
                    }
                }
                blocks.append(.table(header: header, rows: rows))
                continue
            }

            paragraphBuffer.append(trimmed)
            index += 1
        }
        flushParagraph()
        return blocks
    }

    private static func headingLevel(_ trimmed: String) -> Int? {
        guard trimmed.hasPrefix("#") else { return nil }
        var count = 0
        for ch in trimmed {
            if ch == "#" {
                count += 1
            } else {
                break
            }
        }
        guard count >= 1, count <= 6 else { return nil }
        let after = trimmed.dropFirst(count)
        guard after.first == " " else { return nil }
        return count
    }

    private static func isHorizontalRule(_ trimmed: String) -> Bool {
        guard trimmed.count >= 3 else { return false }
        let stripped = trimmed.replacingOccurrences(of: " ", with: "")
        guard stripped.count >= 3 else { return false }
        let chars = Set(stripped)
        return chars == ["-"] || chars == ["*"] || chars == ["_"]
    }

    private static func bulletPrefix(_ trimmed: String) -> String? {
        for prefix in ["- ", "* ", "+ "] {
            if trimmed.hasPrefix(prefix) { return prefix }
        }
        return nil
    }

    private static func orderedPrefix(_ trimmed: String) -> (digits: String, separator: Character)? {
        var digits = ""
        for ch in trimmed {
            if ch.isNumber {
                digits.append(ch)
            } else {
                break
            }
        }
        guard !digits.isEmpty else { return nil }
        let rest = trimmed.dropFirst(digits.count)
        guard let sep = rest.first, sep == "." || sep == ")" else { return nil }
        let afterSep = rest.dropFirst()
        guard afterSep.first == " " else { return nil }
        return (digits, sep)
    }

    /// A markdown table requires a header line followed by a separator
    /// line of `|---|---|` form. Detect both before classifying as a
    /// table — otherwise an ordinary paragraph that happens to contain
    /// `|` would render as a one-row table.
    private static func isTableHeader(line: String, next: String) -> Bool {
        guard line.contains("|"), next.contains("|") else { return false }
        let separatorChars = Set(next.replacingOccurrences(of: " ", with: ""))
        let allowed: Set<Character> = ["|", "-", ":"]
        return !separatorChars.isEmpty && separatorChars.isSubset(of: allowed)
    }

    private static func splitTableRow(_ line: String) -> [String] {
        var trimmed = line
        if trimmed.hasPrefix("|") { trimmed.removeFirst() }
        if trimmed.hasSuffix("|") { trimmed.removeLast() }
        return trimmed
            .components(separatedBy: "|")
            .map { $0.trimmingCharacters(in: .whitespaces) }
    }
}
