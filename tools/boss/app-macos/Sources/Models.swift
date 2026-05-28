import Foundation

enum NavigationMode: String, CaseIterable, Identifiable {
    case agents = "Agents"
    case work = "Work"
    case designs = "Designs"

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
    /// Discriminator for the bound external tracker (`"github"`, etc.).
    /// `nil` when no tracker is bound. Mirrors `Product.external_tracker_kind`.
    var externalTrackerKind: String? = nil
    /// Kind-specific tracker config as raw JSON string. `nil` when no
    /// tracker is bound. Mirrors `Product.external_tracker_config`.
    var externalTrackerConfig: String? = nil
    /// Optional leading prefix for worker branch names. `nil` → engine
    /// default `"boss/"`. Mirrors `Product.worker_branch_prefix`.
    var workerBranchPrefix: String? = nil
}

/// Swift mirror of `boss_protocol::WorkItemExternalRef`. Stable upstream
/// pointer stored on a work item that is linked to an external tracker issue.
struct WorkItemExternalRef: Codable, Hashable {
    /// Tracker discriminator (`"github"`, etc.).
    var kind: String
    /// Stable opaque lookup key (`"spinyfin/mono#560"` for GitHub).
    var canonicalID: String
    /// Tracker-specific extras as a raw JSON string (engine-opaque).
    var raw: String
    /// Canonical browser URL for the upstream issue.
    var webURL: String
    /// Unix-seconds string of the last successful upstream→Boss reconcile.
    var syncedAt: String?
    /// Unix-seconds string when the binding was cleared. `nil` while active.
    var unboundAt: String?

    enum CodingKeys: String, CodingKey {
        case kind
        case canonicalID = "canonical_id"
        case raw
        case webURL = "web_url"
        case syncedAt = "synced_at"
        case unboundAt = "unbound_at"
    }

    init(kind: String, canonicalID: String, raw: String, webURL: String,
         syncedAt: String? = nil, unboundAt: String? = nil) {
        self.kind = kind
        self.canonicalID = canonicalID
        self.raw = raw
        self.webURL = webURL
        self.syncedAt = syncedAt
        self.unboundAt = unboundAt
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        kind = try container.decode(String.self, forKey: .kind)
        canonicalID = try container.decode(String.self, forKey: .canonicalID)
        webURL = try container.decode(String.self, forKey: .webURL)
        syncedAt = try container.decodeIfPresent(String.self, forKey: .syncedAt)
        unboundAt = try container.decodeIfPresent(String.self, forKey: .unboundAt)
        // `raw` is an arbitrary JSON value; decode into Data then re-encode
        // as a string so callers get a stable type without depending on
        // AnyCodable or similar.
        if let rawValue = try? container.decode(AnyDecodable.self, forKey: .raw) {
            let data = try JSONSerialization.data(withJSONObject: rawValue.value)
            raw = String(data: data, encoding: .utf8) ?? "{}"
        } else {
            raw = "{}"
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(kind, forKey: .kind)
        try container.encode(canonicalID, forKey: .canonicalID)
        try container.encode(webURL, forKey: .webURL)
        try container.encodeIfPresent(syncedAt, forKey: .syncedAt)
        try container.encodeIfPresent(unboundAt, forKey: .unboundAt)
        if let data = raw.data(using: .utf8),
           let obj = try? JSONSerialization.jsonObject(with: data) {
            let wrapped = AnyEncodable(obj)
            try container.encode(wrapped, forKey: .raw)
        } else {
            try container.encode([String: String](), forKey: .raw)
        }
    }
}

/// Type-erased helper for decoding arbitrary JSON values in `WorkItemExternalRef.raw`.
private struct AnyDecodable: Decodable {
    let value: Any
    init(from decoder: Decoder) throws {
        if let container = try? decoder.singleValueContainer() {
            if let v = try? container.decode(Bool.self) { value = v; return }
            if let v = try? container.decode(Int.self) { value = v; return }
            if let v = try? container.decode(Double.self) { value = v; return }
            if let v = try? container.decode(String.self) { value = v; return }
            if let v = try? container.decode([String: AnyDecodable].self) {
                value = v.mapValues { $0.value }; return
            }
            if let v = try? container.decode([AnyDecodable].self) {
                value = v.map { $0.value }; return
            }
        }
        value = NSNull()
    }
}

private struct AnyEncodable: Encodable {
    let value: Any
    init(_ value: Any) { self.value = value }
    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch value {
        case let v as Bool: try container.encode(v)
        case let v as Int: try container.encode(v)
        case let v as Double: try container.encode(v)
        case let v as String: try container.encode(v)
        case let v as [String: Any]:
            let mapped = v.mapValues { AnyEncodable($0) }
            try container.encode(mapped)
        case let v as [Any]:
            try container.encode(v.map { AnyEncodable($0) })
        default:
            try container.encodeNil()
        }
    }
}

/// Swift mirror of `boss_protocol::SetProductExternalTrackerInput`.
struct SetProductExternalTrackerInput: Codable, Hashable {
    var productID: String
    var kind: String?
    var config: String?
    var unset: Bool = false

    enum CodingKeys: String, CodingKey {
        case productID = "product_id"
        case kind
        case config
        case unset
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(productID, forKey: .productID)
        try container.encodeIfPresent(kind, forKey: .kind)
        if let config, let data = config.data(using: .utf8),
           let obj = try? JSONSerialization.jsonObject(with: data) {
            try container.encode(AnyEncodable(obj), forKey: .config)
        }
        try container.encode(unset, forKey: .unset)
    }

    init(productID: String, kind: String? = nil, config: String? = nil, unset: Bool = false) {
        self.productID = productID
        self.kind = kind
        self.config = config
        self.unset = unset
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        productID = try container.decode(String.self, forKey: .productID)
        kind = try container.decodeIfPresent(String.self, forKey: .kind)
        unset = (try? container.decodeIfPresent(Bool.self, forKey: .unset)) ?? false
        if let rawValue = try? container.decodeIfPresent(AnyDecodable.self, forKey: .config) {
            let data = try JSONSerialization.data(withJSONObject: rawValue.value)
            config = String(data: data, encoding: .utf8)
        } else {
            config = nil
        }
    }
}

/// Swift mirror of `boss_protocol::LinkExternalRefInput`.
struct LinkExternalRefInput: Codable, Hashable, Equatable {
    var workItemID: String
    var kind: String
    var canonicalID: String

