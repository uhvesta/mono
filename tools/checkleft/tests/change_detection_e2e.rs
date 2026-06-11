//! Functional / end-to-end tests for checkleft change detection.
//!
//! These drive a **real** git repository (and a jj-colocated variant) through
//! every scenario in the design matrix
//! (`tools/checkleft/docs/designs/robust-change-detection-in-checkleft.md`,
//! goal 6) and assert **both** the resolved base sha and the scoped file set.
//!
//! Unlike the per-module unit tests (which stub the git/ref probers), this suite
//! exercises the *public* entry point exactly as `main.rs` does:
//!
//! ```text
//! CiEnvironment + Vcs + ChangeOverrides
//!   -> resolve_change_plan(..)            (classify -> default branch -> ensure history -> select base)
//!   -> ChangePlan { base_sha }
//!   -> vcs.changeset_since(base_sha)      (the scoped file set, via the existing diff plumbing)
//! ```
//!
//! The bug history the design must prevent recurring is encoded here as named
//! regression fixtures — if any of these reappears the corresponding test fails:
//!
//! * **Merge-queue fork-point bug (T774 / #910)** — base must be `HEAD^1`, NOT
//!   `merge-base(HEAD^1, HEAD^2)`; an unrelated main-only file (`github_oauth.rs`)
//!   must NOT be swept into the diff.
//! * **Regular-PR 2-dot bug (T843 / #948)** — base must be
//!   `merge-base(origin/main, HEAD)`; a file changed only on main after the fork
//!   must NOT be flagged.
//! * **Files-changed-only-on-main false positives (build 1053 / #945)** — same,
//!   from the other angle.
//! * **"Always scope to changes" churn (b12d4ede)** — the default is
//!   changed-files-only; `--all` is reachable only via the explicit flag.

use std::path::Path;
use std::process::Command;

use tempfile::{TempDir, tempdir};

use checkleft::change_detection::base::EmptyReason;
use checkleft::change_detection::environment::CiEnvironment;
use checkleft::change_detection::scenario::Scenario;
use checkleft::change_detection::{ChangeOverrides, ChangePlan, base_revision_from_plan, resolve_change_plan};
use checkleft::input::ChangeSet;
use checkleft::vcs::{BaseRevision, Vcs};

// ── Vendored real-shaped GitHub event payloads (design open question Q1) ───────
//
// These are checked-in fixtures of the GitHub Actions event payloads we parse
// (`merge_group.base_sha`, `repository.default_branch`, `pull_request.base.ref`).
// They guard the payload-parsing path against GitHub schema drift independently
// of any live repository.
const MERGE_GROUP_EVENT_JSON: &str = include_str!("fixtures/github_merge_group_event.json");
const PULL_REQUEST_EVENT_JSON: &str = include_str!("fixtures/github_pull_request_event.json");

// ── git repo-builder helpers ───────────────────────────────────────────────────

/// Run a git command in `root`, asserting it succeeds.
fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run a git command in `root` and return its trimmed stdout, asserting success.
fn git_out(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8").trim().to_owned()
}

/// Create a fresh temp repo with the initial branch pinned to `default_branch`.
///
/// The branch name is pinned with `init -b` so the test does not depend on the
/// machine's `init.defaultBranch` (CI leaves it unset → defaults to `master`).
fn init_repo(default_branch: &str) -> TempDir {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-b", default_branch]);
    git(root, &["config", "user.email", "test@checkleft.example"]);
    git(root, &["config", "user.name", "Checkleft Test"]);
    // Make merges deterministic and editor-free.
    git(root, &["config", "commit.gpgsign", "false"]);
    dir
}

/// Write `name`, stage it, commit it, and return the resulting HEAD sha.
fn commit(root: &Path, name: &str, content: &str, msg: &str) -> String {
    std::fs::write(root.join(name), content).expect("write file");
    git(root, &["add", name]);
    git(root, &["commit", "-m", msg]);
    git_out(root, &["rev-parse", "HEAD"])
}

/// Resolve a git revision to a full sha.
fn rev(root: &Path, revision: &str) -> String {
    git_out(root, &["rev-parse", revision])
}

fn detect(root: &Path) -> Vcs {
    Vcs::detect(root).expect("detect vcs")
}

/// Overrides for the normal auto-classification path (no `--all`, no `--base-ref`).
fn auto() -> ChangeOverrides {
    ChangeOverrides {
        all: false,
        base_ref: None,
        default_branch: None,
    }
}

// ── Plan inspection helpers ────────────────────────────────────────────────────

/// The resolved base sha of a `Scoped` plan, or `None` for `All` / `Empty`.
fn base_sha(plan: &ChangePlan) -> Option<&str> {
    match plan {
        ChangePlan::Scoped { base_sha, .. } => Some(base_sha.as_str()),
        _ => None,
    }
}

