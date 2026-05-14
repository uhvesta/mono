import AppKit
import Foundation
import SwiftUI
import Textual

/// Source the renderer is rendering. The `project-design-doc-pointer.md`
/// Q9 + chore #12 framing names `case designTask` and `case projectPointer`
/// — only `.projectPointer` is wired up today because the design-task
/// surface (`GetDesignDoc(task_id)` RPC, Approve / Revoke buttons) lands
/// with `design-producing-tasks` Q6. When that ships, the additional case
/// is added here and the view branches on it; the Approve button is
/// rendered for `.designTask` only, satisfying chore #12's
/// "Approve button hidden in project-pointer mode" acceptance.
enum DesignRendererSource: Hashable {
    case projectPointer(projectID: String, resolved: ResolvedDesignDoc)
}

/// Payload handed to the `"design-renderer"` `WindowGroup`. The scene
/// keys windows by this struct so re-clicking the same project's icon
/// brings an existing window forward rather than stacking a duplicate
/// (Hashable + the `WindowGroup(for:)` initializer). `filePath` is
/// already composed (workspacePath + repo-relative `path`) so the view
/// is purely a disk reader.
struct DesignRendererContent: Codable, Hashable {
    /// Title shown in the window's header row — typically the project
    /// name so the user can tell two open renderer windows apart.
    let title: String
    /// Absolute path to the markdown file on disk, inside a leased cube
    /// workspace. Resolved by [[ChatViewModel.openProjectDesignDoc(_:)]]
    /// before the window is opened; the view does not re-resolve.
    let filePath: String
    /// GitHub web URL for the doc. Surfaced as an "Open on GitHub ↗"
    /// affordance and used as the fallback if the on-disk read fails
    /// (file deleted, workspace evicted between resolve and click).
    let webURL: String
    /// `<owner>/<repo>` rendered next to the title so a glance tells
    /// the reader which repo the doc lives in. Empty string when the
    /// caller couldn't derive one.
    let repoLabel: String
    /// Project id and resolved doc kind discriminator. Persisted so a
    /// state-restored window survives a restart without re-querying
    /// the engine. Unused by the project-pointer surface today; lives
    /// on the payload so the future design-task case can carry its
    /// `task_id` alongside.
    let projectID: String

    /// Convenience for tests and the wiring layer in
    /// [[ChatViewModel.openProjectDesignDoc(_:)]] — builds the payload
    /// from a [[ResolvedDesignDoc]] + workspace path. Returns nil when
    /// the resolved kind is `.external` (no workspace path to read
    /// from) so the caller can fall back to the web URL the same way
    /// the existing dispatcher does.
    static func from(
        projectID: String,
        projectName: String,
        resolved: ResolvedDesignDoc,
        workspacePath: String,
        webURL: String
    ) -> DesignRendererContent? {
        switch resolved.kind {
        case .sameProduct, .otherProduct:
            break
        case .external:
            return nil
        }
        let absolute = (workspacePath as NSString)
            .appendingPathComponent(resolved.path)
        return DesignRendererContent(
            title: projectName.isEmpty ? resolved.path : projectName,
            filePath: absolute,
            webURL: webURL,
            repoLabel: repoOwnerSlash(repoURL: resolved.repoRemoteURL),
            projectID: projectID
        )
    }

    /// Lift `<owner>/<repo>` out of a GitHub URL for the header chip.
    /// Mirrors `ProjectDesignDocAffordancePresentation.repoBasename`
    /// so the kanban tooltip and the renderer's header label stay in
    /// sync. Returns the trimmed URL verbatim when nothing parses,
    /// rather than guessing — the caller renders whatever it gets.
    private static func repoOwnerSlash(repoURL: String) -> String {
        if let url = URL(string: repoURL), url.host != nil {
            let parts = url.path
                .split(separator: "/", omittingEmptySubsequences: true)
                .map(String.init)
            if parts.count >= 2 {
                let owner = parts[0]
                let repo = parts[1].hasSuffix(".git")
                    ? String(parts[1].dropLast(4))
                    : parts[1]
                return "\(owner)/\(repo)"
            }
        }
        if let scpRange = repoURL.range(of: ":") {
            let path = String(repoURL[scpRange.upperBound...])
            return path.hasSuffix(".git") ? String(path.dropLast(4)) : path
        }
        return repoURL
    }
}

/// In-app markdown viewer for a project's pointed-at design doc. Reads
/// the file from a leased cube workspace and renders it with the same
/// Textual + Boss style stack the Designs tab uses, so the doc renders
/// identically to that surface (chore #12 acceptance). Read-only:
/// `design-producing-tasks` Q6 owns the Approve / Revoke affordances
/// and lands them on its own case of [[DesignRendererSource]].
struct DesignRendererView: View {
    let content: DesignRendererContent

    @State private var source: String = ""
    @State private var loadError: String?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                header
                Divider()
                body(of: content)
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .textSelection(.enabled)
        .task(id: content.filePath) {
            await load()
        }
    }

    @ViewBuilder
    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(content.title)
                    .font(.title3.weight(.semibold))
                Spacer(minLength: 12)
                if let url = URL(string: content.webURL), !content.webURL.isEmpty {
                    Link(destination: url) {
                        Label("Open on GitHub", systemImage: "arrow.up.right.square")
                            .font(.callout)
                    }
                    .buttonStyle(.link)
                    .accessibilityIdentifier("design-renderer-github-link")
                }
            }
            HStack(spacing: 8) {
                if !content.repoLabel.isEmpty {
                    Text(content.repoLabel)
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                }
                Text(content.filePath)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(content.filePath)
            }
        }
    }

    @ViewBuilder
    private func body(of content: DesignRendererContent) -> some View {
        if let loadError {
            VStack(alignment: .leading, spacing: 8) {
                Text(loadError)
                    .foregroundStyle(.red)
                    .font(.callout)
                if let url = URL(string: content.webURL), !content.webURL.isEmpty {
                    Link("Open on GitHub instead", destination: url)
                        .font(.callout)
                }
            }
        } else {
            StructuredText(
                markdown: source,
                baseURL: URL(fileURLWithPath: content.filePath).deletingLastPathComponent()
            )
            .bossMarkdown()
            .textual.textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func load() async {
        let path = content.filePath
        let result: Result<String, Error> = await Task.detached {
            do {
                let raw = try String(
                    contentsOf: URL(fileURLWithPath: path),
                    encoding: .utf8
                )
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
            self.loadError = "Failed to read \(path): \(error.localizedDescription)"
            self.source = ""
        }
    }
}