    init(workItemID: String, kind: String, canonicalID: String) {
        self.workItemID = workItemID
        self.kind = kind
        self.canonicalID = canonicalID
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        workItemID = try c.decode(String.self, forKey: .workItemID)
        kind = try c.decode(String.self, forKey: .kind)
        canonicalID = try c.decode(String.self, forKey: .canonicalID)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(workItemID, forKey: .workItemID)
        try c.encode(kind, forKey: .kind)
        try c.encode(canonicalID, forKey: .canonicalID)
    }

    private enum CodingKeys: String, CodingKey {
        case workItemID = "work_item_id"
        case kind
        case canonicalID = "canonical_id"
    }
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
    /// Repo URL the project's design doc lives in. `nil` → inherit
    /// from the project's product. Mirrors
    /// `Project.design_doc_repo_remote_url`.
    var designDocRepoRemoteURL: String? = nil
    /// Branch the design doc lives on. `nil` → inherit from the
    /// product's docs branch (or `"main"`). Mirrors
    /// `Project.design_doc_branch`.
    var designDocBranch: String? = nil
    /// Repo-relative path to the design doc. `nil` → no pointer set,
    /// UI affordance hidden. Mirrors `Project.design_doc_path`.
    var designDocPath: String? = nil
    /// Per-product short id. `nil` only on rows predating the migration
    /// (the engine backfills these at startup, so `nil` is transient).
    /// Mirrors `Project.short_id` on the wire.
    var shortID: Int? = nil
}

/// Swift mirror of `boss_protocol::SetProjectDesignDocInput`.
/// Three optional override fields plus an `unset` switch the engine
/// uses to clear the pointer. `nil` paths are skipped on encode so
/// the wire form matches serde's `skip_serializing_if`.
struct SetProjectDesignDocInput: Codable, Hashable {
    var projectID: String
    var designDocRepoRemoteURL: String?
    var designDocBranch: String?
    var designDocPath: String?
    var unset: Bool = false

    enum CodingKeys: String, CodingKey {
        case projectID = "project_id"
        case designDocRepoRemoteURL = "design_doc_repo_remote_url"
        case designDocBranch = "design_doc_branch"
        case designDocPath = "design_doc_path"
        case unset
    }
}

/// Resolution kind for a project's design-doc pointer. Discriminator
/// drives the open affordance: same/other product can fast-path into
/// a leased workspace; `.external` always opens the GitHub web URL.
enum ResolvedDesignDocKind: Hashable {
    case sameProduct(productID: String)
    case otherProduct(productID: String)
    case external
}

extension ResolvedDesignDocKind: Codable {
    enum CodingKeys: String, CodingKey {
        case type
        case productID = "product_id"
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "same_product":
            self = .sameProduct(productID: try container.decode(String.self, forKey: .productID))
        case "other_product":
            self = .otherProduct(productID: try container.decode(String.self, forKey: .productID))
        case "external":
            self = .external
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown ResolvedDesignDocKind type: \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .sameProduct(let productID):
            try container.encode("same_product", forKey: .type)
            try container.encode(productID, forKey: .productID)
        case .otherProduct(let productID):
            try container.encode("other_product", forKey: .type)
            try container.encode(productID, forKey: .productID)
        case .external:
            try container.encode("external", forKey: .type)
        }
    }
}

/// Swift mirror of `boss_protocol::ResolvedDesignDoc` — the
/// concrete `(repo, branch, path)` triple plus the kind discriminator
/// that decides which open path the affordance should take.
struct ResolvedDesignDoc: Codable, Hashable {
    var repoRemoteURL: String
    var branch: String
    var path: String
    var kind: ResolvedDesignDocKind

    enum CodingKeys: String, CodingKey {
        case repoRemoteURL = "repo_remote_url"
        case branch
        case path
        case kind
    }
}

/// Swift mirror of `boss_protocol::ProjectDesignDocState`. Drives
/// the UI affordance: `.notSet` hides the icon, `.resolved` shows a
/// clickable doc icon (with a tooltip rendered from `webURL`), and
/// `.broken` shows a warning glyph that opens the re-point form.
///
/// On `.resolved`, `workspacePath` is the absolute path of a cube
/// workspace currently leased for the resolved repo (or `nil` when
/// none is leased). The open dispatcher uses it to fast-path
/// `$EDITOR` / the in-app renderer onto the workspace file system
/// when the kind is same- or other-product; absence falls back to
/// the `rawContentURL` (GitHub raw-content fetch for in-app rendering)
/// and then the GitHub web URL.
enum ProjectDesignDocState: Hashable {
    case notSet
    case resolved(resolved: ResolvedDesignDoc, workspacePath: String?, webURL: String, rawContentURL: String?)
    case broken(reason: String)
}

extension ProjectDesignDocState: Codable {
    enum CodingKeys: String, CodingKey {
        case type
        case resolved
        case workspacePath = "workspace_path"
        case webURL = "web_url"
        case rawContentURL = "raw_content_url"
        case reason
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let type = try container.decode(String.self, forKey: .type)
        switch type {
        case "not_set":
            self = .notSet
        case "resolved":
            self = .resolved(
                resolved: try container.decode(ResolvedDesignDoc.self, forKey: .resolved),
                workspacePath: try container.decodeIfPresent(String.self, forKey: .workspacePath),
                webURL: try container.decode(String.self, forKey: .webURL),
                rawContentURL: try container.decodeIfPresent(String.self, forKey: .rawContentURL)
            )
        case "broken":
            self = .broken(reason: try container.decode(String.self, forKey: .reason))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: container,
                debugDescription: "Unknown ProjectDesignDocState type: \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .notSet:
            try container.encode("not_set", forKey: .type)
        case .resolved(let resolved, let workspacePath, let webURL, let rawContentURL):
            try container.encode("resolved", forKey: .type)
            try container.encode(resolved, forKey: .resolved)
            try container.encodeIfPresent(workspacePath, forKey: .workspacePath)
            try container.encode(webURL, forKey: .webURL)
            try container.encodeIfPresent(rawContentURL, forKey: .rawContentURL)
        case .broken(let reason):
            try container.encode("broken", forKey: .type)
            try container.encode(reason, forKey: .reason)
        }
    }
}

/// Swift mirror of `boss_protocol::ResolveProjectDesignDocOutput` —
/// the wire envelope returned by the `ResolveProjectDesignDoc` RPC.
struct ResolveProjectDesignDocOutput: Codable, Hashable {
    var projectID: String
    var state: ProjectDesignDocState