/// Translate a `ChangePlan` into the scoped file set exactly as `main.rs`'s
/// `changeset_from_plan` does, returning the changed paths sorted.
fn scoped_paths(vcs: &Vcs, plan: &ChangePlan) -> Vec<String> {
    let changeset: ChangeSet = match plan {
        ChangePlan::All => vcs.all_files_changeset().expect("all files changeset"),
        ChangePlan::Scoped { base_sha, .. } => vcs.changeset_since(base_sha).expect("changeset since base"),
        ChangePlan::Empty { .. } => ChangeSet::default(),
    };
    let mut paths: Vec<String> = changeset
        .changed_files
        .iter()
        .map(|f| f.path.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    paths
}

fn resolve(env: &CiEnvironment, vcs: &Vcs, overrides: ChangeOverrides) -> ChangePlan {
    resolve_change_plan(env, vcs, &overrides).expect("resolve_change_plan")
}

// ── Environment builders (simulate each CI context) ────────────────────────────

fn env_gha_pull_request(base_ref: &str) -> CiEnvironment {
    CiEnvironment {
        github_actions: true,
        github_event_name: Some("pull_request".to_owned()),
        github_base_ref: Some(base_ref.to_owned()),
        ci: true,
        ..Default::default()
    }
}

fn env_bk_pull_request(number: &str, base_branch: &str, branch: &str) -> CiEnvironment {
    CiEnvironment {
        buildkite: true,
        buildkite_pull_request: Some(number.to_owned()),
        buildkite_pull_request_base_branch: Some(base_branch.to_owned()),
        buildkite_branch: Some(branch.to_owned()),
        ci: true,
        ..Default::default()
    }
}

fn env_gha_merge_group(event_path: Option<&Path>) -> CiEnvironment {
    CiEnvironment {
        github_actions: true,
        github_event_name: Some("merge_group".to_owned()),
        github_event_path: event_path.map(Path::to_owned),
        ci: true,
        ..Default::default()
    }
}

fn env_bk_merge_queue(queue_branch: &str) -> CiEnvironment {
    CiEnvironment {
        buildkite: true,
        buildkite_pull_request: Some("false".to_owned()),
        buildkite_branch: Some(queue_branch.to_owned()),
        ci: true,
        ..Default::default()
    }
}

fn env_gha_push(git_ref: &str) -> CiEnvironment {
    CiEnvironment {
        github_actions: true,
        github_event_name: Some("push".to_owned()),
        github_ref: Some(git_ref.to_owned()),
        ci: true,
        ..Default::default()
    }
}

// ── Shared topology builders ───────────────────────────────────────────────────

/// Build the canonical "PR with base-branch drift" topology and check out the
/// PR branch:
///
/// ```text
///   A (base.txt)  ──────────────  D (only_on_main.rs)   [main]
///    \
///     P (feature.rs)                                    [pr-branch, HEAD]
/// ```
///
/// Returns `(tempdir, fork_sha A)`. `merge-base(main, HEAD) == A`, so a correct
/// 3-dot scope is `{feature.rs}` and `only_on_main.rs` must be excluded
/// (T843/#948 + #945).
fn pr_with_base_drift() -> (TempDir, String) {
    let dir = init_repo("main");
    let root = dir.path().to_owned();
    let fork = commit(&root, "base.txt", "base\n", "A: base");
    git(&root, &["checkout", "-b", "pr-branch"]);
    commit(&root, "feature.rs", "fn feature() {}\n", "P: feature");
    git(&root, &["checkout", "main"]);
    commit(&root, "only_on_main.rs", "fn drift() {}\n", "D: main-only drift");
    git(&root, &["checkout", "pr-branch"]);
    (dir, fork)
}

/// Build the canonical merge-queue topology and leave HEAD at the merge commit:
///
/// ```text
///   A (base.txt) ── B (github_oauth.rs)            [main tip = queue base = HEAD^1]
///    \                \
///     P (feature.rs) ── M (merge)                  [HEAD]
/// ```
///
/// `HEAD^1 == B` (queue base, already contains the unrelated `github_oauth.rs`).
/// `HEAD^2 == P` (PR head). `merge-base(HEAD^1, HEAD^2) == A` (the fork point —
/// the T774/#910 bug). The correct base is `HEAD^1 == B`, so the scope is
/// `{feature.rs}` and `github_oauth.rs` must NOT appear.
///
/// Returns `(tempdir, fork_sha A, queue_base_sha B)`.
fn merge_queue_topology() -> (TempDir, String, String) {
    let dir = init_repo("main");
    let root = dir.path().to_owned();
    let fork = commit(&root, "base.txt", "base\n", "A: base");
    git(&root, &["checkout", "-b", "pr-branch"]);
    commit(&root, "feature.rs", "fn feature() {}\n", "P: feature");
    git(&root, &["checkout", "main"]);
    let queue_base = commit(
        &root,
        "github_oauth.rs",
        "fn oauth() {}\n",
        "B: unrelated main-only change",
    );
    // Build the GitHub-style merge commit: parent1 = main tip (B), parent2 = P.
    git(&root, &["merge", "--no-ff", "pr-branch", "-m", "M: merge pr into main"]);
    (dir, fork, queue_base)
}

// ══ Row 1 — Regular PR ═════════════════════════════════════════════════════════

/// Row 1 (GitHub PR). Regression for T843/#948 and #945: the base is the
/// merge-base (3-dot), NOT `origin/main` (2-dot), so base-branch drift is not
/// attributed to the PR.
#[test]
fn regular_pr_github_scopes_to_pr_only_excluding_base_drift() {
    let (dir, fork) = pr_with_base_drift();
    let vcs = detect(dir.path());
    let env = env_gha_pull_request("main");

    let plan = resolve(&env, &vcs, auto());

    assert!(
        matches!(
            plan,
            ChangePlan::Scoped {
                scenario: Scenario::PullRequest { .. },
                ..
            }
        ),
        "expected Scoped PullRequest, got {plan:?}"
    );
    assert_eq!(
        base_sha(&plan),
        Some(fork.as_str()),
        "PR base must be merge-base(main, HEAD) (the fork point), not origin/main's tip"
    );
    assert_eq!(
        scoped_paths(&vcs, &plan),
        vec!["feature.rs"],
        "only the PR's own file may be scoped; base-branch drift (only_on_main.rs) must be excluded"
    );

    // The base-tree reads must use the SAME sha as the diff (consistency invariant).
    assert_eq!(base_revision_from_plan(&vcs, &plan), Some(BaseRevision::Git(fork)),);
}

/// Row 1 (Buildkite PR). Same regression via the Buildkite signal path.
#[test]
fn regular_pr_buildkite_scopes_to_pr_only_excluding_base_drift() {
    let (dir, fork) = pr_with_base_drift();
    let vcs = detect(dir.path());
    let env = env_bk_pull_request("948", "main", "robust-change-detection");

    let plan = resolve(&env, &vcs, auto());

    assert_eq!(base_sha(&plan), Some(fork.as_str()));
    assert_eq!(scoped_paths(&vcs, &plan), vec!["feature.rs"]);
}

// ══ Row 2 — GitHub merge queue ═════════════════════════════════════════════════

/// Row 2 (GitHub merge queue, with event payload). Regression for T774/#910:
/// base is `merge_group.base_sha == HEAD^1`, NOT the fork point. The unrelated
/// main-only file present at the queue base must not be swept in.
#[test]
fn github_merge_queue_with_payload_uses_base_sha_not_fork_point() {
    let (dir, fork, queue_base) = merge_queue_topology();
    let root = dir.path();
    let vcs = detect(root);

    // A real merge_group event payload pointing base_sha at the queue base (B).
    let event = format!(
        r#"{{"merge_group":{{"base_sha":"{queue_base}","head_sha":"{head}","base_ref":"refs/heads/main"}},"repository":{{"default_branch":"main"}}}}"#,
        head = rev(root, "HEAD"),
    );
    let event_path = root.join("ci_event.json");
    std::fs::write(&event_path, event).expect("write event json");

    let env = env_gha_merge_group(Some(&event_path));
    let plan = resolve(&env, &vcs, auto());

    assert!(
        matches!(
            plan,
            ChangePlan::Scoped {
                scenario: Scenario::MergeQueue,
                ..
            }
        ),
        "expected Scoped MergeQueue, got {plan:?}"
    );
    assert_eq!(
        base_sha(&plan),
        Some(queue_base.as_str()),
        "merge-queue base must be HEAD^1 (queue base), not the fork point"
    );
    assert_ne!(
        base_sha(&plan),
        Some(fork.as_str()),
        "T774/#910 regression: merge-queue base must NOT be the fork point"
    );
    assert_eq!(
        scoped_paths(&vcs, &plan),
        vec!["feature.rs"],
        "github_oauth.rs (present at the queue base) must NOT be swept into the merge-queue diff"
    );
}

/// Row 2 (GitHub merge queue, no payload). Falls back to `HEAD^1`, which on a
/// well-formed merge commit equals the queue base — same answer, same exclusion.
#[test]
fn github_merge_queue_without_payload_falls_back_to_head_parent() {
    let (dir, _fork, queue_base) = merge_queue_topology();
    let vcs = detect(dir.path());
    let env = env_gha_merge_group(None); // no GITHUB_EVENT_PATH

    let plan = resolve(&env, &vcs, auto());

    assert_eq!(base_sha(&plan), Some(queue_base.as_str()), "fallback base == HEAD^1");
    assert_eq!(scoped_paths(&vcs, &plan), vec!["feature.rs"]);
}

// ══ Row 3 — Buildkite merge queue ══════════════════════════════════════════════

/// Row 3 (Buildkite gh-readonly-queue). HEAD is a merge commit → base is
/// `HEAD^1`, NOT the fork point (T774/#910).
#[test]
fn buildkite_merge_queue_uses_head_parent_not_fork_point() {
    let (dir, fork, queue_base) = merge_queue_topology();
    let vcs = detect(dir.path());
    let env = env_bk_merge_queue("gh-readonly-queue/main/pr-910-deadbeef");

    let plan = resolve(&env, &vcs, auto());

    assert!(
        matches!(
            plan,
            ChangePlan::Scoped {
                scenario: Scenario::MergeQueue,
                ..
            }
        ),
        "expected Scoped MergeQueue, got {plan:?}"
    );
    assert_eq!(base_sha(&plan), Some(queue_base.as_str()));
    assert_ne!(base_sha(&plan), Some(fork.as_str()));
    assert_eq!(scoped_paths(&vcs, &plan), vec!["feature.rs"]);
}

// ══ Row 4 — Push to default branch ═════════════════════════════════════════════

/// Row 4. Push to main scopes to this push only: base is `HEAD^1`.
#[test]
fn push_to_default_scopes_to_last_commit() {
    let dir = init_repo("main");
    let root = dir.path();
    let first = commit(root, "base.txt", "base\n", "A: base");
    commit(root, "more.rs", "fn more() {}\n", "B: another commit on main");
    let vcs = detect(root);
    let env = env_gha_push("refs/heads/main");

    let plan = resolve(&env, &vcs, auto());

    assert!(
        matches!(
            plan,
            ChangePlan::Scoped {
                scenario: Scenario::PushToDefault,
                ..
            }
        ),
        "expected Scoped PushToDefault, got {plan:?}"
    );
    assert_eq!(base_sha(&plan), Some(first.as_str()), "push-to-default base == HEAD^1");
    assert_eq!(scoped_paths(&vcs, &plan), vec!["more.rs"]);
}

// ══ Row 5 — Push to non-default branch ═════════════════════════════════════════

/// Row 5. A push to a non-default branch is scoped like a pre-merge branch:
/// 3-dot against the default branch, so base-branch drift is excluded.
#[test]
fn push_to_non_default_branch_scopes_against_default() {
    let (dir, fork) = pr_with_base_drift();
    let root = dir.path();
    // pr_with_base_drift leaves us on `pr-branch`; rename the HEAD ref to a
    // feature branch and simulate a push event for it.
    git(root, &["branch", "-m", "feature/experiment"]);
    let vcs = detect(root);
    let env = env_gha_push("refs/heads/feature/experiment");

    let plan = resolve(&env, &vcs, auto());

    assert!(
        matches!(
            plan,
            ChangePlan::Scoped {
                scenario: Scenario::PushToBranch { .. },
                ..
            }
        ),
        "expected Scoped PushToBranch, got {plan:?}"
    );
    assert_eq!(base_sha(&plan), Some(fork.as_str()), "base == merge-base(main, HEAD)");
    assert_eq!(scoped_paths(&vcs, &plan), vec!["feature.rs"]);
}

// ══ Row 6 — Local / pre-push ═══════════════════════════════════════════════════

/// Row 6. On a developer machine (no CI env) the base is `merge-base(default,
/// HEAD)` and base-branch drift is excluded.
///
/// Note: the implemented `ChangePlan` folds the working-tree variant into
/// `Scoped`, so for **git** the scoped set is the committed `base..HEAD` diff.
/// (Including uncommitted/staged changes for git is design open question Q4 /
/// deferred item F4; the jj path below does include the working copy via `@`.)
#[test]
fn local_prepush_scopes_against_merge_base() {
    let (dir, fork) = pr_with_base_drift();
    let vcs = detect(dir.path());
    let env = CiEnvironment::default(); // no CI signal -> Local

    let plan = resolve(&env, &vcs, auto());

    assert!(
        matches!(
            plan,
            ChangePlan::Scoped {
                scenario: Scenario::Local,
                ..
            }
        ),
        "expected Scoped Local, got {plan:?}"
    );
    assert_eq!(base_sha(&plan), Some(fork.as_str()));
    assert_eq!(scoped_paths(&vcs, &plan), vec!["feature.rs"]);
}

// ══ Row 7 — First commit / no merge-base ═══════════════════════════════════════

/// Row 7. When `HEAD` shares no history with the base branch (unrelated
/// histories / first commit) there is no merge-base → `Empty { NoMergeBase }`,
/// so nothing is checked (exit 0), never a silent mis-scope.
#[test]
fn first_commit_no_merge_base_yields_empty() {
    let dir = init_repo("main");
    let root = dir.path();
    commit(root, "base.txt", "base\n", "A: base on main");
    // An orphan branch with its own root commit shares no ancestry with main.
    git(root, &["checkout", "--orphan", "feature"]);
    git(root, &["rm", "-rf", "--cached", "."]);
    std::fs::remove_file(root.join("base.txt")).ok();
    commit(root, "feature.rs", "fn feature() {}\n", "X: orphan root");
    let vcs = detect(root);
    let env = env_gha_pull_request("main");

    let plan = resolve(&env, &vcs, auto());

    assert_eq!(
        plan,
        ChangePlan::Empty {
            reason: EmptyReason::NoMergeBase
        },
        "unrelated histories must yield Empty {{ NoMergeBase }}, got {plan:?}"
    );
    assert!(scoped_paths(&vcs, &plan).is_empty(), "nothing should be scoped");
}

// ══ Row 8 — Detached HEAD ══════════════════════════════════════════════════════

/// Row 8a. Detached HEAD with no merge-base against the default branch falls
/// back to `HEAD^1` (best-effort scope of the last commit), never a hard error.
#[test]
fn detached_head_with_parent_falls_back_to_head_parent() {
    let dir = init_repo("main");
    let root = dir.path();
    commit(root, "base.txt", "base\n", "A: base on main");
    // Orphan history (unrelated to main) with two commits.
    git(root, &["checkout", "--orphan", "orphan"]);
    git(root, &["rm", "-rf", "--cached", "."]);
    std::fs::remove_file(root.join("base.txt")).ok();
    let parent = commit(root, "x1.txt", "x1\n", "X: orphan root");
    commit(root, "x2.txt", "x2\n", "Y: orphan child");
    // Detach HEAD at the child commit.
    let child = rev(root, "HEAD");
    git(root, &["checkout", &child]);

    let vcs = detect(root);
    let env = CiEnvironment::default(); // Local; detached emerges in base selection

    let plan = resolve(&env, &vcs, auto());

    assert_eq!(
        base_sha(&plan),
        Some(parent.as_str()),
        "detached HEAD with no merge-base must fall back to HEAD^1"
    );
    assert_eq!(scoped_paths(&vcs, &plan), vec!["x2.txt"]);
}

/// Row 8b. Detached HEAD at a root commit (no merge-base AND no parent) yields
/// `Empty { DetachedHeadNoParent }` rather than erroring.
#[test]
fn detached_head_root_commit_yields_empty() {
    let dir = init_repo("main");
    let root = dir.path();
    commit(root, "base.txt", "base\n", "A: base on main");
    git(root, &["checkout", "--orphan", "orphan"]);
    git(root, &["rm", "-rf", "--cached", "."]);
    std::fs::remove_file(root.join("base.txt")).ok();
    commit(root, "z.txt", "z\n", "Z: orphan root");
    let root_commit = rev(root, "HEAD");
    git(root, &["checkout", &root_commit]); // detached at a parentless commit

    let vcs = detect(root);
    let env = CiEnvironment::default();

    let plan = resolve(&env, &vcs, auto());

    assert_eq!(
        plan,
        ChangePlan::Empty {
            reason: EmptyReason::DetachedHeadNoParent
        },
        "detached root commit must yield Empty {{ DetachedHeadNoParent }}, got {plan:?}"
    );
}

// ══ Row 9 — Shallow checkout ═══════════════════════════════════════════════════

/// Build a "remote" with `main` (several commits including a main-only drift
/// file) plus a `pr-branch` forked from the root, then return a depth-1 shallow
/// clone of the PR branch with `origin/main` fetched at depth 1.
///
/// `git fetch --depth=1` (rather than `git clone --depth=1`) is used because git
/// ignores `--depth` for local-path transports, which would yield a full clone.
fn shallow_pr_clone() -> (TempDir, TempDir, String) {
    let remote_dir = init_repo("main");
    let remote = remote_dir.path().to_owned();
    let fork = commit(&remote, "base.txt", "base\n", "A: base (fork point)");
    // Several more commits on main, including a main-only drift file.
    commit(&remote, "c1.txt", "c1\n", "main commit 1");
    commit(&remote, "only_on_main.rs", "fn drift() {}\n", "main-only drift");
    commit(&remote, "c3.txt", "c3\n", "main commit 3");
    // PR branch forked from the root (fork point), adds the PR file.
    git(&remote, &["checkout", "-b", "pr-branch", &fork]);
    commit(&remote, "feature.rs", "fn feature() {}\n", "P: feature");

    let clone_dir = tempdir().expect("tempdir clone");
    let clone = clone_dir.path().to_owned();
    git(&clone, &["init", "-b", "pr-branch"]);
    git(&clone, &["config", "user.email", "test@checkleft.example"]);
    git(&clone, &["config", "user.name", "Checkleft Test"]);
    git(&clone, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&clone, &["fetch", "--depth=1", "origin", "pr-branch"]);
    git(&clone, &["checkout", "-b", "pr-branch", "FETCH_HEAD"]);
    git(&clone, &["fetch", "--depth=1", "origin", "main"]);

    (remote_dir, clone_dir, fork)
}

/// Row 9 (shallow, reachable). A shallow PR clone deepens history on its own
/// until the merge-base is reachable, then resolves the correct base and scope.
#[test]
fn shallow_pr_deepens_until_base_reachable() {
    let (remote_dir, clone_dir, fork) = shallow_pr_clone();
    let clone = clone_dir.path();
    assert_eq!(
        git_out(clone, &["rev-parse", "--is-shallow-repository"]),
        "true",
        "clone must start shallow"
    );
    let vcs = detect(clone);
    // GITHUB_BASE_REF in real GHA is just the branch name (e.g. "main"),
    // not the remote-prefixed form. The code adds "origin/" in resolve_change_plan.
    let env = env_gha_pull_request("main");

    let plan = resolve(&env, &vcs, auto());

    assert_eq!(
        base_sha(&plan),
        Some(fork.as_str()),
        "after auto-deepen, the base must be the fork point (merge-base origin/main HEAD)"
    );
    assert_eq!(
        scoped_paths(&vcs, &plan),
        vec!["feature.rs"],
        "main-only drift must be excluded even from a shallow clone"
    );
    drop(remote_dir);
}

/// Row 9 (shallow, unreachable). When the requested base ref is not on the
/// remote at all, resolution fails with a precise, actionable error rather than
/// silently mis-scoping against the tip.
#[test]
fn shallow_base_permanently_unreachable_errors() {
    let (remote_dir, clone_dir, _fork) = shallow_pr_clone();
    let clone = clone_dir.path();
    let vcs = detect(clone);
    // GITHUB_BASE_REF is the bare branch name; the code adds "origin/" prefix.
    let env = env_gha_pull_request("totally-nonexistent-branch");

    let err =
        resolve_change_plan(&env, &vcs, &auto()).expect_err("unreachable base must error, not silently mis-scope");
    let msg = err.to_string();
    assert!(
        msg.contains("origin/totally-nonexistent-branch"),
        "error must name the unreachable ref: {msg}"
    );
    assert!(msg.contains("git fetch origin"), "error must include the remedy: {msg}");
    drop(remote_dir);
}

// ══ "Always scope to changes" churn (b12d4ede) ═════════════════════════════════

/// b12d4ede regression. The default path scopes to changes only — a pre-existing
/// tracked file the PR did not touch must NOT appear.
#[test]
fn default_scopes_to_changes_not_all_tracked_files() {
    let (dir, _fork) = pr_with_base_drift();
    let vcs = detect(dir.path());
    let env = env_gha_pull_request("main");

    let plan = resolve(&env, &vcs, auto());

    let scoped = scoped_paths(&vcs, &plan);
    assert_eq!(scoped, vec!["feature.rs"]);
    assert!(
        !scoped.contains(&"base.txt".to_owned()),
        "pre-existing base.txt must not be scoped by default (no implicit --all)"
    );
}

/// b12d4ede regression. `--all` is reachable only via the explicit flag and then
/// returns every tracked file.
#[test]
fn all_flag_returns_every_tracked_file() {
    let (dir, _fork) = pr_with_base_drift();
    let vcs = detect(dir.path());
    let env = env_gha_pull_request("main");

    let plan = resolve(
        &env,
        &vcs,
        ChangeOverrides {
            all: true,
            base_ref: None,
            default_branch: None,
        },
    );

    assert_eq!(plan, ChangePlan::All);
    let scoped = scoped_paths(&vcs, &plan);
    assert!(
        scoped.contains(&"base.txt".to_owned()) && scoped.contains(&"feature.rs".to_owned()),
        "--all must return every tracked file, got {scoped:?}"
    );
}

// ══ Operator escape hatch — explicit --base-ref ════════════════════════════════

/// The `--base-ref` override bypasses classification and scopes via
/// `merge-base(ref, HEAD)`, preserving the back-compat semantics callers relied
/// on before auto-classification.
#[test]
fn base_ref_override_still_scopes_via_merge_base() {
    let (dir, fork) = pr_with_base_drift();
    let vcs = detect(dir.path());
    // No CI env at all; the explicit ref must win regardless.
    let env = CiEnvironment::default();

    let plan = resolve(
        &env,
        &vcs,
        ChangeOverrides {
            all: false,
            base_ref: Some("main".to_owned()),
            default_branch: None,
        },
    );

    assert!(
        matches!(
            plan,
            ChangePlan::Scoped {
                scenario: Scenario::Local,
                ..
            }
        ),
        "base-ref override uses the Local scenario marker, got {plan:?}"
    );
    assert_eq!(base_sha(&plan), Some(fork.as_str()));
    assert_eq!(scoped_paths(&vcs, &plan), vec!["feature.rs"]);
}

// ══ Row 10 — Push-to-branch shallow clone: fetch base and scope correctly ═════════

/// Build the standard "single-branch Buildkite shallow clone" fixture used by
/// the Row 10 tests.
///
/// Topology:
/// ```text
///   C1 (fork) ── C2 (main tip, "only_on_main.rs")
///             \
///              B1 (boss branch tip, "boss_change.rs") ← HEAD
/// ```
///
/// The clone is a depth-1 single-branch checkout of `boss/exec_test`.
/// `origin/main` is NOT in the local refs initially, and the configured fetch
/// refspec covers only the boss branch — exactly matching a Buildkite
/// `git clone --depth=1 --single-branch --branch boss/exec_test` checkout.
///
/// Returns `(remote_dir, clone_dir, fork_sha)`.
fn shallow_push_clone() -> (TempDir, TempDir, String) {
    let remote_dir = init_repo("main");
    let remote = remote_dir.path().to_owned();

    let fork = commit(&remote, "base.txt", "base\n", "C1: base");
    commit(&remote, "only_on_main.rs", "fn main_only() {}\n", "C2: main-only");
    git(&remote, &["checkout", "-b", "boss/exec_test", &fork]);
    commit(&remote, "boss_change.rs", "fn boss() {}\n", "B1: boss change");

    let clone_dir = tempdir().expect("tempdir clone");
    let clone = clone_dir.path().to_owned();
    git(&clone, &["init", "-b", "boss/exec_test"]);
    git(&clone, &["config", "user.email", "test@checkleft.example"]);
    git(&clone, &["config", "user.name", "Checkleft Test"]);
    git(&clone, &["remote", "add", "origin", remote.to_str().unwrap()]);
    // Override the fetch refspec to cover ONLY the boss branch (not all of
    // refs/heads/*). This prevents a bare `git fetch origin` from pulling in main,
    // exactly matching a Buildkite single-branch shallow clone.
    git(
        &clone,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/boss/exec_test:refs/remotes/origin/boss/exec_test",
        ],
    );
    git(&clone, &["fetch", "--depth=1", "origin", "boss/exec_test"]);
    git(&clone, &["checkout", "-b", "boss/exec_test", "FETCH_HEAD"]);

    assert_eq!(
        git_out(&clone, &["rev-parse", "--is-shallow-repository"]),
        "true",
        "clone must be shallow for this test to be meaningful"
    );

    (remote_dir, clone_dir, fork)
}

