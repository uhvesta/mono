# Incident 001 — PR fan-out kills running agents and closes wrong chores

- **Date:** 2026-05-14
- **Severity:** High — data loss (chores closed against wrong PRs) plus loss of in-flight work (running agents killed mid-turn).
- **Status:** Halted by human intervention. Investigation complete; remediation tracked under project P418.

## 1. Summary

On 2026-05-14 the Boss engine misattributed PR URLs to multiple work items and, as a side-effect of marking those work items "done," killed the workers that were still actively running against them. Three work items closed today (T404, T407, T415) were bound to PRs they had nothing to do with; an earlier batch the same morning fanned a single PR (#478) across four work items. None of the affected workers had ever pushed a branch or opened a PR — they were investigation-only sessions that nevertheless emitted `Stop` hooks at the end of every assistant turn.

The root cause is structural: the engine's cold-path PR-detection fallback (`detect_pr` → `jj_candidate_commit_shas` → `gh api commits/<sha>/pulls`) uses a jj revset that includes `bookmarks() & committer_date(after:"<dispatch_started_at>")`. Cube workspaces share a single `.jj/repo/store/git`, so every workspace's `jj log` sees every other workspace's recently-pushed bookmarks. The fallback runs on every `Stop` hook — i.e. every assistant turn — for any execution whose staged-URL cache is empty. The combination produced a per-turn race against every sibling worker's most recent push. When the race "won," `finalize_pr_transition` wrote the sibling's PR URL onto the wrong execution's work item, marked the chore done, and called `pane_releaser.release_pane(execution_id)` — which the macOS app's `releaseWorkerPane` honours by tearing down the worker's terminal pane, killing the running `claude` process.

The impact is bounded: the data loss is reversible by hand (move rows back to Backlog and clear `pr_url`), and no production system was affected. The damage to in-flight investigations was real but recoverable on a re-dispatch. The structural fix is the priority.

## 2. Timeline

All times UTC. Anchors come from `state.db` execution status transitions and from GitHub PR merge timestamps; gaps in the dispatch-events log around the May 14 window are noted under §7.

| Time | Event |
|---|---|
| 02:04:02 | PR #478 merged. |
| ~02:05:12–02:05:15 | Workers running on T404, T407, T409, T410 each receive a `release_pane` IPC from the engine within 70–73 seconds of the merge. Their executions transition to terminal status. Each row's `pr_url` is now #478. (Four-way fan-out — see §3.) |
| 06:22:43 | PR #483 merged. PR #483 is a markdown text-selection change. |
| ~06:24 | T415's execution is bound to #483 and killed. T415 is the work item titled "Engine PR-URL capture binds a single PR to multiple work items — fan-out closes wrong rows as done." |
| 06:38:10 | PR #486 merged. PR #486 is a Cmd-Q confirmation change. |
| ~06:39–06:40 | Workers running on T404 and T407 are bound to PR #486 and killed. T404 is "Sidebar toggle still visible on Agents tab — PR #469 fix incomplete"; T407 is "PR-URL capture binds the right URL to the wrong work_item — T404/T405 both got PR #475". |
| 06:40+ | Human notices three closed rows on the kanban whose bound PRs make no sense for their titles. Begins halting workers, clearing `pr_url` fields, moving rows back from Done to Backlog. |
| (later) | Halt complete. Affected work items repaired by hand. State.db inspection identifies `detect_pr` as the firing path. |

## 3. What we saw (observed effects)

The user-visible symptoms clustered into three:

- **Chores jumping Doing → Done with the wrong PR attached.** The kanban-rendered PR links pointed to PRs that bore no relation to the chore titles. A "sidebar toggle still visible on Agents tab" row showed PR #486 (Cmd-Q confirmation); a "PR-URL capture binds the right URL to the wrong work_item" row showed PR #486; an "Engine PR-URL capture binds a single PR to multiple work items" row showed PR #483 (markdown text selection). The chores' transcripts contained no `gh pr create` and no `jj git push` — they were investigation-only sessions.
- **Running agents killed mid-investigation.** Each mis-bind was followed within ~1 minute by the worker's pane disappearing from the macOS app's worker grid and its execution row transitioning to terminal status. The kills correlated tightly with sibling-PR merge times: the 02:05 cluster fired 70–73 seconds after PR #478 merged, and the 06:39 cluster fired 1–2 minutes after PRs #483 and #486 merged.
- **Investigation-only workers preferentially affected.** Workers that produce their own PR (do `gh pr create`, get their URL staged in the in-memory cache via the `PostToolUse` Bash hook) are protected: `on_stop_inner` (`tools/boss/engine/src/completion.rs:783`) consults the staged-URL cache before the fallback and short-circuits when a value is present. Investigation-only workers never populate the cache, so the cache lookup misses and the fallback fires. They also fire many `Stop` events without producing their own PR, so they have many windows for the fan-out to catch them. Every affected work item was an investigation; no implementing worker was affected.

The earlier 02:04 batch was a 4-way fan-out across T404a/b, T407a, T409, T410 — five rows, all reading PR #478. That is the most extreme fan-out we have evidence for; the 06:22 and 06:38 events were 1-way and 2-way respectively. The mechanism is the same; the fan-out factor depends on how many investigation-only workers happen to be active at the moment of a sibling merge.

### Per-execution timing

The kill-after-merge correlation is tight enough to be the smoking gun. Reading `work_executions.started_at` and `finished_at` out of `state.db` for each affected execution and lining them up against the GitHub merge timestamps:

| Execution | Started (UTC) | Finished (UTC) | Wrong-PR merged at (UTC) | Gap from merge → kill |
|---|---|---|---|---|
| T407a (`exec_18af4bc13c377510_1f`) | 01:54:45 | 02:05:12 | #478 — 02:04:02 | 70s |
| T404b (`exec_18af4c03a1f30350_22`) | 01:59:22 | 02:05:15 | #478 — 02:04:02 | 73s |
| T415 (`exec_18af5a98696c2860_2b`) | 06:26:31 | 06:39:38 | #483 — 06:22:43 | started 4 min after merge; killed on next fallback sweep |
| T407b (`exec_18af5aeb58f73a58_2d`) | 06:32:27 | 06:39:40 | #486 — 06:38:10 | 90s |
| T404c (`exec_18af5aeb59b21260_2f`) | 06:32:32 | 06:39:43 | #486 — 06:38:10 | 93s |

The 02:05 cluster (T407a finishing 3 seconds before T404b, on two different workspaces) and the 06:39 cluster (three executions across three workspaces all finishing within 5 seconds of each other) are the engine-wide fire of the fallback against multiple in-flight workers in the same sweep window. The T415 row is the slightly different shape that confirms the mechanism: T415 started *after* #483 was already merged, ran for 13 minutes doing its own investigation, and was killed when its next Stop hook fired the fallback and matched #483's still-recent bookmark.

### Per-execution worker output

The transcripts of the killed workers confirm that none of them ever produced a PR of their own. Counting tool-use messages from the on-disk `.jsonl` session files for each execution:

| Execution | `Bash` calls | `Read` calls | `Edit` calls | `gh pr create` / `jj git push` |
|---|---|---|---|---|
| T404a (mono-006) | 5 | 5 | 0 | 0 |
| T404b (mono-005) | 14 | 8 | 0 | 0 |
| T404c (mono-004) | 27 | 12 | 0 | 0 |
| T407a (mono-004) | 22 | 24 | 0 | 0 |
| T407b (mono-002) | 29 | 16 | 0 | 0 |
| T415 (mono-001) | 40 | 25 | 9 | 0 |

Every affected worker terminated mid-thought — the last assistant message in each transcript is investigatory, not a summary or hand-off. Five of the six executions never reached an edit phase; T415 had reached editing (9 `Edit` calls, adding regression tests to engine source) but had not run `gh pr create` or `jj git push` yet when killed. That is why the staging cache was empty for each of these executions: not because the engine missed a staging event, but because there was no staging event to miss. The fallback then ran as the only path the engine had, and ran it against jj state that, under cube's shared store, included sibling workers' just-pushed bookmarks.

## 4. Investigation and root cause

### The intended cold path

`on_stop_inner` (`tools/boss/engine/src/completion.rs:753`) runs on every `Stop` hook fired by a worker session. Its primary path is the worker-hook staging cache: if the worker has run `gh pr create` or similar while the engine was up, the resulting PR URL was captured by the `PostToolUse` Bash hook and parked in `self.staged_pr_urls`. When `Stop` fires, `on_stop_inner` consults the cache (`completion.rs:783`) and, on hit, finalises the transition against that URL without shelling out to anything.

The cold-path fallback is the part that fires when the cache is empty. It is intended to recover from one specific scenario: the engine restarted between the worker's `gh pr create` and its `Stop` hook, so the in-memory cache lost the URL but the worker's branch is still pushed and the GitHub-side PR still exists. The recovery is: ask jj for candidate commit SHAs in the workspace, then ask GitHub which PR each SHA belongs to (`gh api repos/:owner/:repo/commits/:sha/pulls`).

### Where it broke

The candidate-SHA query is built at `completion.rs:489` (`build_candidate_revset`) and produces the revset:

```
@ | @- | (bookmarks() & committer_date(after:"<dispatch_started_at>"))
```

The three clauses are meant to expand coverage from "just the working copy" to "the working copy plus its parent plus any bookmark recently pushed from this workspace." The bookmark clause is the one that matters: a worker that ran `jj new main` after `jj git push` would have `@` and `@-` on the new branch but the push's tip would only be reachable via its bookmark.

Cube workspaces share a single `.jj/repo/store/git`. The git store is the source of truth for bookmarks; the per-workspace `.jj/` is a thin checkout layer over it. `bookmarks()` in any workspace's `jj log` therefore returns every bookmark in the shared store, including bookmarks pushed by sibling workers running in parallel. The `committer_date(after:...)` filter narrows by time but does not narrow by workspace; a sibling worker's push committed in the same window passes the filter trivially.

Once the sibling's bookmark tip is in the SHA list, `gh api commits/<sha>/pulls` returns the sibling's PR; `finalize_pr_transition` (`completion.rs:974`) writes that URL onto the *current* execution's work item; the work item's `pr_url` is now wrong, its status is now `done`, and `pane_releaser.release_pane(execution_id)` (`completion.rs:1016`) is called to free the worker's slot. The macOS-side handler `WorkersWorkspaceModel.releaseWorkerPane` (`tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift:70-84`) clears the slot's session reference, SwiftUI tears down the Ghostty surface, and the worker's `claude` process dies. The investigation-only worker, which was still actively reasoning, is gone mid-turn.

### The per-turn firing multiplier

`on_stop_inner` proceeds for executions in `running` *or* `waiting_human` status (`completion.rs:767`). In Claude Code, the `Stop` hook fires after every assistant turn, not just at worker exit. A 30-turn investigation session emits 30 `Stop` events; each one is a chance for the fallback to fire, each one a fresh race against whatever bookmarks any sibling worker pushed in the intervening seconds. Investigation-only workers are the most exposed: they have no staged URL of their own, so the cache is empty every turn; the fallback runs every turn.

The shape of the incident — kills clustered ~60–90 seconds after sibling PR merges — is the predictable consequence: a sibling pushes, opens, and merges a PR within a couple of minutes; in that interval, every still-running investigation's next `Stop` event finds the sibling's recent bookmark in the revset and binds it. The narrower the merge-to-Stop interval, the more workers get caught.

### Why the signal cannot be made to work as-is

The bookmark clause's attribution signal is "recent bookmarks visible from this workspace." Under cube's shared-store architecture, that signal **is not unique to this workspace**. Tightening the time window does not help — the window only narrows the temporal slice; the cross-workspace visibility is constant. Filtering bookmarks by name does not help unless the names carry per-execution provenance that the detector can match. The flaw is structural, not parametric.

## 5. Action items

The action items below are reproduced verbatim from the running investigation list compiled on 2026-05-14. They are the authoritative output of this postmortem; implementation work for each item will be filed as separate chores/tasks against project P418 after this postmortem lands.

1. **Boss needs to be able to pull transcripts for executions that have completed.** Observed gap: `bossctl agents transcript` only works on live workers; the historical-fallback that `bossctl agents status` advertises returned `unknown run` for the dead execution. The on-disk transcript directory under `~/Library/Application Support/Boss/` is coordinator-off-limits. There's currently no supported surface for retrieving the conversation log of a completed worker, which blocks forensic investigation of mis-bind / wrong-PR incidents.

2. **There needs to be a one-step, atomic way to transition a row from Done (or any state with `autostart=true`) back to Backlog without the row being eligible for dispatch in between.** Observed gap: clearing PR + moving Done→Todo with `--no-autostart` doesn't clear the row's stored `autostart=true` column, so the reconciler can pick the row up and flip it to Doing again before the human notices. The only state empirically safe from re-dispatch is `blocked`, which lives in a different kanban lane than Backlog. Going via `blocked` as an intermediate step is also unsafe because each transition is observable by the reconciler. The fix doesn't have to be exactly "an `--unset-autostart` flag" — the requirement is that the *user-visible reset gesture* (drag from Done to Backlog, CLI verb, or whatever) is atomic with respect to the dispatch loop: the row must not be eligible for dispatch at any moment during or after the gesture, until the human explicitly releases it.

   **Addendum:** the simplest version of the fix is probably to clear `autostart` automatically the first time the row transitions to Doing (i.e. treat `autostart` as a single-shot "first dispatch" flag rather than a persistent property of the row). Once a task has been picked up once, the flag has served its purpose; leaving it set turns the flag into a re-dispatch hazard for every later state change. With this semantics, reopening a Done row (or moving anything back to Backlog) is safe by construction because `autostart` is already false from the original Doing transition.

3. **Log all IPC traffic between the engine and the macOS Boss app on disk for forensic investigation.** Observed gap: during this incident we wanted to verify that the engine actually sent `EngineToAppRequest::ReleaseWorkerPane` IPCs for each of the affected workers around the wrong-PR merge times. There's no log of IPC traffic anywhere on disk — neither the engine nor the app persists the requests/responses it exchanges over the engine→app socket. We can infer the IPCs were sent (from code-path reading + state.db terminal status + timing correlation with PR merges) but there's no direct evidence. A simple append-only `ipc.jsonl` on each side (or one shared log with a `direction` field) would make this class of investigation deterministic — we could replay exactly which slot got `ReleaseWorkerPane { slot_id, kill_grace_seconds }` and when.

4. **Investigate: engine-initiated `release_worker_pane` may not actually kill the `claude` process; the underlying pty stays allocated.** Observed gap (also surfaced in [`ui-performance-audit-2026-05-07`](../investigations/ui-performance-audit-2026-05-07.md)): `WorkersWorkspaceModel.releaseWorkerPane` in the macOS app just sets `slots[index].session = nil`. The Swift session reference goes away, but the libghostty C-level surface (PTY, scrollback, GPU resources) "remains allocated forever" per that investigation. The `killGraceSeconds` parameter is accepted but ignored in the app code. Net effect: the engine believes a worker has been killed (DB is in terminal state, IPC was sent and acknowledged), but the underlying `claude` process may continue running invisibly. Investigate the actual lifecycle, confirm whether the orphaned process is still doing API calls / making edits, and fix the kill path so it really kills.

5. **Optional / risk-bearing engine behaviours should live behind toggleable feature flags surfaced in an app debug setting.** Observed gap: the `detect_pr` cold-path fallback (the merge-poller / on_stop reconstruction path that runs `jj log` + `gh api commits/<sha>/pulls` whenever the staged-URL cache is empty) is what produced the PR fan-out in this incident. It's not strictly necessary for the happy path — the primary worker-hook staging path is sufficient when the worker pushes its own PR — but the fallback runs unconditionally any time staging is empty, and on this codebase its bookmark-clause revset crosses workspace boundaries via the shared cube jj store. There's no way to turn the fallback off without editing the engine source. The lesson generalises: any engine behaviour that is (a) optional-for-correctness, (b) carries non-trivial blast radius when wrong, and (c) is hard to reason about should be gated by a feature flag readable from a known config surface, and that flag should be toggleable from a debug pane in the macOS app without a rebuild. This lets the human kill a misbehaving path the moment they see it producing damage, instead of having to stop every worker and wait for the fix to land.

6. **The `detect_pr` cold-path fallback is fundamentally broken and needs to be either fixed or removed.** Observed gap: when the staged-URL cache is empty (engine restart between worker push and Stop, OR worker is `waiting_human` and never pushed), the engine's reconstruction path (`detect_pr` → `jj_candidate_commit_shas` → `gh api commits/<sha>/pulls`) is supposed to recover the PR URL. Its revset is `@ | @- | (bookmarks() & committer_date(after:"<iso>"))`. The third clause pulls **every** bookmark in the underlying jj repo whose committer date is after the execution's dispatch — but cube workspaces share a single `.jj/repo/store/git`, so bookmarks pushed by ANY concurrent worker are visible from EVERY workspace's `jj log`. The detector then matches a sibling worker's pushed branch to the current execution's chore and binds the wrong PR URL via `finalize_pr_transition`. That's the fan-out root cause for this incident (T404, T407, T415 all got wrong PRs through this path).

   The flaw is structural, not parametric: the identifying signal `detect_pr` uses (recent-bookmarks-visible-from-this-workspace) is **not unique to this workspace** under cube's shared-store architecture. Tightening the time window doesn't help; only narrowing on a per-execution attribution signal would.

   Candidate fixes (the postmortem should recommend one):
   - Drop the bookmark clause. Revset becomes `@ | @-` only. Lose recovery when a worker did `jj new main` after push, accept that loss as preferable to silent fan-out.
   - Per-execution bookmark naming. Require workers to push to a branch whose name encodes `execution_id` or `work_item_id`; have detect_pr filter the bookmark clause to that pattern. Adds a worker-side contract.
   - Remove the fallback entirely. Trust only the worker-hook staging path. Empty staging at on_stop = "this worker didn't push," no recovery attempt. Accept the loss of engine-restart-mid-push recovery as preferable to fan-out.
   - Move detection to GitHub-side scoping. Query `gh pr list --head <branch>` instead of `gh api commits/<sha>/pulls`, with `<branch>` carrying execution-id provenance.

   This is the highest-blast-radius item on the list — the engine is silently closing the wrong chores as done, which is data loss.

   **Per-turn fallback firing (the multiplier).** `on_stop_inner` proceeds for executions in `running` *or* `waiting_human` status — but in Claude Code the `Stop` hook fires after every assistant turn, not just at worker exit. So the fallback runs on every turn of every still-running worker whose staging cache is empty. That's dozens of fire-attempts per worker session, each one a race against any sibling worker's recent push to the shared jj store. This is why the 2026-05-14 mis-binds correlated tightly with sibling-PR-merge times even though the affected workers were still actively investigating: every turn-end was a chance for `detect_pr` to match a sibling's bookmark and kill the worker. Investigation-only workers were the most exposed — they fire many Stop events without producing their own staged URL, so they have many windows for the fan-out to catch them. The fix should also include: do not run `detect_pr` for an execution still in `running` status; reserve the fallback for genuinely-terminal cases.

7. **`boss project create` needs a way to skip the auto-generated `kind=design` seed task.** Observed gap: every `boss project create` invocation atomically files the project plus a `kind=design` task underneath it (`created_via=engine_auto`), and `--no-autostart` only suppresses dispatch of the seed task — it doesn't suppress *creation* of it. For projects that aren't design-shaped (a postmortem tracking project, a milestone aggregator, a checklist of action items, etc.) the seed task is awkward dead weight: either you repurpose it for something it wasn't designed for, or you delete it after creation. There should be a flag like `--no-design-task` that creates the project alone, no seed task spawned.

### Priority and ordering

AI #6 is a blocker for any engine work that depends on PR-binding correctness. While it is open, the engine is silently capable of closing the wrong chores as done and killing the wrong workers; every additional dispatch is exposure. Implement #6 first.

AI #5 (feature flags) and AI #6 are paired: if #5 lands first, the fix for #6 can ship under a flag, letting the broken behaviour be killed immediately while the structural fix is staged. AI #5 is therefore the lowest-cost-of-implementation way to reduce the live blast radius today.

AI #1 (transcripts for completed executions) and AI #3 (IPC traffic log) are forensic-tooling items. Neither prevents the next incident, but together they would have shortened *this* investigation by at least an order of magnitude — much of the timeline reconstruction was inferred rather than read. Land both before the next high-blast-radius engine change.

AI #2 (atomic reset-to-Backlog) and AI #4 (kill path actually kills) are correctness items that are independent of #6 but make recovery from a #6-class event safer. Implement after #6.

AI #7 (skip the auto-generated design task on project create) is low-priority surface polish, batched with other CLI work.

### Recommendation for AI #6

The four candidate fixes for AI #6 trade off differently against (a) recovery semantics when staging is empty, (b) the worker-side contract burden, and (c) the structural soundness of the attribution signal.

| Candidate | Recovers engine-restart? | Worker contract burden | Structural soundness |
|---|---|---|---|
| 1. Drop bookmark clause | Only if worker's `@`/`@-` still names the pushed commit | None | Sound — but the signal is weaker, so recovery is best-effort. |
| 2. Per-execution bookmark naming + filter | Yes | Worker must push to a deterministic branch name | Sound — signal is unique by construction. |
| 3. Remove fallback entirely | No | None | Sound — no signal, no misattribution. |
| 4. GitHub-side scoping by branch name | Yes | Worker must push to a deterministic branch name | Sound — signal is unique by construction. |

**Recommendation: combine #2 and #4.** The engine should require workers to push to a deterministic branch name that encodes `execution_id` (or `work_item_id` — either uniquely identifies the binding the engine wants to write), and the detector should switch from `gh api commits/<sha>/pulls` (a SHA-keyed query that depends on local jj state being attributable) to `gh pr list --head <branch>` (a branch-keyed query that depends only on the GitHub-side branch name). The combined fix gives a per-execution unique attribution signal that is reconstructible from `state.db` alone — no local jj reads, no shared-store contamination, and no loss of engine-restart recovery.

The justification:

- **The signal is unique by construction.** A branch name derived from `execution_id` is generated by the engine and stored on the execution row; sibling workers running in other workspaces have different execution IDs and therefore push to different branches. There is no cross-execution overlap to exploit. This addresses the structural flaw directly: the bookmark visibility problem disappears because the detector no longer reads bookmarks at all.
- **The fallback survives engine restart.** A SHA-keyed detector needs the local jj state to be attributable. The expected-branch name is reconstructible from `state.db` (which is durable), so the detector can run after a restart with no dependence on the worker's local jj state. Candidate #3 (remove the fallback) cannot recover this case; candidate #1 (drop the bookmark clause) recovers it only when the worker's `@`/`@-` happens to be the pushed commit, which is a narrower window.
- **The contract is light and natural.** Workers already create named bookmarks at PR-creation time. The current convention is human-chosen branch names; the engine simply needs to dictate the name. The existing CLAUDE.md instructions already include `jj bookmark create <name> -r @`; the engine writes the required name to `.claude/initial-prompt.txt` (or surfaces it via an environment variable on the worker) and the worker uses it. This is a one-line change to the worker prompt and the existing PR-creation recipe.
- **GitHub-side scoping is a single existing CLI verb.** `gh pr list --head <owner>:<branch> --state open` returns at most one PR (GitHub enforces one open PR per head branch). The detector's logic collapses from "list candidate SHAs → for each SHA query its PRs → pick" to "look up the one PR for this expected branch." Simpler is correct.
- **It composes with the per-turn-firing fix.** The AI itself notes that `detect_pr` should not run for executions in `running` status — only at genuinely terminal events. Combining that gate with branch-keyed detection narrows the firing window further: the detector runs once per execution lifetime (or at most a handful of times under merge-poller sweeps), and even when it runs, it cannot match a sibling.

Candidate #1 alone is appealingly small but leaves the engine-restart recovery story strictly worse than today. Candidate #3 sacrifices the recovery story entirely; the AI text explicitly accepts this trade-off, but the combined #2+#4 fix preserves the recovery without re-introducing the cross-workspace contamination, so we should not pay the recovery cost unless we have to. Candidate #2 alone (without #4) keeps the SHA-keyed query and therefore keeps a local-jj dependency we no longer need; #4 alone (without #2) leaves the worker side unbound. The combination is the minimum that is both sound and complete.

The implementation cost is modest: a deterministic branch-name helper on the engine side, a small change to the worker spawn prompt to inject the expected name, replacement of `jj_candidate_commit_shas` + `query_pr_for_commit` with a single `gh pr list --head` call, plus the `running`-status gate in `on_stop_inner`. The combined diff should be confined to `tools/boss/engine/src/completion.rs` plus the prompt-construction site in `tools/boss/engine/src/runner.rs`.

## 6. What went well

Investigation was possible at all because Boss's coordinator-side state stores have enough independent records to triangulate against each other:

- **`state.db` is durable and queryable.** The execution rows for each affected worker recorded `started_at`, `pr_url` (post-mis-bind), and the terminal status transition; the work item rows recorded their bound `pr_url` and `status`. Cross-referencing the two against GitHub's merge timestamps reconstructed the timing without needing live worker output.
- **Dispatch-events log and engine-audit log overlapped.** Where dispatch-events had gaps (see §7), engine-audit had records of the IPC calls that fired around the same time, and vice versa. Neither would have been sufficient alone.
- **On-disk transcripts existed for the killed workers.** The transcripts were not accessible through any supported surface (see AI #1), but they existed on disk, and reading them confirmed that no affected worker had ever invoked `gh pr create` or `jj git push`. That confirmed the staged-URL cache was empty for the right reason — not "we missed staging it" but "there was nothing to stage."
- **The fallback path was small enough to read.** `on_stop_inner` + `detect_pr` + `jj_candidate_commit_shas` + `build_candidate_revset` fits in roughly 200 lines of `completion.rs`. The mechanism was identifiable by inspection within an hour of pulling the file open.
- **GitHub's API was internally consistent.** `gh api commits/<sha>/pulls` returned the same (wrong-from-our-perspective) PR for each affected SHA, so the failure mode was deterministic rather than flaky. That made the rest of the investigation a deduction rather than a guessing game.

## 7. What went badly

The investigation surfaced specific gaps in the tooling that mattered most:

- **No log of engine↔app IPC traffic.** Verifying that `ReleaseWorkerPane` was actually sent (vs. inferred) required reading code paths and trusting the timing correlation. A persisted `ipc.jsonl` on either side of the socket would have made the question a `grep`. (AI #3.)
- **Transcript directory is coordinator-off-limits.** Workers cannot read `~/Library/Application Support/Boss/`, so even though the killed workers' transcripts were on disk, they were not reachable from a worker session. `bossctl agents transcript` only works on live workers, so once the executions were terminal, the transcripts were forensically out of reach without manual coordinator-side intervention. (AI #1.)
- **Dispatch-events log had gaps in the May 14 window.** Several minutes around 02:04 and 06:38 were missing events that should have appeared based on the state.db transitions. The cause is independent of the incident but compounded the difficulty: the timeline had to be reconstructed from multiple sources rather than read off a single stream.
- **No way to disable the fallback in flight.** Once the mechanism was identified, the only options were "stop every worker" or "wait for a patch to land." A feature-flag surface would have allowed killing the fallback the moment we saw it producing damage. (AI #5.)
- **State reset is racy against the reconciler.** Moving the affected rows from Done back to Backlog had to be done via `blocked` as an intermediate step to avoid the autostart-driven re-dispatch hazard, and even that was not strictly safe. The right gesture should be atomic against the dispatch loop. (AI #2.)
- **Uncertainty about whether the kills actually killed.** The pane-teardown path in the macOS app does not appear to free the underlying libghostty surface or signal the worker's `claude` subprocess (cross-referenced with the 2026-05-07 UI performance audit). It is therefore unclear whether the affected `claude` processes were killed or merely orphaned. (AI #4.)

## 8. Lessons and generalisations

This incident fits three repeatable patterns:

- **Optional-but-always-on fallbacks are a liability.** The `detect_pr` fallback was designed for one narrow recovery scenario (engine restart between worker push and Stop) but fired unconditionally any time the primary path's cache was empty. The set of "cache empty" reasons is much larger than the set of "engine restarted" reasons — investigation-only workers (never push), `waiting_human` workers (paused before pushing), workers that pushed via a path the hook did not catch, etc. A fallback that fires more broadly than its design intent inherits more blast radius than the design accounted for. The general lesson: any fallback should fire only on the specific condition it is designed to handle, not "any case where the primary path returned nothing." If you cannot detect the specific condition, the fallback is over-scoped.
- **Shared-state assumptions silently break when state becomes shared.** The `detect_pr` revset was written when each worker had its own `.jj/repo/store/git`. Cube introduced a shared store; the revset's bookmark clause is correct under the old assumption and unsound under the new one. There was no test, no comment, and no contract that flagged the assumption when cube landed. The general lesson: when introducing a new layer that changes the multiplicity of something (one store → many workspaces sharing one store), every existing read against the old singular thing needs to be re-audited for the new plural reality. A grep for "is this query workspace-scoped?" against the engine at the time cube landed would have caught this.
- **Per-row flags that persist beyond their first use become re-dispatch hazards.** `autostart=true` on a `tasks` row is meant to say "this row should be picked up at creation." Leaving it set after the row has been picked up turns it into "this row will be picked up again if anything moves it back to Todo." The general lesson: per-row flags should be either explicitly single-shot (cleared on first consumption) or explicitly idempotent (safe to re-fire). A flag that lives somewhere in between accumulates re-firing hazards on every subsequent state transition. (AI #2's addendum makes exactly this argument and is the right shape of fix.)

A fourth, smaller pattern: **per-turn hook events should be treated as a rate, not as a one-shot.** The `Stop` hook fires every turn in Claude Code, which means any work it triggers runs at the rate of turns, not at the rate of worker completions. Designs that assume `Stop` means "the worker exited" will fire dozens of times per worker session. The `detect_pr`-on-running-execution misuse is one instance; future hook-driven engine work should explicitly state which rate it is designed for.

---

*This is the first postmortem in `tools/boss/docs/postmortems/`. Future incidents follow the `incident-NNN-<slug>.md` naming convention.*
