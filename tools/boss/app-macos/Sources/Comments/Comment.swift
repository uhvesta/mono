import Foundation

/// A single in-memory comment attached to a markdown viewer.
///
/// Phase 1 anchoring is naive: the quoted text is stored verbatim and lost
/// when the viewer window closes. Resilient TextQuoteSelector anchoring
/// lives in Phase 2.
struct Comment: Identifiable, Equatable {
    let id: UUID
    /// The text the user had selected when they created this comment.
    let quotedText: String
    /// The comment body the user typed.
    let body: String
    let createdAt: Date
}