fn bk_push_env(branch: &str) -> CiEnvironment {
    CiEnvironment {
        buildkite: true,
        buildkite_pull_request: Some("false".to_owned()),
        buildkite_branch: Some(branch.to_owned()),
        buildkite_pipeline_default_branch: Some("main".to_owned()),
        ci: true,
        ..Default::default()
    }
}

/// Row 10 (reachable). Regression: a Buildkite push build with a single-branch
/// shallow clone must fetch `origin/main` explicitly and compute a real merge-base,
/// scoping the changeset to only the files actually changed in this push.
///
/// Before the fix (PR #1182), this would return `ChangePlan::Empty`, silently
/// disabling all checkleft violations in CI. The correct behaviour is to fetch
/// the base, diff against the fork point, and return only the files changed on
/// the branch — not an empty changeset and not a diff-from-scratch.
#[test]
fn push_to_branch_shallow_fetches_base_and_scopes_to_changed_files() {
    let (remote_dir, clone_dir, fork) = shallow_push_clone();
    let clone = clone_dir.path();
    let vcs = detect(clone);

    let plan = resolve_change_plan(&bk_push_env("boss/exec_test"), &vcs, &auto())
        .expect("push-to-branch in CI must resolve a real base, not error");

    // Must be Scoped — not Empty (which would silently disable all checks).
    assert!(
        matches!(plan, ChangePlan::Scoped { .. }),
        "push-to-branch must resolve a real changeset, got {plan:?}"
    );
    assert_eq!(
        base_sha(&plan),
        Some(fork.as_str()),
        "base must be the fork point (merge-base of origin/main and HEAD)"
    );

    let paths = scoped_paths(&vcs, &plan);
    assert_eq!(
        paths,
        vec!["boss_change.rs"],
        "only the file actually changed on the branch must be in the changeset, got {paths:?}"
    );
    assert!(
        !paths.contains(&"only_on_main.rs".to_owned()),
        "a file changed only on main must NOT appear in the changeset (no diff-from-scratch)"
    );
    assert!(
        !paths.contains(&"base.txt".to_owned()),
        "a file present since the fork point must NOT appear (not changed by this push)"
    );

    drop(remote_dir);
}