    enum CodingKeys: String, CodingKey {
        case projectID = "project_id"
        case state
    }
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
    /// The surface that filed this row — `cli`, `bossctl`, `mac_app`,
    /// `engine_auto`, or `unknown`. Pre-column rows arrive as
    /// `unknown` from the engine's migration default.
    var createdVia: String = "unknown"
    /// Per-work-item repo override. `nil` → inherit from the parent
    /// product's `repoRemoteURL`. Pre-column rows decode as `nil`
    /// because serde skips the field when it's `None` on the wire
    /// (see `Task.repo_remote_url` in `boss_protocol::types`).
    var repoRemoteURL: String? = nil
    /// When `status == "blocked"`, the engine's discriminator for
    /// *why* — `"dependency"`, `"merge_conflict"`, `"review_feedback"`,
    /// `"ci_failure"`, `"ci_failure_exhausted"`. `nil` for non-blocked
    /// rows and for legacy blocked rows without a tracked reason.
    /// Phase 1 of the merge-conflict design only populates this; the
    /// kanban renders it as no-op decoration until a later phase wires
    /// the badge labels through.
    var blockedReason: String? = nil
    /// Soft FK to the engine attempt currently trying to clear the
    /// block (a `conflict_resolutions.id` for `merge_conflict`).
    /// Discriminated by `blockedReason`; `nil` for blocks without an
    /// engine-managed attempt.
    var blockedAttemptID: String? = nil
    /// Per-product short id. `nil` only on rows predating the migration
    /// (the engine backfills these at startup, so `nil` is transient).
    /// Mirrors `Task.short_id` on the wire.
    var shortID: Int? = nil
    /// When `true` the engine will dispatch a worker the moment a slot
    /// is free. Rows with `status=todo AND autostart=true` that have no
    /// active execution are "dispatch-pending" — the engine has committed
    /// to running them but the pool is full. The kanban routes these to
    /// the Doing column with a distinct waiting indicator rather than
    /// leaving them in Backlog. Defaults to `false` when absent from the
    /// wire so legacy rows without the field stay in Backlog (unchanged).
    var autostart: Bool = false
    /// Aggregate required-CI state at last merge-poller probe. One of:
    /// `"in_progress"`, `"success"`, `"fail"`, `"unknown"`. `nil` until the
    /// first probe completes. Only rendered when `status == "in_review"` and
    /// `prURL` is non-nil; hidden otherwise.
    var ciRequiredState: String? = nil
    /// JSON-encoded list of failing check objects for the CI tooltip.
    /// Each object has `"name"` and `"conclusion"` keys. `nil` unless
    /// `ciRequiredState == "fail"`.
    var ciRequiredDetail: String? = nil
    /// Review-gating state at last merge-poller probe. One of:
    /// `"required"`, `"approved"`, `"changes_requested"`, `"unknown"`. `nil`
    /// until the first probe completes. Only rendered when `status == "in_review"`
    /// and `prURL` is non-nil; hidden otherwise.
    var reviewRequiredState: String? = nil
    /// JSON-encoded list of reviewer login strings for the review tooltip.
    /// `nil` unless `reviewRequiredState` is `"approved"` or
    /// `"changes_requested"`.
    var reviewRequiredDetail: String? = nil
    /// RFC 3339 timestamp of the most recent successful poll that wrote the
    /// PR state fields above. `nil` until the first probe completes.
    var prStatePolledAt: String? = nil
    /// Merge-queue state at last poll. `"queued"` when the PR is currently in
    /// GitHub's merge queue; `nil` when not queued or the repo has no merge
    /// queue. Replaces the CI indicator on Review-lane cards while merging.
    var mergeQueueState: String? = nil
    /// Stable upstream pointer to the external tracker issue linked to this
    /// work item. `nil` when no binding exists. Mirrors `Task.external_ref`.
    var externalRef: WorkItemExternalRef? = nil
    /// Repo-relative path to the markdown doc produced by an investigation
    /// worker. `nil` until the worker calls `boss task set-investigation-doc`.
    /// Only meaningful on `kind == "investigation"` rows.
    var investigationDocPath: String? = nil
    /// Remote URL of the repo hosting the investigation doc. `nil` means
    /// "resolve from product docs_repo or BOSS_USER_DOCS_REPO at set time."
    var investigationDocRepoRemoteUrl: String? = nil
    /// PR branch the investigation doc was opened on. Used to construct the
    /// in-review GitHub URL while the PR is open.
    var investigationDocBranch: String? = nil
    /// Soft FK to the parent task for `kind == "revision"` rows. `nil`
    /// for non-revision rows. Mirrors `Task.parent_task_id` on the wire.
    var parentTaskId: String? = nil
    /// Engine-computed R-number for revision tasks (1-based, chain-root-scoped).
    /// `nil` for non-revision rows. Mirrors the derived `revision_seq` field.
    var revisionSeq: Int? = nil
    /// Denormalized parent chain-root PR URL for fast revision card rendering.
    /// `nil` for non-revision rows. Mirrors `revision_parent_pr_url` on the wire.
    var revisionParentPrUrl: String? = nil

    var isChore: Bool {
        kind == "chore"
    }

    /// GitHub web URL for the investigation doc, derived at render time
    /// from the stored `(repo, branch, path)` pointer. Returns `nil`
    /// when the pointer is not yet set or the repo URL can't be parsed.
    /// Uses `investigationDocBranch` while the PR is open; falls back to
    /// `"main"` after merge (branch field cleared by future tooling).
    var investigationDocWebURL: String? {
        guard let path = investigationDocPath, !path.isEmpty,
              let repo = investigationDocRepoRemoteUrl, !repo.isEmpty else {
            return nil
        }
        let branch = investigationDocBranch ?? "main"
        let slug = ProjectDesignDocAffordancePresentation.repoSlug(from: repo)
        return "https://github.com/\(slug)/blob/\(branch)/\(path)"
    }
}

/// Derivation helpers for the kanban card's "blocked" badge — the
/// orange chip in the card footer that reads e.g. `Merge Conflict` /
/// `Blocked`. Centralised so the View and unit tests share one rule.
///
/// **Rule:** the badge MUST only render when `task.status == "blocked"`.
/// Per the engine spec (`Task::blocked_reason` doc), the scalar
/// `blocked_reason` field is `NULL` on rows whose `status` is not
/// `'blocked'`. A non-blocked row carrying a non-nil `blockedReason`
/// is, by definition, locally stale (the engine has cleared the
/// scalar but the macOS reducer hasn't reconverged yet — typically
/// because an `events.sock` envelope was dropped or the work-tree
/// refresh hasn't landed). The badge must NOT mirror that stale
/// signal: the lane is the source of truth, and the lane comes from
/// `status`. So the badge derivation gates on `status` rather than
/// trusting `blockedReason` in isolation. See the chore card
/// `Kanban chore card shows stale "Merge Conflict" badge` regression.
enum WorkBlockedBadge {
    /// Footer chip text for `task`, or `nil` when no chip should
    /// appear. Callers pass the chip text straight into `WorkStatusBadge`;
    /// the `nil` path collapses the chip entirely.
    static func badgeText(for task: WorkTask) -> String? {
        guard task.status == "blocked" else { return nil }
        guard let reason = task.blockedReason else { return "Blocked" }
        return label(forReason: reason)
    }

