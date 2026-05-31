# Design: Automated Reviewer Pass on Every Agent-Authored PR

## Status

Proposed

## Problem / goal

Today an agent-authored PR ships straight to the Review column with no
independent check. The producing worker is the only agent that ever looks
at its own diff, and self-review is weak: workers rationalize their own
choices. We have already been bitten by this — in the #1043 / T793
forward-port, a worker silently dropped a live feature during conflict
resolution and then rationalized the deletion as intentional. Nothing in
the pipeline caught it.

We want a mandatory, independent reviewer pass: every time a worker
**creates or updates** a PR, a *separate* dedicated reviewer agent reviews
that PR read-only and produces high-bar, actionable feedback. If the
reviewer has feedback, it becomes a Boss **revision** on the original task.
Feedback stays **internal to Boss** — it is never posted to GitHub.

The goal is to raise the quality of all produced PRs (correctness,
architecture, readability, tests, edge cases, and specifically
inadvertent regressions from conflict resolution / forward-ports) while
**balancing speed** — a review pass must not excessively lengthen PR
turnaround.

## Goals

- **Independent review of every agent-authored PR.** On every PR create or
  update by a worker, a separate reviewer agent reviews the diff.
- **High-bar, actionable feedback** across correctness/bugs, architecture,
  code quality & readability, test coverage, edge cases, and — as a
  first-class explicit check — **inadvertent deletions / regressions**
  introduced during conflict resolution or forward-ports.
- **Feedback stays internal.** Review output becomes a Boss revision task,
  never a GitHub comment. No internal taxonomy, jargon, or blunt critique
  leaks onto public GitHub surfaces.
- **A dedicated reviewer worker pool**, modeled on the existing automation
  pool, always running at **Opus** level regardless of the reviewed task's
  effort, with its own execution kind and dispatch routing.
- **Read-only enforcement.** The reviewer cannot mutate the PR/branch — by
  prompt mandate, by tool denylist, and by not depending on workspace
  writes.
- **Bounded cost and latency.** Explicit, tunable cap on review→revision
  cycles; skip re-review of no-ops; prefer fast high-signal feedback over
  exhaustive analysis.
- **Reviewer visibility** via a dedicated page on the macOS app's agents
  view, mirroring the automation-pool page.

## Non-goals

- **Posting review feedback to GitHub.** Explicitly out of scope and, in
  fact, prohibited. This design deliberately sidesteps the deferred
  "external PR comments" mechanism by keeping all feedback inside Boss as
  revisions. (See *Alternatives considered*.)
- **Replacing human review or CI.** The reviewer pass is an additional,
  internal quality gate before a PR reaches the Review column for a human;
  it does not gate merge, does not run tests itself, and does not replace
  the existing CI green-ness check the engine already performs.
- **Reviewing non-agent / human-authored PRs.** Scope is agent-authored
  PRs produced by Boss workers only.
- **Reviewing design / investigation doc PRs for code quality.** These are
  in scope for a *light* review (see *Which PRs are in scope*) but the deep
  code rubric (tests, regressions, architecture) does not apply to a pure
  markdown deliverable.
- **A general-purpose, configurable review-rubric DSL.** v1 bakes the
  rubric into the reviewer prompt (mirroring how `editorial.rs` baked in
  defaults first). A configurable policy layer is future work.
- **Fixing the editorial control layer (P576) or the revision nudge-loop
  bug (T955).** This design *depends on / coordinates with* those but does
  not subsume them.

## Alternatives considered

### A1 — Post review feedback as GitHub PR comments (rejected)

The most obvious design: have the reviewer post inline/PR comments on the
GitHub PR, and let the producing worker pick them up like human review
comments (the `"check comments"` flow).

Rejected because:
- It **leaks internal review chatter** (Boss taxonomy, blunt critique,
  internal reasoning, severity labels) onto a public GitHub surface that
  other humans see. This is the exact concern that originally caused us to
  *defer* the whole feature.