/// Row 10 (unreachable). When the default branch genuinely does not exist on the
/// remote (wrong config, orphaned branch), checkleft must fail loudly with an
/// actionable error — never silently produce an empty changeset.
///
/// Hard requirement from the task spec: a missing base must be a red build, not
/// a green pass.
#[test]
fn push_to_branch_shallow_nonexistent_default_branch_errors_loudly() {
    let (remote_dir, clone_dir, _fork) = shallow_push_clone();
    let clone = clone_dir.path();
    let vcs = detect(clone);

    // Claim the default branch is "totally-nonexistent" — not on the remote.
    let env = CiEnvironment {
        buildkite: true,
        buildkite_pull_request: Some("false".to_owned()),
        buildkite_branch: Some("boss/exec_test".to_owned()),
        buildkite_pipeline_default_branch: Some("totally-nonexistent".to_owned()),
        ci: true,
        ..Default::default()
    };

    let err = resolve_change_plan(&env, &vcs, &auto())
        .expect_err("unreachable default branch must produce a hard error, not an empty changeset");
    let msg = err.to_string();
    assert!(
        msg.contains("origin/totally-nonexistent"),
        "error must name the unreachable ref: {msg}"
    );
    assert!(msg.contains("git fetch origin"), "error must include the remedy: {msg}");

    drop(remote_dir);
}