    /// Human-readable label for a raw `blocked_reason` string. Used by
    /// [[badgeText(for:)]] and by any future surface (e.g. detail
    /// metadata row) that needs the same vocabulary. Falls back to a
    /// title-cased version of the raw value so unknown / future reason
    /// codes degrade gracefully rather than rendering as the empty
    /// string.
    static func label(forReason reason: String) -> String {
        switch reason {
        case "dependency": return "Dependency"
        case "merge_conflict": return "Merge Conflict"
        case "ci_failure": return "CI Failure"
        case "ci_failure_exhausted": return "CI Failed"
        case "review_feedback": return "Review"
        default: return reason.replacingOccurrences(of: "_", with: " ").capitalized
        }
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

/// Static copy for the Product create form's repo-URL field. Extracted
/// so the wording can be asserted in a unit test without driving the
/// SwiftUI view itself — per design Q10, the form must surface the
/// field as optional and explain that products spanning multiple repos
/// rely on per-work-item overrides.
enum ProductRepoFieldCopy {
    static let placeholder = "Remote URL (optional)"
    static let helperText =
        "Optional. Leave blank if this product spans multiple repos; per-work-item repo overrides will be required."
}

/// Pure-data form state for the chore/task create form's repo field,
/// per design Q10 / follow-up chore #10 of
/// `multi-repo-work-modeling.md`. Lives outside the SwiftUI view so
/// the two render modes ("product has default" vs "product has no
/// default") and the submission shape can be pinned by XCTest without
/// spinning up a host. The view is a thin reflection of this state.
struct WorkCreateRepoFormState: Equatable {
    enum Mode: Equatable {
        /// Parent product has a `repo_remote_url`. The field is hidden
        /// by default, with an "Override repo…" disclosure that
        /// expands the picker. Inheriting the default is the
        /// no-action path.
        case productHasDefault(defaultURL: String)
        /// Parent product has no default. The field is shown and
        /// required. A "Set as product default" affordance becomes
        /// available for fresh URLs (URLs not already in the
        /// product's empirical known-repo set).
        case productHasNoDefault
    }

    var mode: Mode
    /// Distinct URL set across the product's existing tasks / chores
    /// plus the product default — mirrors the CLI's
    /// `known_repos_for_product` (multi-repo design Q4). Drives the
    /// "Recent repos" picker.
    var knownRepos: [String]
    /// In `.productHasDefault`, whether the user expanded the
    /// "Override repo…" disclosure. Ignored in
    /// `.productHasNoDefault` (the field is always visible there).
    var overrideEnabled: Bool
    /// Text in the URL field. Empty when the disclosure is closed in
    /// `.productHasDefault`; user-supplied otherwise.
    var enteredURL: String
    /// State of the "Set as product default" checkbox. Only
    /// meaningful when `showSetAsProductDefaultCheckbox` is `true` —
    /// the view hides the affordance otherwise.
    var setAsProductDefault: Bool

    /// Initial state for a fresh sheet. Picks the mode from the
    /// parent product's repo URL: empty / whitespace → no default.
    init(productRepoURL: String?, knownRepos: [String]) {
        let normalized = productRepoURL?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if let normalized, !normalized.isEmpty {
            mode = .productHasDefault(defaultURL: normalized)
        } else {
            mode = .productHasNoDefault
        }
        self.knownRepos = knownRepos
        overrideEnabled = false
        enteredURL = ""
        setAsProductDefault = false
    }

    /// URL the submission should write to `tasks.repo_remote_url`.
    /// `nil` means "inherit the product default" — the engine treats
    /// an absent field exactly that way.
    var submittedURL: String? {
        let trimmed = enteredURL.trimmingCharacters(in: .whitespacesAndNewlines)
        switch mode {
        case .productHasDefault:
            return (overrideEnabled && !trimmed.isEmpty) ? trimmed : nil
        case .productHasNoDefault:
            return trimmed.isEmpty ? nil : trimmed
        }
    }

    /// True when the create button should be disabled because the
    /// repo field is required and unfilled. The product-has-default
    /// mode never blocks submission on the repo field (inheriting is
    /// always valid); the no-default mode requires a URL.
    var isSubmissionBlocked: Bool {
        switch mode {
        case .productHasDefault:
            return false
        case .productHasNoDefault:
            return enteredURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
    }

    /// Whether the "Set as product default" checkbox should be
    /// rendered. Only meaningful in `.productHasNoDefault` mode, and
    /// only when the user has typed a *fresh* URL — one not already
    /// in the empirical known-repo set. The design's intent is that
    /// the affordance promotes a brand-new repo URL into the product
    /// default; offering it on a URL the product has already seen
    /// would be redundant.
    var showSetAsProductDefaultCheckbox: Bool {
        guard case .productHasNoDefault = mode else { return false }
        let trimmed = enteredURL.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return false }
        return !knownRepos.contains(trimmed)
    }

    /// Whether the form should send an `update_work_item` patch on
    /// the parent product to set `repo_remote_url` as a side-effect
    /// of work-item creation. Encodes the "Set as product default"
    /// rule end-to-end: the checkbox must be both visible and ticked.
    var shouldSetAsProductDefault: Bool {
        showSetAsProductDefaultCheckbox && setAsProductDefault
    }
}

/// Static copy for the work-item (chore + task) create form's repo
/// field. Extracted from the SwiftUI view for the same reason as
/// `ProductRepoFieldCopy`: the wording is part of the contract with
/// the user (design Q10 calls it out explicitly) and a UI tweak that
/// drops the "required" cue or the override disclosure label should
/// trip a failing test.
enum WorkItemRepoFieldCopy {
    /// Field placeholder when the repo input is required (product has
    /// no default).
    static let requiredPlaceholder = "Repo remote URL (required)"
    /// Field placeholder when the repo input is an optional override
    /// (product has a default and the disclosure is expanded).
    static let overridePlaceholder = "Repo remote URL"
    /// Disclosure title in product-has-default mode.
    static let overrideDisclosureLabel = "Override repo…"
    /// Helper text under the field in product-has-no-default mode.
    static let requiredHelperText =
        "Required. This product has no default repo, so each work item must specify its own."
    /// Helper text under the field in product-has-default mode when
    /// the override disclosure is expanded.
    static let overrideHelperText =
        "Leave blank to inherit the product's default repo."
    /// "Set as product default" checkbox label. Visible only when the
    /// product has no default and the user has entered a fresh URL.
    static let setAsProductDefaultLabel = "Set as product default"
    /// "Recent repos" picker label.
    static let recentReposLabel = "Recent repos"
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
    /// Project id this section represents when the board is grouped
    /// by project. `nil` for chores / un-projected sections / column
    /// groupings. The kanban project-card affordance (design-doc
    /// icon) reads this to look up the resolved
    /// `ProjectDesignDocState` for the section's header row.
    var projectID: String? = nil
}

/// Swift mirror of `boss_protocol::short_name_for(url)` from the
/// multi-repo work modeling design (Q3). The canonical short name is
/// the URL's path basename minus a trailing `.git`. Handles both
/// `https://github.com/foo/bar.git` and SCP-style
/// `git@github.com:foo/bar.git`. Falls back to the trimmed input when
/// neither shape is recognisable so the chip never renders empty.
func shortRepoName(for repoURL: String) -> String {
    let trimmed = repoURL.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !trimmed.isEmpty else { return repoURL }
    let lastSlash = trimmed.split(separator: "/", omittingEmptySubsequences: true).last
        .map(String.init) ?? trimmed
    let lastSegment = lastSlash.split(separator: ":", omittingEmptySubsequences: true).last
        .map(String.init) ?? lastSlash
    if lastSegment.hasSuffix(".git") {
        return String(lastSegment.dropLast(4))
    }
    return lastSegment
}

/// Parsed `(org, repo, number)` triple for a GitHub PR URL like
/// `https://github.com/<org>/<repo>/pull/<n>`. Returns `nil` for any
/// other host or shape — the caller falls back to the raw URL string.
/// Used both by the kanban PR-link label renderer and by the
/// board-local ambiguity detector that decides whether to expand
/// `repo#n` back to `org/repo#n`.
func parseGitHubPRURL(_ urlString: String) -> (org: String, repo: String, number: String)? {
    guard let url = URL(string: urlString),
          let host = url.host?.lowercased(),
          host == "github.com" || host == "www.github.com"
    else {
        return nil
    }
    let parts = url.path.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
    guard parts.count == 4,
          parts[2] == "pull",
          !parts[0].isEmpty,
          !parts[1].isEmpty,
          !parts[3].isEmpty,
          parts[3].allSatisfy(\.isNumber)
    else {
        return nil
    }
    return (org: parts[0], repo: parts[1], number: parts[3])
}

/// Repo names (lowercased) that appear with two or more distinct orgs
/// across the supplied card set's PR URLs. A name in this set means
/// `repo#n` alone is ambiguous on the current board, so the kanban
/// must fall back to the full `org/repo#n` label for that PR.
///
/// Non-GitHub PR URLs and cards without a PR URL are ignored — they
/// can never collide on a repo-name basis.
func ambiguousPRRepoNames(in cards: [WorkTask]) -> Set<String> {
    var orgsByRepo: [String: Set<String>] = [:]
    for task in cards {
        guard let prURL = task.prURL,
              let parsed = parseGitHubPRURL(prURL)
        else { continue }
        let repoKey = parsed.repo.lowercased()
        let orgKey = parsed.org.lowercased()
        orgsByRepo[repoKey, default: []].insert(orgKey)
    }
    return Set(orgsByRepo.compactMap { $0.value.count > 1 ? $0.key : nil })
}

/// Label to display for a PR URL on a kanban card.
///
/// - Returns `nil` when `urlString` isn't a parseable GitHub PR URL —
///   the caller should fall back to the raw URL string.
/// - Returns `repo#n` when the repo name is unambiguous across the
///   supplied `ambiguousRepoNames` set (the board-local disambiguation
///   key from [[ambiguousPRRepoNames(in:)]]).
/// - Returns `org/repo#n` when the repo name *is* in that set, or when
///   the set is `nil` (caller wants the always-full form, e.g. for the
///   detail popover and the hover tooltip).
func pullRequestLinkLabel(
    for urlString: String,
    ambiguousRepoNames: Set<String>?
) -> String? {
    guard let parsed = parseGitHubPRURL(urlString) else { return nil }
    if let ambiguous = ambiguousRepoNames,
       !ambiguous.contains(parsed.repo.lowercased()) {
        return "\(parsed.repo)#\(parsed.number)"
    }
    return "\(parsed.org)/\(parsed.repo)#\(parsed.number)"
}

/// How the kanban should surface the repo for a product, derived from
/// the work item description for "macOS: kanban card repo chip" and
/// design Q7. Single-repo mode lifts one chip to the product header;
/// multi-repo mode prints a chip on every card. `none` collapses the
/// affordance — the product has no default and no card overrides, so
/// there is nothing repo-shaped to surface.
enum WorkBoardRepoMode: Equatable {
    case singleRepo(url: String)
    case multiRepo
    case none

    /// Compute the mode from the product default and the visible card
    /// set. The rule per the work item description:
    /// - Multi-repo as soon as any card carries a per-task override OR
    ///   resolved URLs differ across cards.
    /// - Single-repo when no overrides exist and a product default is
    ///   set; every card inherits the same URL.
    /// - None when neither product nor any card carries a URL.
    static func compute(
        productRepoURL: String?,
        cards: [WorkTask]
    ) -> WorkBoardRepoMode {
        let productURL = nonEmpty(productRepoURL)
        let overrides = cards.compactMap { nonEmpty($0.repoRemoteURL) }
        if overrides.isEmpty {
            if let productURL { return .singleRepo(url: productURL) }
            return .none
        }
        // Any override → multi-repo, even when overrides happen to all
        // match the product default. A user who set an explicit
        // override on a card has stated *I want this row's repo to be
        // visible*; suppressing the chip would erase that signal.
        return .multiRepo
    }

    private static func nonEmpty(_ value: String?) -> String? {
        guard let value, !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return nil
        }
        return value
    }
}

/// Pure-data presentation for the kanban repo chip — short name for
/// the rendered text, full URL for the hover tooltip, and a
/// provenance string that tells the reader where the URL came from.
/// Lives outside the SwiftUI view so tests can pin chip text + tooltip
/// without spinning up a host (mirrors
/// `ProjectDesignDocAffordancePresentation`).
///
/// Per-card chips only render when the card carries information the
/// product header can't: either an explicit override of the product
/// default, or the card's own URL on a product with no default. A
/// card that simply inherits the product default never gets a chip —
/// the chip would be redundant with the header.
struct RepoChipPresentation: Equatable {
    let shortName: String
    let fullURL: String
    let provenance: Provenance

