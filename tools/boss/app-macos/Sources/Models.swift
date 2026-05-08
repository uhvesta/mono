import Foundation

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
    /// `'human'` (default) when the most recent status change came
    /// from a CLI / app caller; `'engine'` when the engine flipped
    /// the status itself (e.g. dependency auto-block / unblock). The
    /// kanban uses this to distinguish auto-blocks (chain badge,
    /// drag refusal) from user-chosen blocks.
    var lastStatusActor: String = "human"
}

struct WorkTask: Identifiable, Hashable {
    let id: String
    let productID: String
    let projectID: String?
    let kind: String
    var name: String
    var description: String
    var status: String
    var priority: String
    var ordinal: Int?
    var prURL: String?
    var deletedAt: String?
    var createdAt: String
    var updatedAt: String
    /// `'human'` (default) when the most recent status change came
    /// from a CLI / app caller; `'engine'` when the engine flipped
    /// the status itself. The kanban renders the auto-block chain
    /// badge only when this is `'engine'` so manual blocks stay
    /// visually quiet (they already get the lane).
    var lastStatusActor: String = "human"

    var isChore: Bool {
        kind == "chore"
    }
}

/// Canonical priority vocabulary shared by tasks, chores, and
/// projects. Lives in one place so kanban chips, edit pickers, and
/// any future filter UI all speak the same dialect.
enum WorkPriority: String, CaseIterable, Identifiable {
    case low
    case medium
    case high

    var id: String { rawValue }

    /// Tolerant decoder. Pre-priority rows arrive without the field
    /// (older engines, unmigrated DBs, hand-built JSON in tests); we
    /// fall back to `.medium` to match the schema default rather than
    /// surfacing `nil` and forcing every call site to special-case it.
    static func parse(_ raw: String?) -> WorkPriority {
        guard let raw, let value = WorkPriority(rawValue: raw.lowercased()) else {
            return .medium
        }
        return value
    }

    var label: String {
        switch self {
        case .low: return "Low"
        case .medium: return "Medium"
        case .high: return "High"
        }
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
    /// Canonical mapping from engine status → kanban column.
    ///
    /// Tasks/chores carry one of `todo`, `active`, `blocked`,
    /// `in_review`, `done` (plus `archived`, which is filtered out by
    /// `deleted_at`). `blocked` belongs in Backlog: the worker can't
    /// start a gated row, so from the user's perspective it sits with
    /// the not-yet-active pile rather than with Doing. The card itself
    /// surfaces the gating with an icon + "Blocked by …" subtitle.
    var boardColumn: WorkBoardColumnKey {
        switch status {
        case "active":
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

/// One row from `work_item_dependencies` — the dependent is gated by
/// the prerequisite for `relation` ("blocks" today). Carried in the
/// work tree so the kanban can render "Blocked by <prereq title>" on
/// blocked cards without an N+1 round trip.
struct WorkItemDependency: Hashable {
    let dependentID: String
    let prerequisiteID: String
    let relation: String
}

/// Resolved dependency row used by the card detail Dependencies
/// subsection. Joins the raw edge against the product's tasks /
/// chores / projects so the popover can render the prereq's title
/// and current status without re-walking the work tree.
struct WorkDependencyRow: Identifiable, Hashable {
    let id: String
    let title: String
    let status: String
    let kind: WorkDependencyKind

    enum WorkDependencyKind: String, Hashable {
        case task
        case chore
        case project
        case unknown
    }
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