// ══ jj-colocated variant ═══════════════════════════════════════════════════════

fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// jj-colocated repo. Base selection runs through the colocated git repo (per
/// design: CI topology *is* git), while the diff uses jj. The PR drift exclusion
/// (T843/#948) must hold for jj too.
///
/// Skipped (test passes) when `jj` is not on PATH so the suite stays green in
/// environments without jj installed.
#[test]
fn jj_colocated_pr_scopes_to_pr_only_excluding_drift() {
    if !jj_available() {
        eprintln!("skipping jj-colocated e2e: `jj` is not on PATH");
        return;
    }

    let (dir, fork) = pr_with_base_drift();
    let root = dir.path();
    // Colocate jj over the existing git repo (git HEAD stays at the pr-branch
    // tip; jj `@` becomes an empty working commit on top of it).
    let colocate = Command::new("jj")
        .args(["git", "init", "--colocate"])
        .current_dir(root)
        .output()
        .expect("spawn jj");
    if !colocate.status.success() {
        eprintln!(
            "skipping jj-colocated e2e: `jj git init --colocate` failed: {}",
            String::from_utf8_lossy(&colocate.stderr)
        );
        return;
    }

    let vcs = detect(root);
    assert_eq!(vcs.kind(), checkleft::vcs::VcsKind::Jujutsu, "must detect as jj");

    let env = CiEnvironment::default(); // Local developer scenario
    let plan = resolve(&env, &vcs, auto());

    assert_eq!(
        base_sha(&plan),
        Some(fork.as_str()),
        "jj base resolution must use the colocated git merge-base (the fork point)"
    );
    // The base-tree reads use a jj revision carrying the same git sha.
    assert_eq!(base_revision_from_plan(&vcs, &plan), Some(BaseRevision::Jujutsu(fork)),);

    let scoped = scoped_paths(&vcs, &plan);
    assert!(
        scoped.contains(&"feature.rs".to_owned()),
        "the PR file must be scoped, got {scoped:?}"
    );
    assert!(
        !scoped.contains(&"only_on_main.rs".to_owned()),
        "base-branch drift must be excluded for jj too (T843/#948), got {scoped:?}"
    );
}