    enum Provenance: Equatable {
        /// Chip lives on the product header, identifying the product's
        /// default repo. Not used for per-card chips.
        case productDefault
        /// Card has its own `repoRemoteURL`. On a product with a
        /// default this is a true override; on a no-default product
        /// the card's URL is just the card's repo. Either way the
        /// chip is informative.
        case taskOverride
    }

    var tooltip: String {
        switch provenance {
        case .productDefault:
            return "\(fullURL)\nInherited from product"
        case .taskOverride:
            return "\(fullURL)\nRepo set on this card"
        }
    }

    var accessibilityLabel: String {
        switch provenance {
        case .productDefault:
            return "Repo \(shortName), inherited from product"
        case .taskOverride:
            return "Repo \(shortName), set on this card"
        }
    }

    /// Build a chip for one card given the parent product's default.
    /// Returns `nil` when the card has no per-row `repoRemoteURL` or when
    /// the task's repo matches the product default (case-insensitive,
    /// trimming `.git` suffix). Returns non-nil only when the task has an
    /// explicit repo that differs from the product default, or when the
    /// product has no default but the task does.
    static func forCard(
        task: WorkTask,
        productRepoURL: String?
    ) -> RepoChipPresentation? {
        guard let override = nonEmpty(task.repoRemoteURL) else {
            return nil
        }
        if let productDefault = nonEmpty(productRepoURL),
           reposEqual(override, productDefault) {
            return nil
        }
        return RepoChipPresentation(
            shortName: shortRepoName(for: override),
            fullURL: override,
            provenance: .taskOverride
        )
    }

