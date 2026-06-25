# Checkleft: surface findings in the GitHub UI

## Overview

`checkleft run` reports findings as human-readable text or JSON on stdout, and gates the process exit code on whether any finding is an error. In CI those findings are only visible to someone who opens the build log. This design specifies how checkleft can surface the **same findings directly in the GitHub UI** — as inline annotations on the PR diff, entries in the Checks tab, or alerts in the Security / code-scanning tab — so a reviewer sees a check failure on the exact line that caused it.

Four candidate mechanisms exist, and the operator requirement is that **all four are explored thoroughly and each is implementable and adoptable independently**, gated behind its own flag/output mode:

1. **GHA workflow commands** — `::error file=…,line=…,col=…::message` printed to stdout.
2. **Problem matchers** — a checked-in regex matcher (`::add-matcher::`) that parses checkleft's text output into annotations.
3. **GitHub Check Runs API** — a provider-agnostic REST call that creates a check run and POSTs annotations.
4. **SARIF + code scanning** — checkleft emits SARIF JSON; the consuming repo uploads it to the code-scanning service.

The mono/flunge ecosystem uses both **GitHub Actions and Buildkite** as co-primary CI platforms. Checkleft is being designed for use in a large enterprise that uses GHA, so GHA-native ergonomics and zero-credential paths matter as much as Buildkite support — the GHA-native options (#1 workflow commands, #2 problem matchers, and #4 SARIF via `upload-sarif`) are first-class, not nice-to-haves. Each option below states plainly how it works on each CI, and the recommendation delivers a first-class supported path for each ecosystem. All options are evaluated against the fact that **mono is a private repository** (`git@github.com:spinyfin/mono.git`).

This document is a design only. No feature code is included; the final section is a dependency-ordered, PR-sized implementation breakdown of independent, flag-gated tasks.

## Goals

- Define a **single internal annotation representation** that maps a checkleft `Finding` (+ its check id) to a GitHub annotation, and specify how each backend renders it.
- Specify **four independently shippable backends**, each behind its own flag/output mode, so they ship and are adopted separately.
- For **each** backend produce: the mechanism + the exact GitHub UI surface it lights up; a GHA-usefulness AND Buildkite-usefulness verdict; checkleft-side implementation requirements (output mode/subcommand, flag, credential handling, batching/limits, severity mapping); consuming-repo usage (workflow YAML, checked-in files, secrets/permissions); and limitations / failure modes.
- Treat **both GHA and Buildkite as first-class** co-primary targets; validate-or-refute the operator's hypothesis that the Check Runs API (#3) is the best option for Buildkite.
- Make **annotation caps loud, never silent**: whenever a backend drops findings to fit a GitHub-imposed limit, it must log that it did.
- Keep the **default off** — existing `checkleft run` stdout, JSON, and exit-code behavior are unchanged unless an annotation mode is explicitly requested.
- Recommend a shared-core-vs-independent architecture and a prioritization for the mono/flunge environment.

## Non-goals

- **Changing what `run` reports or its exit semantics.** Annotation backends are a _side output_; the human/JSON renderers and the "exit 1 iff any error" rule (`main.rs:501-508`) are untouched.
- **Implementing the backends.** This is the design; the task breakdown is the handoff.
- **A new finding model.** The existing `Finding`/`Location`/`Severity` types are the source of truth; no new severities, no per-finding rule taxonomy beyond the existing `check_id`.
- **End-to-end ranges (multi-line / multi-column highlights).** `Finding` carries a single `(line, column)` point today; emitting `endLine`/`endColumn` spans is deferred (listed in the task breakdown as `future / not a v1 blocker`).
- **Provisioning org-level infrastructure.** Creating a GitHub App, enabling GitHub Advanced Security, or minting org secrets are operator/ops actions; this design specifies _what_ is needed and _how checkleft consumes it_, not the provisioning runbook.
- **Deduplication / alert-lifecycle semantics** (dismiss, fingerprint, "fixed in" tracking) beyond what a backend gets for free. SARIF gets dedupe from code scanning; replicating it for the Check Runs backend is out of scope.

## Current-state audit

Source: `tools/checkleft/src/output.rs`, `main.rs`, `change_detection/environment.rs`, `vcs.rs`, `config.rs`, `external/declarative/transform.rs`, the `checks/**/*.yaml` definitions, and `wit/check.wit`.

### Finding & severity model (`output.rs`)

```rust
// output.rs:6-9
pub struct CheckResult { pub check_id: String, pub findings: Vec<Finding> }

// output.rs:12-19
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    pub location: Option<Location>,
    #[serde(default)] pub remediations: Vec<String>,
    pub suggested_fix: Option<SuggestedFix>,
}

// output.rs:41-45
pub struct Location { pub path: PathBuf, pub line: Option<u32>, pub column: Option<u32> }

// output.rs:21-27
#[serde(rename_all = "snake_case")]
pub enum Severity { Error, Warning, Info }
```

- `Location.path` is **repo-relative** (per the WIT contract, `wit/check.wit:124-130`) — exactly what every GitHub backend wants. `line`/`column` are both optional; **file-level findings (no line) are legal** and every backend must handle them.
- `check_id` (e.g. `lint/rust`, `format/bazel`, `file/forbidden-path`) is the natural **rule id** for SARIF and the natural **title** for an annotation.
- **Exit code** (`main.rs:501-508`): exit 1 iff any finding is `Severity::Error`; `Warning`/`Info` never affect it. Annotation backends must not change this.

### Output formats & CLI surface (`main.rs`)

```rust
// main.rs:178-182
enum OutputFormat { Human, Json }
// main.rs:47-48 (on RunArgs, and likewise FixArgs)
#[arg(long, default_value = "human")] format: OutputFormat,
```

- Subcommands today (`main.rs:132-168`): `Run`, `Fix`, `List`, `ShowPlan` (temporary), `Install`, `Uninstall`. A bare `checkleft` is `run`. The format is dispatched in a `match` at `main.rs:486-499` — the natural extension point for a new side output.
- **Human text format** (`main.rs:1764-1816`) — two lines per finding, load-bearing for the problem-matcher regex contract:

  ```text
  error[typo]: Found typo.
    --> a.rs:3:5
     = to resolve: Fix it.
  ```

  i.e. `"{severity}[{check_id}]: {message}"` then `"  --> {path}[:{line}[:{column}]]"`. Color is ANSI-painted for interactive terminals but **off in CI** (pipes / `NO_COLOR` / non-human format), per the `--show-progress` auto-detect doc.

- **JSON output** (`main.rs:1611-1613`) is `serde_json::to_string_pretty(&Vec<CheckResult>)` — every field above round-trips. This is the key reuse lever: a SARIF/Check-Run backend can serialize from the same in-memory `Vec<CheckResult>` (or deserialize the JSON) with no new projection logic.

### CI context already available (`change_detection/environment.rs`, `vcs.rs`, `main.rs`)

This is the foundation the Check Runs and SARIF-upload backends build on — **most of the plumbing already exists**:

- `CiEnvironment::from_env()` (`environment.rs:38-61`) is the single place checkleft reads CI vars. It already captures, for **Buildkite**: `BUILDKITE`, `BUILDKITE_COMMIT`, `BUILDKITE_BRANCH`, `BUILDKITE_PULL_REQUEST`, `BUILDKITE_PULL_REQUEST_BASE_BRANCH`, `BUILDKITE_PIPELINE_DEFAULT_BRANCH`; and for **GHA**: `GITHUB_ACTIONS`, `GITHUB_EVENT_NAME`, `GITHUB_SHA`, `GITHUB_REF`, `GITHUB_HEAD_REF`, `GITHUB_BASE_REF`, `GITHUB_EVENT_PATH` (+ a lazily-parsed event payload exposing `pull_request.base.sha`, `merge_group.{base,head}_sha`, `repository.default_branch`).
- **Gaps for the API backends** (new work, called out in tasks T3): it does **not** capture `GITHUB_REPOSITORY` (owner/repo), nor `pull_request.head.sha` (the payload struct captures only `pull_request.base`), nor a Buildkite repo slug (`BUILDKITE_REPO`).
- **Repo slug** already resolvable without env: `vcs.remote_repo_slug()` (`vcs.rs:203-`) parses `owner/repo` from the `origin` remote (`git@`, `https://`, `ssh://` forms), and `CHECKS_REPOSITORY` (`main.rs:187,1402`) is an explicit override.
- **GitHub token resolution already exists and is already used for authenticated GitHub REST calls.** `detect_github_token()` (`main.rs:1534-1585`) resolves, in priority order: `CHECKS_GITHUB_TOKEN` → `GH_TOKEN` → `GITHUB_TOKEN` → `gh auth token`. It is used today to fetch PR descriptions for the bypass-directive mechanism — so checkleft _already makes authenticated `api.github.com` calls_ and _already_ has the credential ergonomics the Check Runs / SARIF-upload backends need.
- **HTTP client**: `reqwest` is already a dependency (`Cargo.toml:50`) and is used async in `config.rs` (external-checks fetch). The run path is already async, so a backend that POSTs to GitHub adds no new runtime or dependency.

### Declarative finding projection (`external/declarative/transform.rs`, `checks/lint/rust.yaml`)

Findings from external/declarative checks are projected from raw tool output by a `FindingTemplate { path, line, column, message, severity, remediations }` (`transform.rs:153-187`) via jq-style selectors — e.g. `checks/lint/rust.yaml` projects clippy's JSON diagnostics into the same `Finding` shape. **The annotation backends sit downstream of this**: by the time the runner has a `Vec<CheckResult>`, the source of a finding (built-in, declarative, or WASM) is irrelevant. No backend needs to know about projection.

## The shared annotation core

All four backends ultimately render the same data. Define one internal type and one mapping, reused by backends **1, 3, and 4** (backend 2 is structurally different — see its section):

```rust
// proposed: src/annotate/mod.rs
pub enum AnnotationLevel { Failure, Warning, Notice } // GitHub's 3-valued vocabulary

pub struct Annotation {
    pub path: String,          // repo-relative, forward slashes
    pub start_line: u32,       // 1-based; defaults to 1 for file-level findings
    pub end_line: u32,         // == start_line today (no ranges yet)
    pub start_column: Option<u32>, // only meaningful when start_line == end_line
    pub end_column: Option<u32>,
    pub level: AnnotationLevel,
    pub title: String,         // the check_id
    pub message: String,
    pub rule_id: String,       // the check_id (SARIF ruleId / dedupe key)
}

pub fn annotation_from_finding(check_id: &str, f: &Finding) -> Option<Annotation>;
```

Mapping rules (single source of truth for severity and path/line handling):

- **Severity → level**: `Error → Failure`, `Warning → Warning`, `Info → Notice`. (SARIF spells these `error`/`warning`/`note`; the Check Runs API spells them `failure`/`warning`/`notice`; backend renderers translate from `AnnotationLevel`.)
- **Path**: repo-relative already; normalize separators to `/`.
- **Line/column**: GitHub annotations require a line. A file-level finding (`line == None`) maps to `start_line = end_line = 1` with no column — the annotation lands at the top of the file. (Logged at debug; this is GitHub's own limitation, not a checkleft choice.)
- **Column**: passed through only when a line is present and `start_line == end_line`.

The core also owns two cross-cutting helpers used by multiple backends:

- `escape_workflow_data` / `escape_workflow_property` — the GHA `%`/`%0A`/`%0D`/`%3A`/`%2C` escaping (backend 1).
- `cap_with_log(items, limit, surface)` — truncates to a GitHub-imposed limit and **logs a warning naming how many findings were dropped and where** (backends 1, 3, 4). This is the single chokepoint that makes "caps are never silent" a structural property rather than a per-backend promise.

**Why a small shared core and not four independent implementations:** the Finding→Annotation mapping, the severity vocabulary, and the cap-logging discipline are identical across backends 1/3/4; duplicating them invites the three GitHub surfaces to disagree about, say, how a file-level info finding renders. But the core is deliberately _small_ (one type, one mapping fn, two helpers) so it does not become a coupling point — each backend's renderer, flag, and credential handling stay fully independent and independently shippable. The core is a **foundation task (T0)** that backends 1/3/4 depend on; backend 2 depends only on the text-format contract, so it can be built in parallel with everything.

---

## Option 1 — GHA workflow commands

### Mechanism & UI surface

During the run, checkleft prints magic lines to stdout that the GitHub Actions runner interprets:

```text
::error file=src/main.rs,line=42,col=10,title=lint/rust::clippy::needless_return: unneeded `return`
::warning file=README.md,line=3::md/links: broken link
::notice file=BUILD.bazel::format/bazel: would reformat
```

- **Severity → command**: `Error → ::error`, `Warning → ::warning`, `Info → ::notice`.
- **Properties**: `file` (repo-relative), `line`, `col`, optional `endLine`/`endColumn`, `title` (the `check_id`). The message is everything after `::`.
- **Escaping** (toolkit rules): in the **message**, `%`→`%25`, `\r`→`%0D`, `\n`→`%0A`. In **property values**, additionally `:`→`%3A` and `,`→`%2C`. Multiline messages are encoded with `%0A` and render as a multi-line annotation body.
- **UI surface lit up**: **inline annotations on the PR "Files changed" diff** (on the line) **and** in the job's annotations summary, attributed to the running workflow's check. No Security tab, no separate check run.

### GHA verdict — **first-class GHA path: zero credentials, idiomatic, trivial**

This is the canonical, frictionless GHA path — idiomatic on GHA and ZERO-credential (the runner parses stdout; no token needed at all, nothing checked in). In an enterprise GHA setting, the absence of any credential requirement is a real advantage: no GitHub App to provision, no PAT to manage, no secret to rotate. This is a first-class GHA option.

### Buildkite verdict — **does not work**

Buildkite has no concept of `::error::` lines. They print as literal noise in the Buildkite log. The backend must therefore **only emit these lines when `CiEnvironment.github_actions` is true** (or the mode is explicitly forced), so a Buildkite run isn't polluted.

### checkleft-side implementation

- **Flag/mode**: `checkleft run --annotations=gha` (value-enum, see cross-cutting). When set, after the normal render, iterate `Vec<CheckResult>` → `Annotation` (shared core) → print one workflow command per annotation to **stdout** (interleaved with, and in addition to, normal output — they are comments to a human reading the log).
- **Severity mapping**: via the shared core's `AnnotationLevel`.
- **Caps / truncation**: GitHub renders a **bounded number of annotations per step** (historically ~10 of each level per step, with an overall per-run display cap on the order of 50). Beyond that, extra `::error::` lines still print but never become annotations — **silently** from GitHub's side. The backend must therefore sort by severity, apply `cap_with_log` at a configurable ceiling (default the documented per-step limit), and **log** `"checkleft: emitted N of M findings as GHA annotations; M-N exceeded GitHub's per-step annotation cap and appear only in the log"`. The exact cap is GitHub-controlled and may drift, so it is a constant the backend can override, not a hard-coded magic number scattered through the code.
- **Interaction with stdout & exit code**: additive only. The lines go to stdout; the exit code is unchanged; a consumer parsing checkleft's _own_ `--format=json` is unaffected because JSON mode and GHA mode are independent toggles (and you would not normally combine human/json stdout with annotation lines on the same stream — see cross-cutting on `--annotations-out`).

### Consuming-repo usage

Nothing checked in. The workflow just runs checkleft with the flag:

```yaml
# .github/workflows/checks.yml (GHA only)
- name: checkleft
  run: checkleft run --annotations=gha
```

No secrets, no permissions beyond the default token.

### Limitations & failure modes

- **GHA-only** — must self-disable off-GHA.
- **Per-step annotation cap** silently drops the overflow (mitigated by `cap_with_log`).
- Annotations attach to the **workflow's** check, not a checkleft-named check — you can't tell at the Checks-tab level that "checkleft" specifically failed (only #3 gives that).
- `title` is the only structured per-annotation label; there is no rule catalog / help-text surface (that's #4).

---

## Option 2 — Problem matchers

### Mechanism & UI surface

A **problem matcher** is a checked-in JSON file describing regexes that the GHA runner applies to stdout lines; matches become annotations. checkleft registers it at runtime and deregisters it after:

```text
::add-matcher::.github/checkleft.matcher.json
checkleft run            # normal text output; the runner scans each line
::remove-matcher owner=checkleft::
```

The matcher parses checkleft's **existing human text output** — so the annotations come "for free" from text the tool already prints, with no `--annotations` mode at all.

### The matcher JSON & the regex contract

checkleft's text output is two lines per finding, so the matcher is a **multi-line pattern** (consecutive `pattern` entries match consecutive lines; only the last may `loop`):

```json
{
  "problemMatcher": [
    {
      "owner": "checkleft",
      "pattern": [
        {
          "regexp": "^(error|warning|info)\\[([^\\]]+)\\]:\\s+(.*)$",
          "severity": 1,
          "code": 2,
          "message": 3
        },
        { "regexp": "^\\s+-->\\s+(.+?):(\\d+)(?::(\\d+))?$", "file": 1, "line": 2, "column": 3 }
      ]
    }
  ]
}
```

- First line → `severity` (group 1), `code` = `check_id` (group 2), `message` (group 3). Second line → `file`, `line`, optional `column`.
- **owner** = `checkleft` (must match the `remove-matcher` call; version it as `checkleft-v1` so an output-format change can ship a new matcher without colliding with a cached old one).
- **Severity mapping caveat**: GHA problem matchers historically recognize `error` and `warning` only — there is **no `notice`/`info`** problem-matcher severity. checkleft's `info` findings must map to `warning` (or be omitted) in the matcher. This is a real expressiveness loss versus #1.

### checkleft-side implementation

- **A generator subcommand**, because the matcher must be **checked into the consuming repo** and must stay in lockstep with the text format: `checkleft gen-problem-matcher [--owner checkleft] > .github/checkleft.matcher.json`. It emits the JSON above, with the regexes built from the **same format constants** the human renderer uses (so they cannot drift independently).
- **Drift defense (the central risk)**: the matcher regex is coupled to `render_finding` (`main.rs:1764-1816`). If that layout changes (color, prefix, the `-->` arrow, column separator), the regex silently stops matching → **zero annotations, no error**. Mitigations, all required: (a) generate the regex from shared constants, not a hand-written string; (b) a **golden test** that runs `checkleft run` over a fixture, feeds the real text output through the generated regex, and asserts the captures; (c) a CI check that fails if a checked-in matcher is out of date versus `gen-problem-matcher` (same pattern as other "generated file is stale" checks).
- **Color must be off**: the matcher runs on raw log bytes; ANSI escapes would break the anchors. CI output is already non-colored, but the docs must state `--show-progress=false` / `NO_COLOR` for safety.
- No credentials, no severity-posting logic, no caps logic of its own — but it inherits the **same per-step annotation cap** as #1 (matched lines become the same annotation objects).

### GHA verdict — **works, but strictly weaker than #1**

It produces the same inline PR-diff annotations as #1, with: no `info`/notice level, fragile coupling to text layout, a checked-in file to maintain, and the add/remove lifecycle to wire. Its only advantage over #1 is that it needs **no `--annotations` mode** — it scrapes output checkleft already prints. That advantage is marginal once #1 exists.

### Buildkite verdict — **does not work**

`::add-matcher::` is a GHA runner feature. Buildkite ignores it (prints as text). No path.

### Consuming-repo usage

```yaml
# .github/workflows/checks.yml (GHA only)
- run: echo "::add-matcher::.github/checkleft.matcher.json"
- run: checkleft run --show-progress=false
  continue-on-error: true # so remove-matcher still runs
- run: echo "::remove-matcher owner=checkleft::"
  if: always()
```

Checked-in file: `.github/checkleft.matcher.json` (generated by `checkleft gen-problem-matcher`, kept fresh by the staleness check). No secrets.

### Limitations & failure modes

- **GHA-only**; **no info/notice**; **silent breakage on text-format drift** (the headline hazard, mitigated by the golden + staleness tests); same per-step cap as #1; checked-in file to maintain.

---

## Option 3 — GitHub Check Runs API

### Mechanism & UI surface

checkleft itself calls the REST API to create a **check run** against the head commit and attach annotations:

```text
POST /repos/{owner}/{repo}/check-runs
  { "name": "checkleft", "head_sha": "<sha>", "status": "completed",
    "conclusion": "failure",
    "output": { "title": "12 findings", "summary": "...", "annotations": [ ...≤50... ] } }
PATCH /repos/{owner}/{repo}/check-runs/{id}   # append further batches of ≤50 annotations
```

- Each annotation: `{ path, start_line, end_line, start_column?, end_column?, annotation_level, message, title, raw_details? }` where `annotation_level ∈ {notice, warning, failure}`.
- **UI surface lit up**: a dedicated **"checkleft" check in the Checks tab** (own name, own conclusion, own summary) **plus inline annotations on the PR diff** **plus** the annotations list on the check page. This is the only option that produces a _checkleft-named_ check independent of the CI provider's own check.

### Auth / credential model — the crux

Creating a check run is **not** something an arbitrary token can do. The reliable, documented path is a **GitHub App installation token** with the `checks: write` permission. The GHA default `GITHUB_TOKEN` already _is_ such a token (it is the GitHub Actions app's installation token, with `checks: write` when `permissions: checks: write` is granted) — which is exactly why workflow commands and check runs work there. Concretely:

- **GHA**: pass the job's `GITHUB_TOKEN` (with `permissions: checks: write`) to checkleft via `CHECKS_GITHUB_TOKEN`/`GITHUB_TOKEN`. checkleft's existing `detect_github_token()` already finds it. No new app needed.
- **Buildkite**: there is no ambient installation token. You must **stand up a GitHub App** (org-owned, `checks: write`, installed on the repo), store its **app id + private key** as Buildkite secrets, and have checkleft mint an installation token at runtime: sign a short-lived JWT with the private key → `POST /app/installations/{id}/access_tokens` → use the returned token. This is the **heaviest credential lift of the four**, and the real cost behind the operator's "best for Buildkite" hypothesis.
- **PATs**: a **classic PAT cannot create check runs** (returns 403 "Resource not accessible by personal access token"). A **fine-grained PAT with explicit `Checks: write`** is a **supported credential path** and the recommended lighter-weight onboarding story for Buildkite (and other non-GHA CI) environments — it passes through the existing `CHECKS_GITHUB_TOKEN` env var with no new infrastructure. The **GitHub App installation token** (app id + private key) is the **fallback** for environments where PAT provisioning is not an option or where the fine-grained PAT proves insufficient in practice.
- **How the secret reaches checkleft**: token via the existing `CHECKS_GITHUB_TOKEN` env var (already wired). For the GitHub-App-minting path, new inputs are needed — `CHECKS_GITHUB_APP_ID`, `CHECKS_GITHUB_APP_PRIVATE_KEY` (or a file path), and optionally `CHECKS_GITHUB_INSTALLATION_ID` (else discover it via `GET /repos/{owner}/{repo}/installation`). These are **secrets** and must be flagged as such in any consuming pipeline.

### Resolving repo + head SHA from CI

- **owner/repo**: `GITHUB_REPOSITORY` (GHA, new capture in T3) → else `CHECKS_REPOSITORY` → else `vcs.remote_repo_slug()`.
- **head SHA**: on **Buildkite**, `BUILDKITE_COMMIT` is the PR head — exactly right. On **GHA `pull_request`**, `GITHUB_SHA` is the _merge_ commit, not the PR head; annotating the merge commit still shows on the PR, but to annotate the PR head commit you want `pull_request.head.sha` from the event payload (new capture in T3). On push/merge-group, `GITHUB_SHA` / `merge_group.head_sha` are correct. The resolver picks per `github_event_name`.

### Batching, limits, rate limits

- **50 annotations per request.** Create with the first ≤50; `PATCH` the same check run with each subsequent ≤50 (GitHub appends). For N findings that is `ceil(N/50)` requests — e.g. 230 findings → 1 POST + 4 PATCHes.
- **Annotation-level mapping**: `Failure/Warning/Notice` → `failure/warning/notice`.
- **conclusion**: any error finding → `failure`; otherwise `success` (or `neutral` if you prefer warnings not to render as a green check — a config choice, default `success`). **summary**: a markdown count table; **title**: `"{errors} errors, {warnings} warnings"`.
- **Rate limits**: 5000 requests/hour for the token; batching keeps a run to a handful of requests. Not a practical constraint.
- **Caps are not silently lossy here** the way #1 is: there is no hard annotation ceiling, only the per-request batch size, so all findings can be posted across batches. (If an operator caps batches for cost, that cap goes through `cap_with_log`.)

### GHA verdict — **works (credentials = the ambient `GITHUB_TOKEN`)**

Strictly more than #1/#2: a checkleft-named check with its own named check entry, a rich per-check summary, and inline PR-diff annotations — with no checked-in file. Costs one `permissions: checks: write` line and passing the ambient token. For enterprise GHA deployments that want a distinct, independently-named checkleft check (visible in the Checks tab separately from the workflow), this is meaningfully better than #1. For lightweight GHA usage where only inline annotations matter, #1 (workflow commands) remains the zero-credential, zero-configuration first choice.

### Buildkite verdict — **works, and is the only option that lights up inline PR annotations on Buildkite without GHAS** (credentials required)

This **confirms the operator's hypothesis**. It is provider-agnostic as an HTTP call, and supports two credential tiers: the **recommended lighter-weight path** is a **fine-grained PAT with `Checks: write`** passed via `CHECKS_GITHUB_TOKEN` — no new app to provision, no private key to manage. The **fallback path** (for environments where PAT provisioning is not an option) is a **GitHub App installation token** (app id + private key as Buildkite secrets). It is the best Buildkite option; the PAT path is the recommended starting point.

### checkleft-side implementation

- **Flag/mode**: `checkleft run --annotations=check-run`.
- **Reuse**: `detect_github_token()`, `remote_repo_slug()`, `CiEnvironment`, the existing `reqwest` client + async runtime.
- **New**: T3 CI-context fields (owner/repo, PR head SHA, Buildkite repo); the App-token minting path (JWT + installation token) as a credential sub-task; POST/PATCH with ≤50 batching; level/conclusion/summary mapping; **non-fatal failure handling** — if no token/SHA/repo, or the API errors, **log a warning and continue with the normal exit code** (the check _content_ already failed via exit 1; failing to _post_ annotations must not turn a clean run red, nor mask a dirty one). An opt-in `--annotations-strict` could make posting failures fatal, default off.

### Consuming-repo usage

GHA:

```yaml
permissions: { contents: read, checks: write }
steps:
  - run: checkleft run --annotations=check-run
    env: { CHECKS_GITHUB_TOKEN: ${{ github.token }} }
```

Buildkite — recommended path (fine-grained PAT with `Checks: write`):

```yaml
steps:
  - command: checkleft run --annotations=check-run
    env:
      CHECKS_GITHUB_TOKEN: "$CHECKLEFT_GH_PAT" # fine-grained PAT, Checks:write, from Buildkite secrets
```

Buildkite — fallback path (GitHub App, for environments where PAT provisioning is not an option):

```yaml
steps:
  - command: checkleft run --annotations=check-run
    env:
      CHECKS_GITHUB_APP_ID: "123456"
      CHECKS_GITHUB_APP_PRIVATE_KEY: "$GH_APP_PEM" # from Buildkite secrets
```

Checked-in files: none. Secrets: GHA uses the ambient token; Buildkite uses either a fine-grained PAT (lighter) or GitHub App id + private key (heavier, fallback).

### Limitations & failure modes

- **Credential complexity on Buildkite** (App + private key) is the real barrier.
- Posting failures must be **non-fatal + logged**, never silent and never run-failing.
- `head_sha` must be a commit GitHub has (it will, in CI).
- Check runs do **not** dedupe across runs — each push makes a new check run (that's expected; dedupe/alert-lifecycle is #4's domain).

---

## Option 4 — SARIF + code scanning

### Mechanism & UI surface

checkleft emits **SARIF 2.1.0** JSON; the consuming repo uploads it to GitHub's code-scanning service.

```json
{
  "version": "2.1.0",
  "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
  "runs": [
    {
      "tool": {
        "driver": {
          "name": "checkleft",
          "rules": [
            { "id": "lint/rust", "name": "lintRust", "shortDescription": { "text": "lint/rust" } }
          ]
        }
      },
      "results": [
        {
          "ruleId": "lint/rust",
          "level": "error",
          "message": { "text": "clippy::needless_return: ..." },
          "locations": [
            {
              "physicalLocation": {
                "artifactLocation": { "uri": "src/main.rs" },
                "region": { "startLine": 42, "startColumn": 10 }
              }
            }
          ]
        }
      ]
    }
  ]
}
```

- **rules catalog**: one `reportingDescriptor` per distinct `check_id`; each `result.ruleId` references it.
- **level mapping**: `Error → error`, `Warning → warning`, `Info → note` (SARIF's vocabulary is `none|note|warning|error`).
- **UI surface lit up**: the **Security → Code scanning alerts tab** (persistent alerts with dedupe, dismissal, "fixed" tracking via fingerprints) **plus inline PR-diff annotations** **plus** a code-scanning check. The richest surface of the four — and the only one with a durable alert lifecycle.

### Availability constraint — the crux for mono

Code scanning is **free on public repos** but on **private/internal repos requires GitHub Advanced Security (GHAS)** to be licensed and enabled. **mono is private** (`spinyfin/mono`). Therefore: **#4 does not light up anything on mono unless GHAS is enabled**, regardless of CI provider. The SARIF _serializer_ is still worth building (it's cheap and works for the public `checkleft-sandbox` and any future public repo), but the _upload-surfacing_ on mono is gated on a licensing decision outside checkleft's control.

**Enterprise context:** A large GitHub enterprise commonly has GHAS licensed at the org or enterprise tier — if this deployment is in such an environment, GHAS may already be available on mono. That would make #4 immediately viable as the richest annotation surface (Security tab + inline PR annotations + durable fingerprint-based alert lifecycle). This materially raises #4's value and priority: in an enterprise GHA setting it is the native Code-Scanning path via `github/codeql-action/upload-sarif`, zero-credential beyond the ambient `security-events: write` permission. The GHAS licensing question (flagged in Risks) should be treated as high-priority precisely because the answer changes #4 from "deferred" to "recommended first."

### GHA verdict — **native Code-Scanning path on GHA; free on public repos, private repos need GHAS** (credentials = ambient token + `security-events: write`)

This is the idiomatic GHA code-scanning integration via `github/codeql-action/upload-sarif` — ZERO additional credentials beyond the ambient token with `security-events: write`. In an enterprise GHA environment where GHAS is licensed, this is the recommended primary GHA path for the richest surface (Security tab, durable alert lifecycle, inline annotations). Upload via the official action:

```yaml
permissions: { security-events: write }
steps:
  - run: checkleft run --annotations=sarif --annotations-out=checkleft.sarif
  - uses: github/codeql-action/upload-sarif@v3
    with: { sarif_file: checkleft.sarif }
```

### Buildkite verdict — **works via the SARIF REST API; private repos need GHAS** (credentials required)

There is no `upload-sarif` action on Buildkite, but the REST endpoint is provider-agnostic:

```text
POST /repos/{owner}/{repo}/code-scanning/sarifs
  { "commit_sha": "<sha>", "ref": "refs/heads/<branch>",
    "sarif": "<gzip-then-base64 of the SARIF JSON>" }
# then optionally GET /repos/{owner}/{repo}/code-scanning/sarifs/{id} to poll processing status
```

- Token needs **`security_events: write`** (GitHub App or PAT). Unlike check runs, a PAT with this scope _is_ accepted for SARIF upload, so the Buildkite credential lift is lighter than #3 _if_ a suitable token exists — **but the GHAS license is still mandatory for private mono.**
- checkleft can either (a) emit the file and leave upload to a separate Buildkite step (a `curl`/script), or (b) integrate the upload (`--annotations=sarif --upload`) reusing the token plumbing. Recommend (a) emit-only as the v1 SARIF serializer, with (b) the integrated upload as a separate task.

### Batching / limits

- GitHub's code-scanning SARIF caps (GitHub-published, may drift): on the order of **5000 results per upload** and a **~10 MB gzip-compressed** upload size, with bounded rule/run counts. Results beyond the cap are **rejected by GitHub** (the whole upload can fail), so the serializer must `cap_with_log` at a safe ceiling and warn when it truncates.

### checkleft-side implementation

- **Flag/mode**: `checkleft run --annotations=sarif --annotations-out=<path>` (serializer, no creds). Optional later: `--upload` (Buildkite) reusing `detect_github_token()` + T3 context + gzip/base64.
- **Reuse**: shared core for level mapping & path; check-id set → rules catalog; `serde_json` for emission.
- **Severity mapping**: `error/warning/note`.

### Consuming-repo usage

- **GHA**: the `upload-sarif` action above; `security-events: write`; no checked-in file.
- **Buildkite**: emit + a script step that gzips+base64s and POSTs with a `security_events`-scoped token (secret).

### Limitations & failure modes

- **Private repos require GHAS** — the hard gate for mono.
- Upload **caps reject** oversized SARIF (must `cap_with_log`).
- Two-step on Buildkite (emit then upload) unless integrated.
- Async processing: an accepted upload isn't instantly visible; the check appears after GitHub processes it.

---

## Cross-cutting design

### Flag / CLI surface

- **Recommendation: a single repeatable value-enum** `--annotations=<mode>` on `run` (and `fix`), `mode ∈ { gha, check-run, sarif, none }`, **default `none`** (off; existing behavior unchanged). Repeatable so **multiple backends can be active at once** — e.g. `--annotations=gha --annotations=sarif` on a public GHA repo. Each variant is independently implemented and independently shippable; the enum simply grows one variant per task.
- **`--annotations-out=<path>`** for file-producing modes (sarif), so stdout stays clean for `--format=human|json`. The `gha` mode writes its `::…::` lines to stdout by design (they are log comments).
- **Problem matchers are not a runtime mode** — they are the separate **`checkleft gen-problem-matcher`** subcommand emitting a static, checked-in file. Keeping it off the `--annotations` enum reflects that it scrapes existing output rather than rendering annotations.
- **Why this over per-mode boolean flags** (`--gha-annotations`, `--sarif`, `--check-run`): the enum is less surface, composes cleanly when several are active, and still lets each backend ship on its own (adding a variant is additive). The trade-off — a flag value referencing a not-yet-built backend — is avoided by only listing shipped variants in the enum.

### Shared core vs fully independent

Recommended: a **small shared foundation (T0)** — the `Annotation` type, `annotation_from_finding`, severity→level, the GHA escaping helpers, and `cap_with_log` — that backends **1, 3, 4** depend on; backend **2** depends only on the text-format contract. This satisfies "independently shippable behind its own flag" (each backend is a separate task with its own flag and credential story) while preventing the three GitHub surfaces from disagreeing about the mapping. The core is intentionally tiny so it never becomes a coupling bottleneck.

### Comparison matrix

| Option                    | GHA                                     | Buildkite                            | UI surface                                      | Complexity             | Credentials                                                                            | Checked-in files         |
| ------------------------- | --------------------------------------- | ------------------------------------ | ----------------------------------------------- | ---------------------- | -------------------------------------------------------------------------------------- | ------------------------ |
| 1 · Workflow commands     | ✅ works                                | ❌ ignored                           | Inline PR diff + job annotations                | Trivial                | None                                                                                   | None                     |
| 2 · Problem matchers      | ✅ works (weaker than #1)               | ❌ ignored                           | Inline PR diff + job annotations                | Low (regex drift risk) | None                                                                                   | `.github/*.matcher.json` |
| 3 · Check Runs API        | ✅ works (ambient token)                | ✅ **works** (PAT or GitHub App)     | **Own check in Checks tab** + inline PR diff    | High                   | GHA: ambient token; Buildkite: fine-grained PAT (recommended) or GitHub App (fallback) | None                     |
| 4 · SARIF + code scanning | ✅ public free / **private needs GHAS** | ✅ via REST / **private needs GHAS** | **Security/code-scanning tab** + inline PR diff | Medium                 | `security-events`/`security_events: write` token                                       | None (GHA action)        |

### Recommendation: co-primary paths for GHA and Buildkite

The recommendation delivers a first-class supported option for each ecosystem rather than optimizing for one:

**GHA-native path (zero credentials on GHA):**

1. **#1 (GHA workflow commands) is the idiomatic, zero-credential GHA path** — just add `--annotations=gha` to the workflow step. No GitHub App, no PAT, no secret to manage. This is a first-class GHA option and the right default starting point for GHA deployments.
2. **#4 (SARIF via `upload-sarif`) is the native Code-Scanning path on GHA** and the recommended next step once GHAS is confirmed. In a large enterprise GHA environment, GHAS is often already licensed — which makes #4 immediately viable and the richest surface available (Security tab, durable alert lifecycle with fingerprint-based dedupe, inline PR annotations). Confirm GHAS availability before deprioritizing this; if GHAS is on, #4 becomes the primary GHA recommendation.

**CI-agnostic / Buildkite path (credentials required):**

3. **#3 (Check Runs API) is confirmed best for Buildkite** — the **only** option that surfaces inline PR-diff annotations _and_ a checkleft-named check on Buildkite without a GHAS license. **The operator's hypothesis is confirmed.** The recommended Buildkite onboarding path is a **fine-grained PAT with `Checks: write`** (passed as `CHECKS_GITHUB_TOKEN`), which is materially lighter to adopt than a GitHub App. The **GitHub App installation token** (app id + private key as Buildkite secrets) is the fallback for environments where PAT provisioning is not an option. On GHA, #3 also works via the ambient token (with `permissions: checks: write`) and produces a checkleft-named check — a meaningful improvement for enterprise GHA workflows that want check-level granularity separate from the workflow.
4. **Build the #4 SARIF serializer regardless (cheap, credential-free)** — independently useful for public repos and as the foundation for the REST upload path on Buildkite. The REST-based SARIF upload path (`POST /code-scanning/sarifs`) works on Buildkite and requires GHAS for private mono; it is the preferred Buildkite #4 path once GHAS is licensed.

**#2 (problem matchers) remains deferred** — same surface as #1 on GHA, with added fragility (text-format coupling) and a checked-in file to maintain. Build it only if a no-`--annotations`-mode scraper is specifically wanted; otherwise mark it `future / not a v1 blocker`.

In short: **GHA path → #1 now + #4 when GHAS confirmed; Buildkite path → #3 (GitHub App required); #4 SARIF serializer ships regardless; #2 deferred.**

## Alternatives considered

### A. One auto-detecting annotation mode (rejected)

A single `--annotations` flag (no value) that picks the backend from CI env: GHA → workflow commands, Buildkite → check run, public repo → SARIF. **Rejected** because the operator requires each backend to be **independently flag-gated and independently adoptable** — a repo must be able to turn on exactly one surface and reason about it. Auto-detection couples the backends' shipping schedules, hides which surface is live, and makes the credential requirements implicit (a Buildkite run would silently need a GitHub App the moment it's enabled). The chosen explicit enum keeps each backend a discrete, separately-shipped choice.

### B. Emit JSON only; let consumers convert externally (rejected)

checkleft already emits `--format=json`; each consuming repo could run its own converter (jq → `::error::` lines on GHA; a Python uploader for Check Runs / SARIF on Buildkite). **Rejected** because it pushes the severity mapping, escaping, batching, cap-handling, and credential management into _every consumer_, with no shared tested core — exactly the drift the operator wants avoided — and it defeats the requirement that the mechanism be implementable _in checkleft_. The one good idea here — that the JSON / in-memory `Vec<CheckResult>` is a clean reuse point — is **adopted internally**: backends serialize from the same data without re-projecting findings.

### C. Build only the Check Runs API backend (rejected)

Since #3 is the most CI-agnostic, build only it and skip the rest. **Rejected** because (a) #1 is near-free and strictly easier for the GHA repos, where #3's check-name advantage matters less; (b) #4 unlocks the durable Security-tab alert lifecycle that #3 cannot, wherever GHAS is available; and (c) the operator explicitly asked all four be explored and independently shippable. Prioritization ≠ exclusivity.

## Risks / open questions

### Decided

- **Check Runs credential model on Buildkite.** ✅ **Decided**: support both ambient token / fine-grained PAT (recommended lighter-weight onboarding path) and GitHub App installation token (fallback for environments where PAT provisioning is not an option). Fine-grained PAT with `Checks: write` is the primary Buildkite onboarding story; the GitHub App path (T4a) is the fallback. Classic PATs are confirmed _not_ to work.
- **Flag surface.** ✅ **Decided**: single repeatable `--annotations=<mode>` enum (+ `--annotations-out`) confirmed over per-mode boolean flags.
- **Posting failures non-fatal by default.** ✅ **Decided**: a failure to _post_ annotations (missing token, network error, 403) logs a warning and preserves the content-driven exit code. `--annotations-strict` is the opt-in to make posting failures fatal; default is non-fatal.
- **Problem matchers (Option 2) in v1.** ✅ **Decided**: Option 1 only; Option 2 is deferred as future / not a v1 blocker. T2 is superseded by T1 for the same GHA surface.

### Open

- **Is GitHub Advanced Security licensed for the private `spinyfin/mono` repo?** In a large enterprise GHA environment, GHAS is often licensed at the org or enterprise tier — if so, #4 becomes immediately viable and is the richest annotation surface (Security tab, durable alert lifecycle, inline PR annotations via `upload-sarif` with zero extra credentials beyond `security-events: write`). This single fact decides whether #4's upload path is a v1 priority (GHAS on) or deferred (GHAS off). Treat this as high-priority to resolve; a human must answer it.
- **PR head SHA vs merge SHA on GHA `pull_request` events.** Annotate `pull_request.head.sha` (the contributor's commit) or `GITHUB_SHA` (the synthetic merge commit)? Head SHA is more intuitive for reviewers; confirm the choice (and the new payload capture in T3).
- **Annotation caps must be loud.** #1's per-step cap and #4's upload caps silently drop/reject overflow on GitHub's side. The `cap_with_log` chokepoint must log every truncation; reviewers should confirm the default ceilings.

## Proposed implementation task breakdown

Tasks are PR-sized and **each backend is independently shippable behind its own flag**. Dependencies are by task name. Depth-0 tasks may all start immediately and in parallel.

### T0 — Shared annotation core

**Scope:** Add `src/annotate/mod.rs`: the `Annotation` type and `AnnotationLevel`, `annotation_from_finding(check_id, &Finding) -> Option<Annotation>` (severity→level, repo-relative path normalization, file-level-finding → line 1), the GHA escaping helpers, and `cap_with_log(items, limit, surface)`. Pure library code with unit tests; wires into nothing yet. **Effort:** small. **Dependencies:** none.

### T1 — GHA workflow commands backend

**Scope:** Add `gha` to the `--annotations` enum; render `::error/::warning/::notice::` lines from annotations to stdout, self-disabling unless `CiEnvironment.github_actions` (or forced); apply `cap_with_log` at the per-step ceiling. Docs snippet. **Effort:** small. **Dependencies:** T0.

### T2 — Problem-matcher generator

**Scope:** Add `checkleft gen-problem-matcher` emitting the multi-line matcher JSON built from the shared text-format constants; a golden test asserting the regex captures real `checkleft run` output; a staleness check that fails when a checked-in matcher diverges from the generator; usage docs + the add/remove workflow snippet. **Effort:** small. **Dependencies:** none (couples only to the text-format contract; can run parallel with T0/T1). **Status: deferred / not a v1 blocker** — produces the same surface as T1 on GHA with added fragility and a checked-in file to maintain; superseded by T1 for the same use case. Build only if a no-`--annotations`-mode scraper is specifically requested.

### T3 — CI-context resolution extension

**Scope:** Extend `CiEnvironment` / event-payload parsing to capture `GITHUB_REPOSITORY`, `pull_request.head.sha`, and a Buildkite repo slug (`BUILDKITE_REPO`); add resolvers `resolve_owner_repo()` (env → `CHECKS_REPOSITORY` → `remote_repo_slug()`) and `resolve_head_sha()` (per `github_event_name` / Buildkite). Pure, table-tested. Gates the two API backends. **Effort:** small. **Dependencies:** none (can run parallel with T0).

### T4 — Check Runs API backend

**Scope:** Add `check-run` to the `--annotations` enum; build the check-run payload (level/conclusion/summary mapping), POST create + PATCH append in ≤50-annotation batches via the existing `reqwest` client; resolve owner/repo + head SHA (T3); authenticate via `detect_github_token()`; non-fatal failure handling (`--annotations-strict` opt-out). **Effort:** large. **Dependencies:** T0, T3.

### T4a — GitHub App installation-token credential support _(credential/secret handling)_

**Scope:** Add the GitHub App token path as the **fallback credential** for environments where a fine-grained PAT is not an option: read `CHECKS_GITHUB_APP_ID` + `CHECKS_GITHUB_APP_PRIVATE_KEY` (+ optional `CHECKS_GITHUB_INSTALLATION_ID`, else discover via `GET /repos/{owner}/{repo}/installation`), sign a JWT, exchange for an installation token, feed it to T4. Note: the **recommended Buildkite onboarding path** is a fine-grained PAT with `Checks: write` (already handled by T4's existing `detect_github_token()` plumbing via `CHECKS_GITHUB_TOKEN`); T4a is only needed for the heavier GitHub App fallback. **FLAGGED: secret handling.** **Effort:** medium. **Dependencies:** T4.

### T5 — SARIF serializer

**Scope:** Add `sarif` to the `--annotations` enum + `--annotations-out`; serialize SARIF 2.1.0 (rules catalog from distinct check ids; `error/warning/note` levels; repo-relative `artifactLocation`/`region`); `cap_with_log` at the code-scanning result ceiling. Pure serializer, **no credentials**; independently useful for public repos. **Effort:** medium. **Dependencies:** T0.

### T5a — SARIF upload integration for Buildkite _(credential/secret handling + GHAS dependency)_

**Scope:** Add `--upload` to the SARIF mode: gzip+base64 the SARIF and `POST /repos/{owner}/{repo}/code-scanning/sarifs` with a `security_events`-scoped token (T3 context, `detect_github_token()`), optional status poll. **FLAGGED: secret handling; surfacing on private mono additionally requires GHAS.** **Effort:** medium. **Dependencies:** T5, T3.

### T6 — Consuming-repo docs & recipes

**Scope:** Author the userdoc page(s): per-backend GHA workflow YAML and Buildkite step snippets, required permissions/secrets, checked-in files, and the GitHub App setup pointer for the Check Runs Buildkite path. Incremental — one section lands per backend as it ships. **Effort:** small. **Dependencies:** at least one of T1/T2/T4/T5.

### Parallelism & graph

- **Depth 0 (start immediately, in parallel):** T0, T2, T3.
- **Depth 1 (after their deps):** T1 (after T0), T5 (after T0), T4 (after T0+T3) — all three parallel.
- **Depth 2:** T4a (after T4), T5a (after T5+T3) — parallel. T6 trails whichever backend(s) it documents.

### Deferred / `future / not a v1 blocker`

- **T2 (problem matchers)** unless a no-`--annotations`-mode scraper is specifically wanted — superseded by T1 for the same surface.
- **T5a SARIF upload on mono** until a GHAS licensing decision is made (the serializer T5 still ships and serves public repos).
- **End-to-end ranges** (`endLine`/`endColumn` spans) — needs `Finding` to carry end positions; today it's a single point.
- **Check-run dedupe/fingerprints** (alert-lifecycle parity with SARIF) — not attempted for #3.
- **Auto-detecting annotation mode** (Alternative A) — explicitly rejected in favor of explicit, independently-gated flags.
