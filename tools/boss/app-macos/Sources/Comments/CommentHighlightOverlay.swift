import SwiftUI

/// Phase 1 highlight stub. The sidebar makes commented spans visually
/// distinct by quoting the selected text inline; painting background tints
/// on the exact glyph ranges inside the Textual renderer requires the
/// NSLayoutManager bridge that ships in Phase 2. This file holds the
/// architectural placeholder so the module shape is correct from day one.
///
/// Phase 2 will fill this with a Canvas overlay that calls
/// `NSLayoutManager.boundingRect(forGlyphRange:in:)` per comment, using
/// the plain-text ↔ source-range map the renderer will emit.
struct CommentHighlightOverlay: View {
    let comments: [Comment]

    var body: some View {
        // Phase 1: no glyph-level highlights — the sidebar quote snippet
        // carries the visual anchor for now.
        EmptyView()
    }
}
