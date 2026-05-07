import Foundation

enum AgentRole: String {
    case standard
    case boss

    var title: String {
        switch self {
        case .standard:
            return "Agent"
        case .boss:
            return "Picard"
        }
    }

    var systemImage: String {
        switch self {
        case .standard:
            return "person.circle"
        case .boss:
            return "person.crop.circle.badge.checkmark"
        }
    }
}

struct Agent: Identifiable {
    let id: String
    var name: String
    var role: AgentRole = .standard
    var isReady: Bool = false
    var timeline: [TranscriptItem] = []
    var isSending: Bool = false
    var activeAssistantMessageID: UUID?
    var terminalEntryIndexByID: [String: Int] = [:]

    var isBoss: Bool {
        role == .boss
    }
}

enum ChatRole {
    case user
    case assistant
    case system
}

struct ChatMessage: Identifiable {
    let id: UUID
    let role: ChatRole
    var text: String

    init(id: UUID = UUID(), role: ChatRole, text: String) {
        self.id = id
        self.role = role
        self.text = text
    }
}

struct TerminalActivity: Identifiable {
    let id: String
    var title: String
    var command: String
    var cwd: String?
    var output: String
    var status: String
}

enum TranscriptItem: Identifiable {
    case message(ChatMessage)
    case terminal(TerminalActivity)

    var id: String {
        switch self {
        case .message(let message):
            return "msg-\(message.id.uuidString)"
        case .terminal(let terminal):
            return "terminal-\(terminal.id)"
        }
    }
}

enum NavigationMode: String, CaseIterable, Identifiable {
    case agents = "Agents"
    case work = "Work"

    var id: String { rawValue }
}

struct WorkProduct: Identifiable, Hashable {
    let id: String
    var name: String
    var slug: String
    var description: String
    var repoRemoteURL: String?
    var status: String
    var createdAt: String
    var updatedAt: String
}

struct WorkProject: Identifiable, Hashable {
    let id: String
    let productID: String
    var name: String
    var slug: String
    var description: String
    var goal: String
    var status: String
    var priority: String
    var createdAt: String
    var updatedAt: String
}

struct WorkTask: Identifiable, Hashable {
    let id: String
    let productID: String
    let projectID: String?
    let kind: String
    var name: String
    var description: String
    var status: String
    var ordinal: Int?
    var prURL: String?
    var deletedAt: String?
    var createdAt: String
    var updatedAt: String

    var isChore: Bool {
        kind == "chore"
    }
}

enum WorkNodeID: Hashable {
    case product(String)
    case project(String)
    case task(String)
    case chore(String)
}

enum WorkBoardColumnKey: String, CaseIterable, Identifiable {
    case backlog
    case doing
    case review
    case done

    var id: String { rawValue }

    var title: String {
        switch self {
        case .backlog:
            return "Backlog"
        case .doing:
            return "Doing"
        case .review:
            return "Review"
        case .done:
            return "Done"
        }
    }

    var targetStatus: String {
        switch self {
        case .backlog:
            return "todo"
        case .doing:
            return "active"
        case .review:
            return "in_review"
        case .done:
            return "done"
        }
    }
}

enum WorkBoardGrouping: String, CaseIterable, Identifiable {
    case none
    case project

    var id: String { rawValue }

    var title: String {
        switch self {
        case .none:
            return "Ungrouped"
        case .project:
            return "Project"
        }
    }
}

enum WorkItemPayload {
    case product(WorkProduct)
    case project(WorkProject)
    case task(WorkTask)
    case chore(WorkTask)

    var id: String {
        switch self {
        case .product(let product):
            return product.id
        case .project(let project):
            return project.id
        case .task(let task), .chore(let task):
            return task.id
        }
    }
}

struct WorkSidebarRow: Identifiable {
    let id: WorkNodeID
    let title: String
    let subtitle: String?
    let statusBadge: String?
    let systemImage: String
    let depth: Int
}

enum WorkCreateKind {
    case product
    case project(productID: String)
    case task(productID: String, projectID: String)
    case chore(productID: String)
}

struct WorkCreateRequest: Identifiable {
    let id = UUID()
    let kind: WorkCreateKind
}

struct WorkEditRequest: Identifiable {
    let id = UUID()
    let item: WorkItemPayload
}

