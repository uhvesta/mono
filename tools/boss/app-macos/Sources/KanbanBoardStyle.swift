import SwiftUI

/// Visual style for the kanban board. Persisted in UserDefaults and
/// switchable from View > Board Style in the menu bar.
///
/// Three distinct takes on reducing "too many vertical lines":
///   - classic:  current appearance (column borders + card borders)
///   - airy:     soft column panels, borderless cards with a drop shadow
///   - minimal:  flat cards, tinted column panels, no borders anywhere
enum KanbanBoardStyle: String, CaseIterable, Identifiable {
    case classic
    case airy
    case minimal

    var id: String { rawValue }

    var displayName: String {
        switch self {
        case .classic: return "Classic"
        case .airy: return "Airy"
        case .minimal: return "Minimal"
        }
    }
}

private struct KanbanBoardStyleKey: EnvironmentKey {
    static let defaultValue = KanbanBoardStyle.classic
}

extension EnvironmentValues {
    var kanbanBoardStyle: KanbanBoardStyle {
        get { self[KanbanBoardStyleKey.self] }
        set { self[KanbanBoardStyleKey.self] = newValue }
    }
}
