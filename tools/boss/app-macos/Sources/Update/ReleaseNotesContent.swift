import SwiftUI
import Textual
import UpdateCore

/// The version-grouped, markdown-rendered release-notes body shared by the full
/// update sheet (``UpdateResultSheet``) and the compact toolbar badge popover
/// (`UpdateBadgePopover`).
///
/// Both surfaces render the *same* data through the *same* renderer:
/// - **Releases since installed:** `changelog` is `AvailableUpdate.changelog`,
///   the cumulative notes for every release newer than the installed version,
///   aggregated once in `UpdateChecker`. When it is empty (e.g. an older check
///   result), the single-version `fallbackNotes` is shown instead.
/// - **Markdown rendering:** notes go through `StructuredText` + `bossMarkdown()`
///   so nothing leaks raw `#`/`-`/`**`/link source.
///
/// The caller supplies the surrounding `ScrollView` / chrome, since the sheet and
/// the popover present this body at different sizes.
struct ReleaseNotesContent: View {
    let changelog: [ReleaseNote]
    /// Single-version fallback when `changelog` is empty.
    let fallbackNotes: String

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if !changelog.isEmpty {
                ForEach(changelog, id: \.version.description) { note in
                    versionBlock(note)
                }
            } else if !fallbackNotes.isEmpty {
                StructuredText(markdown: fallbackNotes)
                    .bossMarkdown()
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    @ViewBuilder
    private func versionBlock(_ note: ReleaseNote) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Text("Version \(note.version.description)")
                    .font(.subheadline.weight(.semibold))
                if let date = note.publishedAt {
                    Text("·")
                        .foregroundStyle(.secondary)
                    Text(date, format: .dateTime.month(.abbreviated).day().year())
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
            }
            if note.notes.isEmpty {
                Text("No release notes.")
                    .font(.callout)
                    .foregroundStyle(.tertiary)
                    .italic()
            } else {
                StructuredText(markdown: note.notes)
                    .bossMarkdown()
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}