    /// Build the chip carried on the product header in single-repo
    /// mode. Always provenance `.productDefault` — single-repo mode
    /// requires zero overrides by construction.
    static func forProductHeader(productRepoURL: String) -> RepoChipPresentation {
        RepoChipPresentation(
            shortName: shortRepoName(for: productRepoURL),
            fullURL: productRepoURL,
            provenance: .productDefault
        )
    }

    private static func nonEmpty(_ value: String?) -> String? {
        guard let value, !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return nil
        }
        return value
    }

    private static func reposEqual(_ url1: String, _ url2: String) -> Bool {
        let normalize = { (url: String) in
            var normalized = url.lowercased()
            if normalized.hasSuffix(".git") {
                normalized.removeLast(4)
            }
            return normalized
        }
        return normalize(url1) == normalize(url2)
    }
}

/// Pure-data presentation for the work-item detail "Repo:" row.
/// Mirrors the CLI `boss <kind> show` Repo line so the macOS detail
/// popover and the terminal output stay in lockstep on the
/// provenance vocabulary (per Follow-up chore #12 of
/// `multi-repo-work-modeling.md`). Three states correspond to the
/// three branches of the engine's `resolve_repo_for_work_item`:
/// override on the work item, inherited from the parent product, or
/// no resolution at all (the work item cannot dispatch).
///
/// `provenanceLabel` is the parenthetical that follows the URL on
/// the CLI; the `.none` case has no URL and the label is the entire
/// line. The Swift view renders the label as a secondary-style
/// caption beneath the URL.
struct RepoOverridePresentation: Equatable {
    let resolvedURL: String?
    let provenanceLabel: String
    let provenance: Provenance

    enum Provenance: Equatable {
        case taskOverride
        case productDefault(productSlug: String)
        case none
    }

    /// Full single-line form, matching the CLI `Repo: <url>
    /// (<provenance>)` shape. Used by tests to pin the wire-shape
    /// agreement between CLI and macOS UI; the view itself renders
    /// the URL and label as separate text rows so each can carry its
    /// own style.
    var cliLine: String {
        switch provenance {
        case .taskOverride, .productDefault:
            if let url = resolvedURL { return "\(url) (\(provenanceLabel))" }
            return provenanceLabel
        case .none:
            return provenanceLabel
        }
    }

    /// Build the presentation for one work item given its parent
    /// product (or `nil` when the product can't be resolved — e.g. a
    /// snapshot in flight). When the product is unavailable, we can
    /// only honour the override; an empty override collapses to the
    /// "cannot dispatch" state so the row never silently looks
    /// inherited from a product that isn't there.
    static func resolve(
        task: WorkTask,
        product: WorkProduct?
    ) -> RepoOverridePresentation {
        if let override = nonEmpty(task.repoRemoteURL) {
            return RepoOverridePresentation(
                resolvedURL: override,
                provenanceLabel: "override on this work item",
                provenance: .taskOverride
            )
        }
        if let product, let inherited = nonEmpty(product.repoRemoteURL) {
            return RepoOverridePresentation(
                resolvedURL: inherited,
                provenanceLabel: "inherited from product `\(product.slug)`",
                provenance: .productDefault(productSlug: product.slug)
            )
        }
        return RepoOverridePresentation(
            resolvedURL: nil,
            provenanceLabel: "(none — work item cannot dispatch)",
            provenance: .none
        )
    }

    private static func nonEmpty(_ value: String?) -> String? {
        guard let value, !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return nil
        }
        return value
    }
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
    ///
    /// Dispatch-pending rows (`status=todo AND autostart=true`) route to
    /// Doing rather than Backlog. From the user's perspective these rows
    /// are already committed — the engine will start them the moment a
    /// slot frees up — so they belong visually with active work, not with
    /// unscheduled backlog items. The card renders a distinct hourglass
    /// indicator to distinguish "queued" from "working".
    var boardColumn: WorkBoardColumnKey {
        switch status {
        case "active":
            return .doing
        case "in_review":
            return .review
        case "done":
            return .done
        case "todo" where autostart:
            return .doing
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
    /// Free-text one-sentence description of what the worker is doing
    /// right now, generated by the engine's `live_status` summarizer
    /// from a tail of the transcript. `nil` while spawning, idle long
    /// enough for the prior text to have been cleared, or before the
    /// first summary lands. Rendered as the Doing-card subtitle and
    /// the Agents-tab worker header subtitle.
    let liveStatus: String?
    /// ISO-8601 timestamp of the most recent successful update to
    /// `liveStatus`. Used by the UI to dim stale values.
    let liveStatusAt: String?
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
    /// The row is `status=todo, autostart=true` and no worker slot is
    /// free yet. The engine has committed to dispatching it; the pool
    /// is just full. Renders as a hourglass rather than a coloured dot
    /// so it reads as "queued" rather than "active or paused worker".
    case dispatchPending

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
        case .dispatchPending:
            return "Queued — waiting for a worker slot"
        }
    }
}

