import Foundation

/// Owns the single libghostty pane that hosts the Boss session — a
/// Claude Code session with a coordinator-flavoured system prompt
/// that uses `bossctl` to drive the engine.
///
/// The Boss session runs in a dedicated working directory under
/// Application Support so its `CLAUDE.md` system prompt and any
/// session state stay isolated from worker workspaces.
@MainActor
final class BossPaneModel: ObservableObject {
    let runtime: GhosttyRuntime
    @Published var session: TerminalPaneSession

    init() {
        self.runtime = GhosttyRuntime.shared
        let workingDirectory = Self.ensureBossWorkingDirectory()
        let launchSpec = TerminalLaunchSpec(
            fontSize: 11.0,
            workingDirectory: workingDirectory,
            initialInput: "claude\n"
        )
        self.session = TerminalPaneSession(
            id: "boss",
            role: .boss,
            launchSpec: launchSpec
        )
    }

    private static func ensureBossWorkingDirectory() -> String {
        let fm = FileManager.default
        let appSupport = fm
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first ?? URL(fileURLWithPath: NSHomeDirectory())
                .appendingPathComponent("Library/Application Support")
        let bossSession = appSupport.appendingPathComponent("Boss/boss-session")
        try? fm.createDirectory(at: bossSession, withIntermediateDirectories: true)

        let claudeDir = bossSession.appendingPathComponent(".claude")
        try? fm.createDirectory(at: claudeDir, withIntermediateDirectories: true)

        let claudeMd = bossSession.appendingPathComponent("CLAUDE.md")
        // Always rewrite so iterations on the prompt take effect on
        // the next Boss-session start without manually clearing files.
        try? bossSystemPrompt().write(to: claudeMd, atomically: true, encoding: .utf8)

        // Auto-mode allowlist for the Boss session. Without these,
        // Claude Code's auto-mode classifier blocks the Boss from
        // running its own CLIs (`boss` for work-taxonomy CRUD,
        // `bossctl` for control verbs) and we lose the Boss's
        // ability to delegate or queue work. Read-only inspection
        // tools (Read/Glob/Grep, gh PR/issue read verbs, jj
        // log/status/diff) are also allowed; explicit Edit/Write/
        // jj-push/git-push are not — the Boss delegates code work
        // to workers per its system prompt.
        let settings = bossSettingsLocalJson()
        let settingsPath = claudeDir.appendingPathComponent("settings.local.json")
        try? settings.write(to: settingsPath, atomically: true, encoding: .utf8)

        return bossSession.path
    }
}

private func bossSettingsLocalJson() -> String {
    """
    {
      "permissions": {
        "allow": [
          "Bash(boss *)",
          "Bash(bossctl *)",
          "Bash(gh pr view *)",
          "Bash(gh pr list *)",
          "Bash(gh pr checks *)",
          "Bash(gh pr comments *)",
          "Bash(gh issue view *)",
          "Bash(gh issue list *)",
          "Bash(jj log *)",
          "Bash(jj status)",
          "Bash(jj diff *)",
          "Read",
          "Glob",
          "Grep",
          "TodoWrite"
        ]
      }
    }
    """
}

private func bossSystemPrompt() -> String {
    """
    # The Boss

    You are The Boss inside the Boss V2 application — the single
    coordinating Claude Code session running alongside up to eight
    worker sessions in libghostty panes.

    Your role is to coordinate work and keep Boss's representation of
    work accurate. You delegate; you do not implement directly.

    ## How you control the engine

    Use `bossctl` (NOT `boss`) for control verbs. The `boss` CLI is
    available but is the user-facing CLI; `bossctl` is the
    coordinator-only superset. Common calls:

    - `bossctl agents list / status / focus / send / interrupt /
      launch / stop / transcript` — observe and steer worker sessions.
    - `bossctl probe <run-id> "question"` — inject a probe prompt that
      a worker answers on its next Stop boundary.
    - `bossctl work start <work-item-id>` — request the engine
      schedule a work item for execution.
    - `bossctl workspace summary` — see the cube workspace pool state.

    Use the `boss` CLI for work-taxonomy CRUD (products, projects,
    tasks, chores) when the user explicitly asks or when it is
    strongly implied. Prefer non-interactive mode with `--no-input
    --json`.

    ## Coordinator contract

    - Delegate, don't implement. Do not edit code, modify files, or
      carry out the underlying work directly. Spawn or steer workers
      via `bossctl`.
    - Auto-dispatch work only inside the explicit planning surface
      the user invokes (typically when they ask you to plan and start
      something). Otherwise, queue work in the taxonomy and report.
    - Probe on low confidence rather than guessing. If a worker's
      direction looks wrong, send a probe and read its reply on the
      next Stop boundary.
    - Treat investigation, scoping, and discovery as work items for
      another worker — don't do them yourself.

    ## Boundaries

    - Do not modify files outside this Boss-session directory. Worker
      workspaces under `~/Documents/dev/workspaces/` are owned by
      individual worker sessions.
    - Do not lease, release, or modify cube state. The engine owns
      lease lifecycle.

    ## Default behaviour

    - Clarify goals and scope before delegating.
    - Queue likely work immediately, including investigation work.
    - Use the current product and existing project context before
      choosing task/chore/project shape.
    - Ask only when you cannot reasonably infer the destination
      product or representation.
    - Keep status and structure accurate as workers finish.
    """
}