- It creates a hard dependency on a mechanism for attaching comments
  *outside* GitHub that does not yet exist.
- It would conflict with the editorial controls (P576), which exist
  precisely to police what agents say on GitHub.

The internal-revision approach gets the quality benefit with none of the
leakage, and needs no new GitHub-comment infrastructure.

### A2 — Self-review in the producing worker (rejected)

Add a "now review your own diff against main" step to the producing
worker's prompt before it opens the PR.

Rejected because:
- **Self-review is weak.** The producing worker is the same agent that
  made the choices; it rationalizes them. The T793 incident is a concrete
  case where a worker defended a deletion it should have flagged.
- It provides no model-level independence and no separate, higher-tier
  (Opus) perspective.
- It muddies the producing worker's role and makes its prompt longer and
  its turn slower with no independent check.

A separate agent with a fresh context and a review-only mandate is the
point.

### A3 — A single shared worker pool with a review execution kind (rejected)

Add a `pr_review` execution kind but dispatch it to the *existing* general
worker pool rather than a dedicated pool.

Rejected because:
- Reviews would **compete with production work** for the same slots,
  coupling review latency to production backlog and vice versa.
- We could not give reviewers a **per-pool Opus model override** cleanly —
  model selection is currently effort-driven per task; a dedicated pool is
  the natural place to force a model regardless of effort (exactly how the
  automation pool is modeled).
- Reviewer visibility (its own agents-view page) and per-pool exhaustion
  metrics fall out naturally from a dedicated pool.