/// Swift mirror of `boss_protocol::ConflictResolution`. One engine
/// attempt to clear a merge conflict on an `in_review` PR. Powers the
/// Engine tab's attempt-row list (design Phase 5 #14) and the
/// "🔧 conflict cleared" PR-card badge (#15).
struct WorkConflictResolution: Identifiable, Hashable {
    let id: String
    var productID: String
    var workItemID: String
    var prURL: String
    var prNumber: Int
    var headBranch: String
    var baseBranch: String
    var baseSHAAtTrigger: String?
    var headSHABefore: String?
    var headSHAAfter: String?
    /// `pending` / `running` / `succeeded` / `failed` / `abandoned` /
    /// `superseded`. See the wire-type docs in `boss_protocol::types`
    /// for the lifecycle.
    var status: String
    var failureReason: String?
    var cubeLeaseID: String?
    var cubeWorkspaceID: String?
    var workerID: String?
    /// Raw JSON blob the worker prompt was built from. Carried verbatim
    /// here so the detail panel can surface it without a separate fetch.
    var conflictDiagnosis: String?
    var createdAt: String
    var startedAt: String?
    var finishedAt: String?
}

/// PR-card chip state for the CI auto-fix flow (design Q11 / Phase
/// 11 #37). Either "engine is still trying" (with a numeric
/// `used/budget`) or "engine has given up." The exhausted variant
/// stays visible until the user kicks `boss engine ci retry`; the
/// in-flight variant clears when the next probe observes CI back at
/// `Clean`.
struct CiFailureBadge: Equatable, Hashable {
    enum State: String, Hashable {
        /// `blocked: ci_failure` — engine still trying.
        case inFlight = "in_flight"
        /// `blocked: ci_failure_exhausted` — engine has given up.
        case exhausted
    }
    var state: State
    var attemptsUsed: Int
    var budget: Int
}

/// Swift mirror of `boss_protocol::CiRemediation`. One engine attempt
/// to clear a CI failure on an `in_review` PR. Powers the Engine
/// tab's CI rows (design Phase 11 #37) and the per-PR badges (Q11).
struct WorkCiRemediation: Identifiable, Hashable {
    let id: String
    var productID: String
    var workItemID: String
    var prURL: String
    var prNumber: Int
    var headBranch: String
    var headSHAAtTrigger: String
    var headSHAAfter: String?
    /// `"fix"` or `"retrigger"` — the engine's pre-spawn triage call.
    var attemptKind: String
    /// `1` for fix-kind attempts that actually pushed; `0` for
    /// retriggers and triage-bailouts.
    var consumesBudget: Int
    /// JSON-encoded list of failing-check snapshots captured at trigger
    /// time. Stored as a verbatim string; consumers parse on demand.
    var failedChecks: String
    /// Worker-assigned classification of the failure after reading the
    /// log — one of `tractable` / `flaky_or_infra` / `unfixable`. `nil`
    /// until the worker fills it.
    var triageClass: String?
    var logExcerpt: String?
    /// `pending` / `running` / `succeeded` / `failed` / `abandoned` /
    /// `superseded`. See the wire-type docs in `boss_protocol::types`.
    var status: String
    var failureReason: String?
    var cubeLeaseID: String?
    var cubeWorkspaceID: String?
    var workerID: String?
    var createdAt: String
    var startedAt: String?
    var finishedAt: String?
}

/// Discriminator for the unified Engine-tab attempt feed. Phase 5 #14
/// lists `conflict_resolutions`; Phase 11 #37 grows the enum with the
/// CI subsystem (`ci_remediations`). The `rebase_attempts` row kind
/// is reserved for when the `auto-rebase-stacked-prs` flow lands.
enum EngineAttemptRow: Identifiable, Hashable {
    case conflictResolution(WorkConflictResolution)
    case ciRemediation(WorkCiRemediation)

    var id: String {
        switch self {
        case .conflictResolution(let r):
            return "crz:\(r.id)"
        case .ciRemediation(let r):
            return "cir:\(r.id)"
        }
    }

    var kindLabel: String {
        switch self {
        case .conflictResolution:
            return "Conflict"
        case .ciRemediation(let r):
            switch r.attemptKind {
            case "fix": return "CI fix"
            case "retrigger": return "CI retrigger"
            default: return "CI"
            }
        }
    }

    var status: String {
        switch self {
        case .conflictResolution(let r):
            return r.status
        case .ciRemediation(let r):
            return r.status
        }
    }

    var prURL: String {
        switch self {
        case .conflictResolution(let r):
            return r.prURL
        case .ciRemediation(let r):
            return r.prURL
        }
    }

    var workItemID: String {
        switch self {
        case .conflictResolution(let r):
            return r.workItemID
        case .ciRemediation(let r):
            return r.workItemID
        }
    }

    var createdAt: String {
        switch self {
        case .conflictResolution(let r):
            return r.createdAt
        case .ciRemediation(let r):
            return r.createdAt
        }
    }

    var finishedAt: String? {
        switch self {
        case .conflictResolution(let r):
            return r.finishedAt
        case .ciRemediation(let r):
            return r.finishedAt
        }
    }

    var failureReason: String? {
        switch self {
        case .conflictResolution(let r):
            return r.failureReason
        case .ciRemediation(let r):
            return r.failureReason
        }
    }
}

/// Snapshot of one per-installation setting, decoded from a
/// `settings_list` response. Mirrors `boss_protocol::SettingSnapshot`.
struct EngineSetting: Identifiable, Hashable {
    var id: String { key }
    let key: String
    let description: String
    let defaultEnabled: Bool
    let enabled: Bool
}

/// One UI-actionable engine-health issue, decoded from an
/// `engine_health_result` response. Mirrors
/// `boss_protocol::EngineHealthIssue` one-for-one. Drives the
/// chrome-level banner and the Settings-pane warning that flag
/// missing/invalid engine config — introduced after #699 where a
/// missing `ANTHROPIC_API_KEY` silently broke summarization with no
/// UI affordance.
struct EngineHealthIssue: Identifiable, Hashable {
    /// Stable lowercase snake_case kind id. Used as the `Identifiable`
    /// key so SwiftUI animations are stable across snapshots.
    var id: String { kind }
    let kind: String
    /// `"error"` or `"warning"` — drives banner color / icon.
    let severity: String
    let title: String
    let body: String
}