// ══ Vendored event-payload parsing (design open question Q1) ═══════════════════

/// Parse a checked-in real-shaped merge_group payload via the production
/// `CiEnvironment::read_github_event_payload` path and assert the fields the
/// base selector depends on. Guards GitHub schema drift independently of a repo.
#[test]
fn vendored_github_merge_group_event_payload_parses() {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join("merge_group_event.json");
    std::fs::write(&path, MERGE_GROUP_EVENT_JSON).expect("write fixture");
    let env = CiEnvironment {
        github_event_path: Some(path),
        ..Default::default()
    };

    let payload = env.read_github_event_payload().expect("merge_group fixture must parse");
    let mg = payload.merge_group.expect("merge_group present");
    assert_eq!(mg.base_sha.as_deref(), Some("1234567890abcdef1234567890abcdef12345678"));
    assert_eq!(mg.base_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(
        payload.repository.and_then(|r| r.default_branch).as_deref(),
        Some("main"),
    );
}

/// Parse a checked-in real-shaped pull_request payload and assert the base ref /
/// default branch fields are extracted.
#[test]
fn vendored_github_pull_request_event_payload_parses() {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join("pull_request_event.json");
    std::fs::write(&path, PULL_REQUEST_EVENT_JSON).expect("write fixture");
    let env = CiEnvironment {
        github_event_path: Some(path),
        ..Default::default()
    };

    let payload = env
        .read_github_event_payload()
        .expect("pull_request fixture must parse");
    let base = payload
        .pull_request
        .and_then(|pr| pr.base)
        .expect("pull_request.base present");
    assert_eq!(base.branch_ref.as_deref(), Some("main"));
    assert_eq!(
        payload.repository.and_then(|r| r.default_branch).as_deref(),
        Some("main"),
    );
}