The automation pool (T793 / #1043) already established the
"second dedicated pool + dispatch routing + per-pool model" pattern. The
review pool is a third pool following the same blueprint, which is why
this alternative is rejected in favor of the *Chosen approach*.

### A4 — Review only on PR *create*, not on update (rejected)

Cheaper: review the first PR a worker opens, but skip review of
subsequent updates (including revisions).

Rejected because:
- The T793 regression class can be *introduced by an update* (a
  forward-port / conflict-resolution push onto an existing PR), so
  skipping update-review would miss exactly the incident we want to catch.
- However, we keep the *cost* concern by **skipping no-op / trivial
  updates** (see *No-op skipping*) rather than skipping all updates. That
  gives most of the savings without the blind spot.

## Chosen approach

A third, dedicated **review worker pool** running always-Opus, fed by a new
`pr_review` execution kind. When a worker's PR is created or updated, the
engine enqueues a `pr_review` execution targeting the review pool. The
reviewer reads the PR read-only, emits structured findings, and — if and
only if it has actionable feedback above the severity bar — the engine
creates a **revision task** on the *original producing task*, carrying the
feedback as the revision instructions. The revision is dispatched to the
normal worker pool, updates the PR, and (subject to the cycle bound) is
itself reviewed again. When the reviewer returns no actionable feedback,
or the cycle bound is hit, the PR proceeds to Review unimpeded.

```
worker opens/updates PR
        │  (pr_url capture hook detects create/update)
        ▼
engine enqueues pr_review execution ──► REVIEW POOL (Opus, read-only)
        │                                      │
        │                              structured ReviewResult
        ▼                                      │
   no actionable feedback?  ◄──────────────────┤
        │ yes → PR proceeds to Review          │ revision_warranted = true
        │                                      ▼
        │                          engine creates REVISION on original task
        │                          (feedback = revision_instructions,
        │                           pr_url inherited — see T955)
        │                                      │
        └──────────────  GENERAL POOL ◄────────┘
                         worker revises, pushes update → loops back to top
                         (bounded by max review cycles)
```

### 1. Trigger / hook

The trigger is the existing **pr_url capture path** in the engine
(`engine/src/pr.rs` / the hook plumbing in `engine/src/hooks.rs`) that
already fires when a worker's session results in a PR being created or
updated. We hook the *same* detection point used to move a task into
Review and to set/update `pr_url`.

On a detected PR create-or-update for an agent-authored task:

1. Resolve the producing task, its `pr_url`, and the current head SHA.
2. Run the **no-op / scope gate** (see *No-op skipping*). If it says
   "don't review", record the skip reason and let the PR proceed.
3. Otherwise enqueue a `pr_review` execution carrying:
   - `target_task_id` (the producing task),
   - `pr_url`,
   - `base` (merge base / `main`) and `head` SHA,
   - `review_cycle` (the current cycle counter for this PR),
   - the producing task's title/description for context.

The `pr_review` execution is routed to the review pool (see *Pool &
routing*). The producing task does **not** advance to "human Review" until
the review pass resolves (either no-feedback, or the resulting revision is
itself reviewed and clears, or the cycle bound is hit). This keeps the
"mandatory pass" guarantee while the bound keeps it terminating.

> Note: the trigger fires on the producing worker's completion regardless
> of whether the PR was created *or* updated — because updates (e.g.
> forward-ports) are exactly where the regression class we care about
> appears (A4).

### 2. Reviewer role & prompt

The reviewer worker is spawned with a **review-only** system prompt. Full
structure:

**Role & hard mandate (read-only):**

> You are an independent PR reviewer. Your ONLY job is to review the diff
> and return structured feedback. You MUST NOT change the PR in any way:
> no commits, no pushes, no `gh` writes, no edits to the branch or any
> file, no comments on GitHub. You operate read-only. Anything you would
> "fix" you instead describe as a finding. Posting to GitHub is
> prohibited — your feedback stays inside Boss.

This mandate is reinforced at the tooling level (see *Read-only
enforcement*).

**What to review (inputs):** the PR diff against the merge base / `main`,
the producing task's stated purpose, and the changed files in context
(read access to the workspace checkout is allowed; writes are denied).

**High-bar, actionable rubric.** Push back, with a HIGH bar and VERY
actionable feedback, on:

- **Critical correctness issues / bugs** — logic errors, broken
  invariants, mishandled errors, race conditions.
- **Inadvertent deletions / regressions** *(first-class, explicit check)*
  — diff against the merge base / `main` and flag anything dropped that is
  unrelated to the PR's stated purpose. Conflict-resolution and
  forward-port PRs get extra scrutiny here. This is the T793 check: a live
  feature silently removed during a forward-port must be caught.
- **Architectural improvements** — wrong layer, missed reuse, abstraction
  that fights the codebase's conventions.
- **Code quality & readability** — match surrounding style; naming;
  dead/confusing code.
- **Test coverage gaps** — untested new behavior, missing edge-case tests.
- **Edge cases & gotchas** — boundary conditions, nullability, concurrency,
  failure modes.

**Actionability requirement:** every finding must name a file (and line/
hunk where possible) and state concretely what to change. "Consider
improving error handling" is not acceptable; "in `pr.rs` the `?` on the
gh call swallows the 422 — handle the duplicate-PR case explicitly" is.

**Speed / comprehensiveness balance (explicit in prompt):**

> Prefer fast, high-signal feedback over exhaustive analysis. Every PR may
> now pass through up to ~3 produce→review→revise cycles, so do NOT
> excessively lengthen turnaround. If in doubt about a non-critical
> suggestion, you MAY offer it WITHOUT deep analysis and mark it
> low-severity — the downstream revision worker decides whether to apply
> it. Spend your scrutiny budget on correctness and regressions first.

Reviewer effort is bounded (see *Loop termination & bounds* and *Cost*):
a soft turn/time budget, and the structured output forces the reviewer to
converge on a verdict rather than rambling.

### 3. Structured reviewer output (`ReviewResult`)

The reviewer is required to emit a single structured result (enforced via
the worker's structured-output mechanism, not free text). Shape:

```jsonc
{
  "pr_url": "https://github.com/...",
  "head_sha": "abc123",
  "summary": "one-paragraph overall read",
  "revision_warranted": true,            // the decision bit
  "findings": [
    {
      "severity": "critical | high | medium | low",
      "category": "correctness | regression | architecture | readability | tests | edgecase",
      "file": "tools/boss/engine/src/pr.rs",
      "location": "fn ensure_pr, ~L120",  // best-effort
      "title": "Forward-port dropped the autostart feature",
      "detail": "Concrete description + what to change.",
      "confidence": "high | medium | low"  // low = suggestion, apply at revisor's discretion
    }
  ],
  "regression_check": {                    // first-class, always present
    "performed": true,
    "suspected_deletions": [ /* findings of category=regression */ ]
  }
}
```

- `revision_warranted` is computed by the reviewer but **gated by the
  engine** against a severity bar (see *Bounds*): the engine creates a
  revision only if there is at least one finding at/above the configured
  threshold (default: any `critical`/`high`, or a `regression` of any
  severity). `low`-confidence suggestions alone do **not** force another
  cycle.
- `regression_check.performed` must be `true` — the reviewer cannot skip
  the deletion check; if it has no signal it returns an empty list with
  `performed: true`.

### 4. Feedback → revision (internal, never GitHub)

If the engine decides (from `ReviewResult` + severity gate) that a
revision is warranted, it creates a **revision task** on the **original
producing task**, exactly as in the revision-tasks design:

- `parent_task_id` = the producing task.
- `revision_instructions` = a rendered, human-readable digest of the
  qualifying findings (title + detail + file/location, grouped by
  severity). This is the *internal* feedback; it lives in Boss only.
- `pr_url` = the producing task's `pr_url`, **explicitly inherited** so the
  revision does not strand in the T955 nudge-loop (see *T955 interaction*).
- provenance: mark the revision as `source = pr_review` (extends the
  unify-pr-remediation-on-revisions taxonomy of remediation sources:
  CI, human, automated reviewer).

The revision is dispatched on the **general** worker pool (not the review
pool — revising is production work). It resumes the branch, applies the
feedback, and pushes an update → which re-triggers the pr_url capture hook
→ which (subject to the cycle bound and no-op gate) enqueues another
`pr_review`. The loop converges when the reviewer returns no qualifying
findings or the bound is hit.

If no revision is warranted, **nothing is created** and the PR proceeds to
Review. The reviewer never writes to GitHub in either case.

### 5. Review worker pool + execution kind + dispatch routing

Modeled directly on the automation pool (T793 / #1043). The automation
pool added a *second* pool with: `WorkerPool::new_automation`,
`set_automation_pool` on engine state, `execution_targets_automation_pool`
for routing, per-pool exhaustion in `drain_ready_queue`, and
`release_worker_and_kick` routing by worker-id prefix. The review pool is
the **third** pool following the same blueprint:

- **Pool construction:** add `WorkerPool::new_review(...)` and a
  `review_pool` slot on engine state with a `set_review_pool` setter,
  parallel to the automation pool. Worker ids carry a distinct prefix
  (e.g. `review-`) so release routing can tell pools apart.
- **Execution kind:** add `pr_review` to the execution-kind enum
  (`protocol/src/types.rs`), parallel to `automation_triage`.
- **Routing:** add `execution_targets_review_pool(kind)` (parallel to
  `execution_targets_automation_pool`) returning true for `pr_review`.
  `drain_ready_queue` gains a third per-pool ready slice and drains the
  review pool against its own slots with its own exhaustion handling.
  `release_worker_and_kick` recognizes the `review-` worker-id prefix and
  kicks the review pool.
- **Per-pool model override (always Opus):** the automation pool already
  establishes a per-pool model override that ignores task effort. The
  review pool sets its override to **Opus** unconditionally. Model
  selection for an execution checks the owning pool's override before
  falling back to the effort-driven default. This is the single most
  important per-pool property: reviewers are always Opus regardless of the
  reviewed task's effort.
- **Pool sizing:** the review pool gets its own configurable slot count
  (config in `engine/src/config.rs`). Default small (e.g. 1–2) — reviews
  are short and we want to bound concurrent Opus spend; tunable.

This isolates review latency and cost from production work and gives
reviewers their own exhaustion metrics and visibility.

### 6. Agents-view page (macOS app)

Add a **Reviewers** page/section to the agents view, mirroring the
automation-pool page. It lists the review pool's workers, their current
PR-under-review, cycle number, and recent verdicts. This reuses the
automation-pool page's view/model wiring; the only new pieces are the pool
identity and the review-specific columns (PR, cycle, verdict). Source
lives alongside the existing automation-pool page in the app's agents
view module.

### 7. Loop termination & bounds

The produce→review→revise loop must terminate. Termination conditions
(first to fire wins):

1. **Reviewer approves** — `ReviewResult` has no findings at/above the
   severity gate → no revision → PR proceeds. (Normal, desired exit.)
2. **Max review cycles per PR** — a tunable `max_review_cycles` (default
   **3**, matching the operator's "~3 cycles at worst" mental model). The
   `review_cycle` counter is tracked per PR (on the producing task). When a
   `pr_review` would be the `(max+1)`th, the engine **skips review**,
   records "cycle bound reached", and lets the PR proceed to human Review
   with the last unresolved findings attached internally for the human.
3. **Convergence / severity decay** — if successive reviews only produce
   findings *below* the severity gate, no revision is created (this is just
   case 1 applied each cycle). Optionally, raise the gate slightly each
   cycle so late cycles only block on genuinely critical issues — keeps
   late cycles from ping-ponging on stylistic nits. (Tunable; default off
   for v1, gate constant.)

All three knobs (`max_review_cycles`, severity gate, per-cycle gate decay)
live in `engine/src/config.rs` and are tunable without code changes.

### 8. No-op skipping

Re-reviewing a pure rebase / no-op / trivial change wastes an Opus turn.
Before enqueuing a `pr_review`, the engine runs a cheap gate:

- **Empty / pure-rebase diff against the previously-reviewed head:** if the
  effective diff (new head vs. last-reviewed head) is empty or only a
  rebase onto a new base with no content change → **skip**.
- **Trivial-size diff:** below a tiny threshold (e.g. only whitespace, or
  < N changed lines in non-test files) → skip and let it proceed (tunable;
  conservative default so we don't skip a one-line correctness fix).
- **No new content since last review of this PR:** track `last_reviewed_sha`
  on the producing task; if head hasn't meaningfully advanced, skip.

Skips are logged with a reason and surfaced on the Reviewers page so
"why wasn't this reviewed" is answerable. The first review of a PR is
never skipped by the trivial-size rule (we always do at least one pass).

### 9. Read-only enforcement

Three layers:

1. **Prompt mandate** (section 2): explicit, repeated read-only / no-GitHub
   instruction.
2. **Tool denylist** (primary enforcement): the reviewer worker kind is
   spawned with write/push tools denied — no `Edit`/`Write`, no `jj`/`git`
   push, no `gh` write subcommands, no `cube pr ensure`. Read/search tools
   and read-only `gh`/`jj` (diff, view, log) are allowed. This requires a
   per-worker-kind tool allow/deny capability in the worker spawn path; if
   one does not already exist it is a prerequisite implementation task
   (see breakdown). This is the *enforceable* guarantee, not just a polite
   request.
3. **No-write workspace posture:** the reviewer reads the PR via
   `gh pr diff` / a read-only checkout and does not need workspace mutation
   at all. If feasible, run the reviewer against a read-only view so even an
   attempted write fails. (Defense in depth; the denylist is the contract.)

### 10. Cost & latency analysis

**Per-PR added cost.** One Opus review per qualifying PR update. With the
no-op gate, the *number* of reviews ≈ number of meaningful pushes, not
every push. Worst case per PR ≈ `max_review_cycles` (default 3) Opus
reviews + up to 2 extra revision turns by the general pool.

**Latency.** Each cycle adds: review-pool queue wait + one Opus review
turn (reviews are short and bounded — diff-reading, not building). With a
dedicated pool sized to keep queue wait low, the dominant added latency is
review turn time, not queueing. The speed/comprehensiveness balance
(section 2) caps review depth so a review is closer to a fast triage than a
full investigation.

**Controls that bound cost** (all tunable in `config.rs`):
- `max_review_cycles` (default 3) caps cycles per PR.
- No-op / trivial-diff gate avoids re-reviewing rebases and tiny changes.
- Severity gate avoids spawning revisions (and thus further reviews) for
  low-severity nits.
- Review-pool slot count caps concurrent Opus spend.
- Always-Opus is scoped to the review pool only; production effort/model
  selection is unchanged.

**Expected steady state.** Most PRs: 1 review, 0–1 revision. Problem PRs:
up to 3 reviews. This matches "raise quality without excessively
lengthening turnaround."

### 11. Interaction with other work

- **T955 (revision nudge-loop).** Review-generated revisions are exactly
  the case T955 warns about: a revision whose `pr_url` is null strands in
  a "produce a PR" nudge loop. This design **depends on** T955's fix and,
  belt-and-suspenders, always sets the revision's `pr_url` from the parent
  (section 4). The review feature must not ship before T955 is fixed, or it
  will amplify the loop. Sequenced accordingly in the task breakdown.
- **P576 / editorial controls.** `editorial.rs` governs what agents say on
  GitHub. The reviewer **never** talks to GitHub, so the two do not
  conflict — but to be safe the reviewer worker kind is *exempt by being
  denied GitHub-write tools entirely* (section 9), so there is no
  GitHub-facing action for the editorial evaluator to police. We confirm
  the editorial hook is not invoked on the reviewer's (nonexistent)
  gh actions. The revision-instructions text is internal and never flows
  to an editorial-evaluated surface.
- **unify-pr-remediation-on-revisions.** The reviewer is a new remediation
  *source* (`pr_review`) feeding the same revision mechanism that designates
  CI/human/automated sources. This design slots into that taxonomy rather
  than inventing a parallel path.

### Which PRs are in scope

- **In scope:** all agent-authored code PRs produced by Boss workers, on
  both create and update.
- **Light scope:** design / investigation doc PRs (pure markdown) get a
  light review — structure, completeness, internal consistency,
  and the required-sections check for design docs — but not the code
  rubric (tests/regressions/architecture). The reviewer detects a
  docs-only diff and switches rubric.
- **Out of scope:** human-authored PRs; the reviewer's own output
  (reviewers produce no PRs, so there is nothing to recursively review).

## Risks / open questions

1. **Mandatory-pass vs. throughput.** Holding the producing task out of
   human Review until the review pass resolves adds latency to *every* PR.
   Is the default `max_review_cycles = 3` the right ceiling, and should the
   *first* review be blocking while later cycles are best-effort
   (non-blocking) to protect throughput? **Needs an operator decision.**
2. **Severity gate calibration.** What exactly forces a revision? Proposed
   default: any `critical`/`high` finding, or any `regression` finding.
   Too loose → churn; too tight → misses. Needs tuning against real PRs.
3. **No-op gate aggressiveness.** Skipping trivial diffs risks skipping a
   one-line correctness fix. The conservative default skips only
   whitespace/rebase no-ops. Confirm the threshold.
4. **Per-worker-kind tool denylist.** Does the worker spawn path already
   support denying write/push tools per worker kind? If not, that is a
   prerequisite task and the riskiest unknown for read-only enforcement.
   **Needs codebase confirmation.**
5. **Cost ceiling.** Always-Opus on every PR (× up to 3 cycles) is the
   dominant new cost. Is a hard per-day review-budget / circuit-breaker
   needed in v1, or do pool-size + cycle caps suffice?
6. **Cycle-bound exit UX.** When the cycle bound is hit with unresolved
   findings, the PR proceeds to human Review with findings attached
   internally. Where do those surface for the human — the task detail, the
   Reviewers page, or both?
7. **Reviewer reading the diff.** Read the diff via `gh pr diff` (needs
   read-only gh auth) vs. a local read-only checkout (needs the branch
   present in a workspace). Which is the canonical input? Affects the
   read-only workspace posture (section 9.3).
8. **Interaction with stacked PRs.** For stacked PRs (auto-rebase design),
   does each PR in the stack get its own review, and how is the merge base
   computed per-stack-entry? Likely "review each PR against its stack
   parent," but confirm.

## Proposed implementation task breakdown

Tasks are PR-sized and listed in dependency order. Depth annotations note
what may run in parallel. Deferred items are marked
`future / not a v1 blocker`.

1. **Confirm/add per-worker-kind tool denylist** — *Depth 0*
   - **Scope:** Verify whether the worker spawn path can deny specific
     tools (Edit/Write, push, gh-write, cube pr) per worker kind. If the
     capability exists, document it; if not, add it. This is the
     enforcement substrate for the reviewer's read-only mandate.
   - **Effort:** medium
   - **Dependencies:** none

2. **Land T955 fix (revision pr_url nudge-loop)** — *Depth 0*
   - **Scope:** Ensure revisions always carry the parent's `pr_url` and a
     PR *update* (not just create) satisfies the "PR exists" condition, so
     auto-generated revisions don't strand. Tracked as T955; this design
     hard-depends on it. (May already be in flight — if landed, this entry
     is a verification step.)
   - **Effort:** small
   - **Dependencies:** none

3. **Add `pr_review` execution kind** — *Depth 0*
   - **Scope:** Add the `pr_review` variant to the execution-kind enum in
     `protocol/src/types.rs` and thread it through serialization/DB. Pure
     additive plumbing, no routing yet.
   - **Effort:** trivial
   - **Dependencies:** none

   *Depth 0 tasks (1, 2, 3) may run in parallel.*

4. **Add the review worker pool + dispatch routing** — *Depth 1*
   - **Scope:** Add `WorkerPool::new_review`, a `review_pool` state slot +
     `set_review_pool`, `review-` worker-id prefix,
     `execution_targets_review_pool`, per-pool exhaustion in
     `drain_ready_queue`, and `release_worker_and_kick` routing for the
     review prefix — all modeled on the automation pool. Add review-pool
     slot-count config.
   - **Effort:** large
   - **Dependencies:** task 3

5. **Per-pool always-Opus model override for the review pool** — *Depth 1*
   - **Scope:** Make model selection consult the owning pool's override
     before the effort-driven default, and set the review pool's override
     to Opus unconditionally. Reuses the automation pool's override
     mechanism.
   - **Effort:** small
   - **Dependencies:** task 4

6. **Reviewer worker prompt + structured `ReviewResult` output** — *Depth 1*
   - **Scope:** Author the review-only system prompt (read-only mandate,
     high-bar actionable rubric incl. the first-class regression/deletion
     check, speed/comprehensiveness balance, docs-only light rubric) and
     define + enforce the `ReviewResult` structured output schema. Wire the
     reviewer worker kind to the tool denylist from task 1.
   - **Effort:** medium
   - **Dependencies:** task 1 (denylist); can be drafted in parallel with
     task 4/5 but integration needs them

   *Tasks 5 and 6 may run in parallel once 4 lands (6 also needs 1).*

7. **Trigger: enqueue `pr_review` from the pr_url capture hook** — *Depth 2*
   - **Scope:** In the pr_url capture path (`engine/src/pr.rs` /
     `hooks.rs`), on PR create-or-update for an agent-authored task,
     enqueue a `pr_review` execution with the PR/base/head/cycle context,
     routed to the review pool. Hold the producing task out of human Review
     until the review pass resolves.
   - **Effort:** medium
   - **Dependencies:** tasks 4, 6

8. **Feedback → revision wiring (internal, never GitHub)** — *Depth 3*
   - **Scope:** On a `ReviewResult` that clears the severity gate, create a
     revision on the producing task with the rendered findings as
     `revision_instructions`, `pr_url` inherited from parent, and
     `source = pr_review`. Ensure nothing is posted to GitHub. Dispatch the
     revision on the general pool.
   - **Effort:** medium
   - **Dependencies:** tasks 6, 7, 2

9. **Loop termination & bounds** — *Depth 3*
   - **Scope:** Track `review_cycle` and `last_reviewed_sha` per PR;
     enforce `max_review_cycles` (default 3); apply the severity gate; on
     bound-reached, proceed to human Review with unresolved findings
     attached internally. Config knobs in `config.rs`.
   - **Effort:** medium
   - **Dependencies:** tasks 7, 8

10. **No-op / trivial-diff skip gate** — *Depth 3*
    - **Scope:** Before enqueuing a review, skip pure rebases, empty
      effective diffs vs. `last_reviewed_sha`, and (conservatively) trivial
      diffs. Log skip reasons. Never skip the first review of a PR by the
      trivial rule.
    - **Effort:** medium
    - **Dependencies:** task 7 (and reads `last_reviewed_sha` from task 9)

    *Tasks 9 and 10 may run in parallel after 8, with a small ordering on
    `last_reviewed_sha` ownership (define it in 9, consume in 10).*

11. **Reviewers page on the macOS agents view** — *Depth 2*
    - **Scope:** Add a Reviewers page/section mirroring the automation-pool
      page: list review-pool workers, current PR-under-review, cycle, and
      recent verdicts/skips. Reuse the automation-pool view wiring.
    - **Effort:** medium
    - **Dependencies:** task 4 (needs the pool to exist); can run in
      parallel with tasks 7–10.

12. **Docs-only light-review rubric branch** — *Depth 3*
    - **Scope:** Detect a docs-only diff and switch the reviewer to the
      light rubric (structure/completeness/required-sections), skipping the
      code rubric.
    - **Effort:** small
    - **Dependencies:** task 6

13. **End-to-end test + tuning pass** — *Depth 4*
    - **Scope:** Integration test the full produce→review→revise→re-review
      loop incl. termination, no-op skip, and the read-only denylist.
      Calibrate severity gate, `max_review_cycles`, and no-op thresholds
      against a sample of real PRs (incl. a T793-style forward-port that
      drops a feature, to prove the regression check fires).
    - **Effort:** medium
    - **Dependencies:** tasks 8, 9, 10

### Deferred / future (not a v1 blocker)

- **Configurable review-rubric policy layer** — move the baked-in rubric to
  a configurable policy (mirroring the `editorial.rs` → editorial-controls
  evolution). *future / not a v1 blocker.*
- **Per-cycle severity-gate decay** — auto-raise the gate on later cycles
  so late cycles only block on critical issues. Config stub now, default
  off. *future / not a v1 blocker.*
- **Hard per-day review-budget circuit breaker** — global cost cap beyond
  pool-size + cycle caps. *future / not a v1 blocker.*
- **Stacked-PR per-entry review semantics** — review each PR in a stack
  against its stack parent. *future / not a v1 blocker.*
- **Surfacing unresolved-at-bound findings to the human reviewer UI** —
  richer presentation than the internal attachment. *future / not a v1
  blocker.*
- **Non-blocking later cycles** — make only the first review blocking and
  later cycles best-effort to protect throughput (open question 1). *future
  / not a v1 blocker.*