struct WorkBoardSection: Identifiable {
    let id: String
    let title: String
    let items: [WorkTask]
    var isCollapsible: Bool = false
    var defaultExpanded: Bool = true
}

extension WorkTask {
    var boardColumn: WorkBoardColumnKey {
        switch status {
        case "active", "blocked":
            return .doing
        case "in_review":
            return .review
        case "done":
            return .done
        default:
            return .backlog
        }
    }
}

struct WorkTaskRuntime: Hashable {
    let workItemID: String
    let executionStatus: String?
    let runStatus: String?
    /// Active or most recent execution id for this work item. Used to
    /// join task → LiveWorkerState (engine registers LiveWorkerState
    /// with `run_id == execution_id`).
    let executionID: String?
}

/// Live runtime state for one allocated worker slot, mirroring the
/// engine's `LiveWorkerState`. Keyed by `slotId` (1..=8) — survives
/// run-record finalisation, which happens within a second of spawn.
struct WorkerLiveState: Hashable {
    let slotId: Int
    let runId: String
    let model: String
    let shellPid: Int32
    let lastEventAt: String?
    let currentTool: String?
    let lastToolEndedAt: String?
    let activity: WorkerActivity
}

enum WorkerActivity: String, Hashable {
    case spawning
    case working
    case waitingForInput = "waiting_for_input"
    case idle
    case errored
    case terminated

    /// Short human-readable label suitable for the pane titlebar pill.
    /// We pair this with the model name (e.g., "claude-opus-4-7") so
    /// the user sees both `Claude Opus 4.7 · Working` rather than
    /// the legacy "Claude Unknown".
    var label: String {
        switch self {
        case .spawning: "Spawning"
        case .working: "Working"
        case .waitingForInput: "Waiting"
        case .idle: "Idle"
        case .errored: "Errored"
        case .terminated: "Terminated"
        }
    }
}

enum AgentActivityState {
    case active
    case waiting(reason: String)
    case errored(reason: String)
    case none

    /// Build from the persisted task runtime status alone. Used as
    /// the fallback when no LiveWorkerState is available (worker
    /// hasn't emitted any hook events yet, or run is no longer
    /// active).
    init(runtime: WorkTaskRuntime?) {
        guard let runtime, let executionStatus = runtime.executionStatus else {
            self = .none
            return
        }
        switch executionStatus {
        case "running":
            if runtime.runStatus == "active" {
                self = .active
            } else {
                self = .waiting(reason: "Run \(runtime.runStatus ?? "in progress")")
            }
        case "ready":
            self = .waiting(reason: "Queued for a worker")
        case "waiting_human":
            self = .waiting(reason: "Waiting on human input")
        case "waiting_review":
            self = .waiting(reason: "Waiting on review")
        case "waiting_merge":
            self = .waiting(reason: "Waiting on merge")
        case "waiting_dependency":
            self = .waiting(reason: "Waiting on dependency")
        case "failed":
            self = .waiting(reason: "Last run failed")
        case "completed":
            self = .none
        default:
            self = .waiting(reason: executionStatus.replacingOccurrences(of: "_", with: " "))
        }
    }

    /// Build by preferring engine-supplied LiveWorkerState (sourced
    /// from hook events) when available, falling back to the
    /// persisted task runtime status. The dot color now distinguishes
    /// working / waiting-on-input / idle / errored rather than always
    /// rendering the same yellow `running` indicator.
    init(runtime: WorkTaskRuntime?, liveState: WorkerLiveState?) {
        if let liveState {
            switch liveState.activity {
            case .working:
                self = .active
            case .waitingForInput:
                self = .waiting(reason: "Waiting on user input")
            case .idle:
                self = .waiting(reason: "Worker idle between turns")
            case .spawning:
                self = .waiting(reason: "Worker spawning")
            case .errored:
                self = .errored(reason: "Worker reported an error")
            case .terminated:
                self = .none
            }
            return
        }
        self.init(runtime: runtime)
    }

    var tooltip: String {
        switch self {
        case .active:
            return "Agent is actively working"
        case .waiting(let reason):
            return reason
        case .errored(let reason):
            return reason
        case .none:
            return "No agent attached"
        }
    }
}