/// Snapshot of one engine feature flag, decoded from a
/// `feature_flags_list` response. Mirrors the engine's
/// `boss_protocol::FeatureFlagSnapshot` one-for-one.
struct FeatureFlag: Identifiable, Hashable {
    /// Stable flag identifier (lowercase snake_case). The toggle send
    /// path uses this verbatim; identifier for `Identifiable`.
    var id: String { name }
    let name: String
    let description: String
    let category: String
    let defaultEnabled: Bool
    let enabled: Bool
}

/// Snapshot of one engine metric (counter or gauge), decoded from a
/// `metrics_list_live_result` response. Mirrors the engine's
/// `boss_protocol::MetricLiveEntry` one-for-one.
struct EngineMetric: Identifiable, Hashable {
    var id: String { name }
    let name: String
    let description: String
    /// `"counter"` or `"gauge"`.
    let kind: String
    let value: Int64
    /// Milliseconds since Unix epoch of the last update. 0 = never updated.
    let timestampMs: Int64
    /// True when this row was rehydrated from state.db but the current
    /// engine binary has no matching handle.
    let stale: Bool
}

/// Presentation model for the kanban card's upstream-link affordance.
/// Derived from `WorkTask.externalRef`; `nil` when no external ref is present.
///
/// Three states map to three visual treatments:
/// - `externalRef == nil` → `forTask` returns `nil` (no affordance)
/// - `externalRef.unboundAt == nil` → bound; label in accent color, opens URL
/// - `externalRef.unboundAt != nil` → stale; label dimmed/strikethrough, still opens URL
struct ExternalRefLinkPresentation: Equatable {
    /// Short label rendered on the card, e.g. `↗ #560`.
    let label: String
    /// Canonical browser URL to open on click.
    let url: String
    /// Hover tooltip text.
    let tooltip: String
    /// True when the upstream binding was cleared (`unboundAt` is set).
    let isStale: Bool

    /// Derive the presentation for a task. Returns `nil` when the task has no
    /// external ref — callers use this to suppress the affordance entirely.
    static func forTask(_ task: WorkTask) -> ExternalRefLinkPresentation? {
        guard let ref = task.externalRef else { return nil }
        let stale = ref.unboundAt != nil
        let label = issueLabel(from: ref.canonicalID)
        var tooltip = ref.canonicalID
        if stale {
            tooltip += "\nUpstream binding cleared"
        } else if let syncedAt = ref.syncedAt {
            tooltip += "\nLast synced: \(syncedAt)"
        }
        return ExternalRefLinkPresentation(label: label, url: ref.webURL, tooltip: tooltip, isStale: stale)
    }

    /// Extracts a short display label from a canonical ID. For GitHub
    /// (`"spinyfin/mono#560"`) this yields `"↗ #560"`. Any canonical ID
    /// without a `#` fragment falls back to `"↗ <canonical_id>"`.
    static func issueLabel(from canonicalID: String) -> String {
        if let hashIdx = canonicalID.lastIndex(of: "#") {
            let fragment = String(canonicalID[hashIdx...])
            return "↗ \(fragment)"
        }
        return "↗ \(canonicalID)"
    }
}

/// Swift mirror of `boss_protocol::WorkAttentionItem`. One attention-item
/// row from `work_attention_items`, attached to either an execution or a
/// work item (product / task / chore).
struct WorkAttentionItem: Identifiable, Codable, Hashable {
    var id: String
    var executionID: String?
    var workItemID: String?
    var kind: String
    var status: String
    var title: String
    var bodyMarkdown: String
    var createdAt: String
    var resolvedAt: String?

    enum CodingKeys: String, CodingKey {
        case id
        case executionID = "execution_id"
        case workItemID = "work_item_id"
        case kind
        case status
        case title
        case bodyMarkdown = "body_markdown"
        case createdAt = "created_at"
        case resolvedAt = "resolved_at"
    }
}

/// Pure-data presentation model for an external-tracker attention item.
/// Derived from a `WorkAttentionItem` whose `kind` starts with
/// `"external_tracker_"`. `forItem` returns `nil` for unrecognised kinds
/// so callers can filter to only the items they know how to render.
///
/// Four reasons are defined in the design doc (chore 16):
/// - `external_tracker_auth_failed`
/// - `external_tracker_transient_errors`
/// - `external_tracker_removed_upstream`
/// - `external_tracker_permission_denied`
struct ExternalTrackerAttentionPresentation: Equatable {
    /// Short reason code extracted from the kind, e.g. `"auth_failed"`.
    let reasonCode: String
    /// Human-readable title shown in the attention list.
    let displayTitle: String
    /// One-line summary of the remediation action.
    let remediationHint: String
    /// SF Symbol name for the attention icon.
    let iconName: String
    /// Whether the item is still open (not resolved).
    let isOpen: Bool

    /// Build a presentation from a raw attention item. Returns `nil` when
    /// the kind is not a recognised external-tracker kind.
    static func forItem(_ item: WorkAttentionItem) -> ExternalTrackerAttentionPresentation? {
        let prefix = "external_tracker_"
        guard item.kind.hasPrefix(prefix) else { return nil }
        let reasonCode = String(item.kind.dropFirst(prefix.count))
        let (displayTitle, remediationHint, iconName) = metadata(for: reasonCode, item: item)
        return ExternalTrackerAttentionPresentation(
            reasonCode: reasonCode,
            displayTitle: displayTitle,
            remediationHint: remediationHint,
            iconName: iconName,
            isOpen: item.status == "open"
        )
    }

    private static func metadata(
        for reasonCode: String,
        item: WorkAttentionItem
    ) -> (String, String, String) {
        switch reasonCode {
        case "auth_failed":
            return (
                item.title,
                "Run `gh auth login` to refresh credentials.",
                "lock.trianglebadge.exclamationmark"
            )
        case "transient_errors":
            return (
                item.title,
                "Boss will retry automatically. Check network connectivity if this persists.",
                "exclamationmark.icloud"
            )
        case "removed_upstream":
            return (
                item.title,
                "Re-bind manually with `boss chore link-external` if this was unintended.",
                "link.badge.plus"
            )
        case "permission_denied":
            return (
                item.title,
                "Run `gh auth login --scopes repo` to grant write permission.",
                "exclamationmark.shield"
            )
        default:
            return (
                item.title,
                "See the engine log for details.",
                "exclamationmark.triangle"
            )
        }
    }
}
