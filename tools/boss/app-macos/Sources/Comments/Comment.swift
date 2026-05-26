import Foundation

/// Identifies the exact occurrence of a quoted-text span a comment is anchored to.
///
/// Two comments on two different occurrences of the same word have the same
/// `quotedText` but different `occurrenceIndex` values, letting the highlight
/// renderer paint each one independently.
struct CommentAnchor: Equatable {
    /// The verbatim selected text.
    let quotedText: String
    /// 0-based index: which occurrence of `quotedText` in the plain-text
    /// projection this anchor targets.
    let occurrenceIndex: Int
}

/// A single in-memory comment attached to a markdown viewer.
///
/// Phase 1 anchoring is naive: the quoted text and its occurrence index are
/// stored verbatim in memory and lost when the viewer window closes.
/// Resilient TextQuoteSelector anchoring lives in Phase 2.
struct Comment: Identifiable, Equatable {
    let id: UUID
    /// The text the user had selected when they created this comment.
    let quotedText: String
    /// Which occurrence of `quotedText` in the rendered plain text this
    /// comment targets (0-based). Captured at selection time.
    let occurrenceIndex: Int
    /// The comment body the user typed.
    let body: String
    let createdAt: Date

    var anchor: CommentAnchor {
        CommentAnchor(quotedText: quotedText, occurrenceIndex: occurrenceIndex)
    }
}
