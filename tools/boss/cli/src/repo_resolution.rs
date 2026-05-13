//! Creation-time repo resolution for `boss task create` / `boss chore create`.
//!
//! Implements the chain from §Q4 of
//! `tools/boss/docs/designs/multi-repo-work-modeling.md`:
//!   1. explicit `--repo <url>`
//!   2. prompt-text parser against the product's known-repo set
//!   3. recent-context query (last task's repo on this product)
//!   4. product default (`product.repo_remote_url`)
//!   5. ask once (interactive) or fail
//!
//! The parser is regex-free substring matching — no LLM, no registry.

use std::collections::BTreeSet;
use std::io::{self, Write};

use boss_client::BossClient;
use boss_protocol::{FrontendEvent, FrontendRequest, Product, Task};

use crate::CliError;

/// Which step in the chain supplied the URL. Returned by the pure
/// [`run_chain`] helper; `InteractiveAsk` is not represented here
/// because the async wrapper handles the prompt outside the chain
/// and only ever returns a raw URL to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionSource {
    Explicit,
    PromptParser,
    Recent,
    ProductDefault,
}

/// One leg of the chain — either a resolved URL with its provenance
/// or "fall through to the ask-or-fail step." Returned by the pure
/// `run_chain` helper so unit tests can pin the inference order
/// without touching IO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainOutcome {
    Resolved {
        url: String,
        source: ResolutionSource,
    },
    AskOrFail,
}

/// Run the deterministic part of the resolution chain (steps 1-4 of
/// Q4). Returns [`ChainOutcome::AskOrFail`] when nothing matched —
/// the caller decides whether to prompt the human or refuse.
///
/// Pure: takes plain data so unit tests can cover the precedence
/// table without spinning up an engine.
pub fn run_chain(
    explicit_flag: Option<&str>,
    prompt_text: &str,
    known_repos: &[String],
    recent_repo: Option<&str>,
    product_default: Option<&str>,
) -> ChainOutcome {
    if let Some(url) = explicit_flag.and_then(non_empty) {
        return ChainOutcome::Resolved {
            url: url.to_owned(),
            source: ResolutionSource::Explicit,
        };
    }
    if let Some(url) = match_known_repo_in_prompt(prompt_text, known_repos) {
        return ChainOutcome::Resolved {
            url,
            source: ResolutionSource::PromptParser,
        };
    }
    if let Some(url) = recent_repo.and_then(non_empty) {
        return ChainOutcome::Resolved {
            url: url.to_owned(),
            source: ResolutionSource::Recent,
        };
    }
    if let Some(url) = product_default.and_then(non_empty) {
        return ChainOutcome::Resolved {
            url: url.to_owned(),
            source: ResolutionSource::ProductDefault,
        };
    }
    ChainOutcome::AskOrFail
}

/// Resolve the repo URL to write on a newly-created chore / task.
///
/// Mirrors the design's `resolve_repo_at_create_time(conn, product_id,
/// …)` signature, but adapted for the CLI surface: it talks to the
/// engine via [`BossClient`] instead of a direct sqlite connection,
/// and takes the already-fetched [`Product`] so the caller's existing
/// product-resolution path is reused.
///
/// When the product has its own `repo_remote_url`, the new work item
/// must NOT carry a row-level override — the engine resolves from the
/// product at read time. In this case we return `Ok(None)` unless the
/// caller supplied an explicit non-empty `--repo`, in which case we
/// return an error.
///
/// Returns `Ok(None)` only when the deterministic chain whiffed *and*
/// either `interactive == false` or the user skipped the interactive
/// prompt. Callers should error out on `(false, None)` with a message
/// pointing at `--repo` / `boss product update --repo`.
pub async fn resolve_repo_at_create_time(
    client: &mut BossClient,
    product: &Product,
    explicit_flag: Option<&str>,
    prompt_text: &str,
    interactive: bool,
) -> Result<Option<String>, CliError> {
    // Single-repo product: the row must be NULL; the product's repo is
    // resolved at dispatch time. Reject an explicit --repo override.
    if let Some(product_repo) = product.repo_remote_url.as_deref() {
        if explicit_flag.and_then(non_empty).is_some() {
            return Err(CliError::usage(format!(
                "cannot set per-task repo override on product `{}`: \
                 product has its own repo (`{}`). \
                 Clear the product's repo first, or omit --repo to inherit.",
                product.slug,
                product_repo,
            )));
        }
        return Ok(None);
    }

    // Multi-repo product (no product default): run the full chain.

    // Short-circuit: explicit flag wins without touching the engine.
    if let Some(url) = explicit_flag.and_then(non_empty) {
        return Ok(Some(url.to_owned()));
    }

    let items = list_all_work_items_for_product(client, &product.id).await?;
    let known_repos = collect_known_repos(None, &items);
    let recent_repo = recent_repo_for_product(&items);

    match run_chain(
        None,
        prompt_text,
        &known_repos,
        recent_repo.as_deref(),
        None,
    ) {
        ChainOutcome::Resolved { url, .. } => Ok(Some(url)),
        ChainOutcome::AskOrFail => {
            if interactive {
                interactive_ask(&known_repos, &product.slug)
            } else {
                Ok(None)
            }
        }
    }
}

/// Build the error message the caller emits when the chain whiffs and
/// we're in `--no-input` mode. Kept here so the wording stays in one
/// place — tested by the integration test.
pub fn unresolved_repo_error(product_slug: &str) -> CliError {
    CliError::usage(format!(
        "could not resolve repo for new work item under product `{product_slug}` \
         (product has no default; prompt mentions no known repo; \
         no prior work-item repo cached). Re-run with `--repo <url>` \
         or set a product default with `boss product update {product_slug} --repo <url>`."
    ))
}

async fn list_all_work_items_for_product(
    client: &mut BossClient,
    product_id: &str,
) -> Result<Vec<Task>, CliError> {
    let tasks = send_list_tasks(client, product_id).await?;
    let chores = send_list_chores(client, product_id).await?;
    let mut all = tasks;
    all.extend(chores);
    Ok(all)
}

async fn send_list_tasks(client: &mut BossClient, product_id: &str) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListTasks {
            product_id: product_id.to_owned(),
            project_id: None,
            dep_filter: None,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::TasksList { tasks, .. } => Ok(tasks),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(CliError::internal(anyhow::anyhow!(
            "unexpected engine event for tasks list during repo resolution: {}",
            serde_json::to_string(&other).unwrap_or_else(|_| "<unserializable>".to_owned())
        ))),
    }
}

async fn send_list_chores(
    client: &mut BossClient,
    product_id: &str,
) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListChores {
            product_id: product_id.to_owned(),
            dep_filter: None,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ChoresList { chores, .. } => Ok(chores),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(CliError::internal(anyhow::anyhow!(
            "unexpected engine event for chores list during repo resolution: {}",
            serde_json::to_string(&other).unwrap_or_else(|_| "<unserializable>".to_owned())
        ))),
    }
}

/// Empirical known-repo set for a product: distinct, non-empty repo
/// URLs across every work item under the product, plus the product
/// default if any. Per design §Q4, the set bootstraps from zero —
/// brand-new products with no work items and no default produce an
/// empty `Vec`.
fn collect_known_repos(product_default: Option<&str>, items: &[Task]) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    if let Some(url) = product_default.and_then(non_empty) {
        set.insert(url.to_owned());
    }
    for item in items {
        if let Some(url) = item.repo_remote_url.as_deref().and_then(non_empty) {
            set.insert(url.to_owned());
        }
    }
    set.into_iter().collect()
}

/// Pick the most-recently-updated work item with a non-NULL repo
/// override; design §Q4's recent-context query, lifted into Rust
/// because the CLI doesn't have direct DB access.
///
/// Ties on `updated_at` are broken by the engine's existing ordering
/// (insertion order on the slice), which matches the SQL behavior
/// closely enough for the "last repo the human used" heuristic.
fn recent_repo_for_product(items: &[Task]) -> Option<String> {
    items
        .iter()
        .filter(|task| task.deleted_at.is_none())
        .filter_map(|task| {
            let url = task.repo_remote_url.as_deref().and_then(non_empty)?;
            let stamp = parse_epoch_seconds(&task.updated_at);
            Some((stamp, url.to_owned()))
        })
        .max_by_key(|(stamp, _)| *stamp)
        .map(|(_, url)| url)
}

fn parse_epoch_seconds(stamp: &str) -> i64 {
    stamp.trim().parse::<i64>().unwrap_or(0)
}

/// Case-insensitive substring search for any known repo (or its
/// derived aliases) in `prompt_text`. Left-most match wins; ties on
/// position break by longer match. Returns the *canonical URL* of
/// the match, not the alias that hit.
fn match_known_repo_in_prompt(prompt_text: &str, known_repos: &[String]) -> Option<String> {
    if prompt_text.trim().is_empty() || known_repos.is_empty() {
        return None;
    }
    let lc_prompt = prompt_text.to_ascii_lowercase();
    let mut best: Option<(usize, usize, &str)> = None;
    for url in known_repos {
        let aliases = aliases_for(url);
        for alias in &aliases {
            if alias.len() < MIN_ALIAS_LEN {
                continue;
            }
            let lc_alias = alias.to_ascii_lowercase();
            let Some(pos) = lc_prompt.find(&lc_alias) else {
                continue;
            };
            let better = match best {
                None => true,
                Some((best_pos, best_len, _)) => {
                    pos < best_pos || (pos == best_pos && lc_alias.len() > best_len)
                }
            };
            if better {
                best = Some((pos, lc_alias.len(), url.as_str()));
            }
        }
    }
    best.map(|(_, _, url)| url.to_owned())
}

/// Minimum-length sanity check on aliases. Matches R10's reasoning
/// for the `--repo` filter — single-character aliases produce
/// pathological false-positive density.
const MIN_ALIAS_LEN: usize = 2;

/// Aliases derived from a canonical repo URL. Per design §Q4:
///   - the full URL,
///   - the `owner/repo` path segment,
///   - the short name (basename minus `.git`),
///   - the short name with dashes stripped.
/// Duplicates pruned; aliases below [`MIN_ALIAS_LEN`] dropped at
/// match time.
fn aliases_for(url: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(4);
    let mut push = |s: String| {
        if !s.is_empty() && !out.iter().any(|existing| existing == &s) {
            out.push(s);
        }
    };
    push(url.to_owned());
    if let Some(slug) = owner_repo_for(url) {
        push(slug);
    }
    let short = short_name_for(url).to_owned();
    push(short.clone());
    let dashless: String = short.chars().filter(|c| *c != '-').collect();
    if dashless != short {
        push(dashless);
    }
    out
}

/// `owner/repo` for a GitHub-shaped URL. Defensive — returns `None`
/// when the URL doesn't carry two trailing path components.
fn owner_repo_for(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.trim_end_matches(".git");
    // For `git@host:owner/repo.git` the path lives after `:`; for
    // `https://host/owner/repo` it lives after the third `/`. The
    // common shape after the first prefix-strip is `…/owner/repo`,
    // so take the last two non-empty path components.
    let after_scheme = trimmed
        .splitn(2, "://")
        .nth(1)
        .unwrap_or(trimmed)
        .trim_start_matches('/');
    let body = after_scheme
        .splitn(2, ':')
        .last()
        .unwrap_or(after_scheme);
    // `body` is now e.g. `github.com/foo/bar` or `foo/bar`.
    let parts: Vec<&str> = body
        .split('/')
        .filter(|seg| !seg.is_empty())
        .collect();
    if parts.len() < 2 {
        return None;
    }
    let repo = parts[parts.len() - 1];
    let owner = parts[parts.len() - 2];
    // Guard against the `github.com/foo` (1-component) case where the
    // split picked up the host; require the owner to look like a path
    // segment, not a host.
    if owner.contains('.') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Basename of the path minus `.git`. Re-implemented here so the
/// module is self-contained for tests; mirrors `short_name_for` in
/// `main.rs` and `engine/src/work.rs`.
fn short_name_for(url: &str) -> &str {
    let after_slash = url.rsplit('/').next().unwrap_or(url);
    let after_colon = after_slash.rsplit(':').next().unwrap_or(after_slash);
    after_colon.trim_end_matches(".git")
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Show the known-repo list and read one selection. Returns `Ok(None)`
/// when the user gives an empty answer (treated as "skip the prompt").
fn interactive_ask(
    known_repos: &[String],
    product_slug: &str,
) -> Result<Option<String>, CliError> {
    let mut stderr = io::stderr().lock();
    writeln!(
        stderr,
        "No repo could be resolved for the new work item under product `{product_slug}`."
    )
    .map_err(CliError::internal)?;
    if known_repos.is_empty() {
        writeln!(
            stderr,
            "  This product has no known repos yet; enter a full URL."
        )
        .map_err(CliError::internal)?;
    } else {
        writeln!(stderr, "  Known repos for `{product_slug}`:").map_err(CliError::internal)?;
        for (idx, url) in known_repos.iter().enumerate() {
            writeln!(
                stderr,
                "    [{n}] {short}  ({url})",
                n = idx + 1,
                short = short_name_for(url)
            )
            .map_err(CliError::internal)?;
        }
        writeln!(
            stderr,
            "  Enter a number from the list above, or a full URL. Empty line skips."
        )
        .map_err(CliError::internal)?;
    }
    stderr.flush().map_err(CliError::internal)?;

    let mut stdout = io::stdout().lock();
    write!(stdout, "Repo: ").map_err(CliError::internal)?;
    stdout.flush().map_err(CliError::internal)?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(CliError::internal)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Ok(idx) = trimmed.parse::<usize>() {
        if (1..=known_repos.len()).contains(&idx) {
            return Ok(Some(known_repos[idx - 1].clone()));
        }
        return Err(CliError::usage(format!(
            "interactive repo selection: index {idx} is out of range (1..={n})",
            n = known_repos.len()
        )));
    }
    Ok(Some(trimmed.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(value: &str) -> String {
        value.to_owned()
    }

    fn task_with_repo(id: &str, repo: Option<&str>, updated_at: &str) -> Task {
        Task {
            id: id.to_owned(),
            product_id: "prod_1".to_owned(),
            project_id: None,
            kind: "chore".to_owned(),
            name: "x".to_owned(),
            description: String::new(),
            status: "todo".to_owned(),
            ordinal: None,
            pr_url: None,
            deleted_at: None,
            created_at: updated_at.to_owned(),
            updated_at: updated_at.to_owned(),
            autostart: true,
            last_status_actor: "human".to_owned(),
            priority: "medium".to_owned(),
            created_via: "cli".to_owned(),
            repo_remote_url: repo.map(str::to_owned),
            blocked_reason: None,
            blocked_attempt_id: None,
            effort_level: None,
            model_override: None,
            ci_attempt_budget: None,
            ci_attempts_used: 0,
            short_id: None,
            blocked_signals: Vec::new(),
        }
    }

    #[test]
    fn short_name_strips_dotgit_and_protocol() {
        assert_eq!(short_name_for("git@github.com:foo/nimbus.git"), "nimbus");
        assert_eq!(
            short_name_for("https://github.com/foo/nimbus.git"),
            "nimbus"
        );
        assert_eq!(short_name_for("https://github.com/foo/nimbus"), "nimbus");
    }

    #[test]
    fn aliases_include_full_url_owner_repo_short_and_dashless() {
        let aliases = aliases_for("https://github.com/spinyfin/mono-agent.git");
        assert!(aliases.contains(&s("https://github.com/spinyfin/mono-agent.git")));
        assert!(aliases.contains(&s("spinyfin/mono-agent")));
        assert!(aliases.contains(&s("mono-agent")));
        assert!(aliases.contains(&s("monoagent")));
    }

    #[test]
    fn owner_repo_handles_ssh_and_https() {
        assert_eq!(
            owner_repo_for("git@github.com:foo/bar.git").as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            owner_repo_for("https://github.com/foo/bar.git").as_deref(),
            Some("foo/bar")
        );
        assert_eq!(
            owner_repo_for("https://github.com/foo/bar").as_deref(),
            Some("foo/bar")
        );
        // Not enough path components.
        assert_eq!(owner_repo_for("https://github.com/foo").as_deref(), None);
    }

    #[test]
    fn parser_picks_short_name_in_prompt() {
        let known = vec![s("git@github.com:foo/nimbus.git")];
        let url = match_known_repo_in_prompt("In the nimbus repo, fix the deploy", &known);
        assert_eq!(url.as_deref(), Some("git@github.com:foo/nimbus.git"));
    }

    #[test]
    fn parser_is_case_insensitive() {
        let known = vec![s("git@github.com:foo/Nimbus.git")];
        let url = match_known_repo_in_prompt("touch the NIMBUS repo", &known);
        assert_eq!(url.as_deref(), Some("git@github.com:foo/Nimbus.git"));
    }

    #[test]
    fn parser_leftmost_position_wins_then_longer_alias_wins() {
        let known = vec![
            s("git@github.com:org/nimbus.git"),
            s("git@github.com:org/nimbus-frontend.git"),
        ];
        // `nimbus` and `nimbus-frontend` both match at the same position;
        // longer alias wins.
        let url = match_known_repo_in_prompt("fix the nimbus-frontend deploy", &known);
        assert_eq!(
            url.as_deref(),
            Some("git@github.com:org/nimbus-frontend.git")
        );
    }

    #[test]
    fn parser_returns_none_when_prompt_mentions_nothing() {
        let known = vec![s("git@github.com:foo/nimbus.git")];
        let url = match_known_repo_in_prompt("rewrite the docs", &known);
        assert!(url.is_none());
    }

    #[test]
    fn parser_skips_too_short_aliases() {
        // A repo named "x" would produce a 1-char alias; we drop it
        // to avoid spurious matches. The full URL still matches if
        // the prompt happens to include it verbatim.
        let known = vec![s("https://github.com/foo/x.git")];
        let url = match_known_repo_in_prompt("an example of a fox", &known);
        assert!(url.is_none());
    }

    #[test]
    fn collect_known_repos_dedups_and_includes_product_default() {
        let items = vec![
            task_with_repo("t1", Some("git@github.com:foo/nimbus.git"), "100"),
            task_with_repo("t2", Some("git@github.com:foo/nimbus.git"), "200"),
            task_with_repo("t3", None, "300"),
            task_with_repo("t4", Some("git@github.com:foo/ledger.git"), "400"),
        ];
        let known = collect_known_repos(Some("git@github.com:foo/console.git"), &items);
        // BTreeSet → sorted by URL.
        assert_eq!(
            known,
            vec![
                s("git@github.com:foo/console.git"),
                s("git@github.com:foo/ledger.git"),
                s("git@github.com:foo/nimbus.git"),
            ]
        );
    }

    #[test]
    fn recent_repo_picks_newest_updated_at() {
        let items = vec![
            task_with_repo("t1", Some("git@github.com:foo/nimbus.git"), "100"),
            task_with_repo("t2", Some("git@github.com:foo/ledger.git"), "300"),
            task_with_repo("t3", Some("git@github.com:foo/console.git"), "200"),
        ];
        assert_eq!(
            recent_repo_for_product(&items).as_deref(),
            Some("git@github.com:foo/ledger.git")
        );
    }

    #[test]
    fn recent_repo_skips_deleted_and_null() {
        let mut deleted = task_with_repo("t1", Some("git@github.com:foo/nimbus.git"), "500");
        deleted.deleted_at = Some("550".to_owned());
        let items = vec![
            deleted,
            task_with_repo("t2", None, "400"),
            task_with_repo("t3", Some("git@github.com:foo/ledger.git"), "100"),
        ];
        assert_eq!(
            recent_repo_for_product(&items).as_deref(),
            Some("git@github.com:foo/ledger.git")
        );
    }

    /// Acceptance: explicit `--repo` overrides every other signal,
    /// including a prompt that names a known repo and a product default.
    #[test]
    fn chain_explicit_overrides_parser_and_recent_and_default() {
        let known = vec![s("git@github.com:foo/nimbus.git")];
        let outcome = run_chain(
            Some("git@github.com:foo/console.git"),
            "in the nimbus repo",
            &known,
            Some("git@github.com:foo/ledger.git"),
            Some("git@github.com:foo/work.git"),
        );
        assert_eq!(
            outcome,
            ChainOutcome::Resolved {
                url: s("git@github.com:foo/console.git"),
                source: ResolutionSource::Explicit,
            }
        );
    }

    /// Acceptance: prompt parser overrides recent-context query and
    /// product default. Empty `--repo` (the "clear" wire form on
    /// update) is treated as "no explicit value" at create time.
    #[test]
    fn chain_parser_overrides_recent_and_default() {
        let known = vec![s("git@github.com:foo/nimbus.git")];
        let outcome = run_chain(
            Some(""),
            "in the nimbus repo, fix the deploy",
            &known,
            Some("git@github.com:foo/ledger.git"),
            Some("git@github.com:foo/work.git"),
        );
        assert_eq!(
            outcome,
            ChainOutcome::Resolved {
                url: s("git@github.com:foo/nimbus.git"),
                source: ResolutionSource::PromptParser,
            }
        );
    }

    /// Acceptance: recent-context overrides product default when the
    /// prompt doesn't name any known repo.
    #[test]
    fn chain_recent_overrides_default() {
        let known = vec![s("git@github.com:foo/nimbus.git")];
        let outcome = run_chain(
            None,
            "rewrite the docs",
            &known,
            Some("git@github.com:foo/ledger.git"),
            Some("git@github.com:foo/work.git"),
        );
        assert_eq!(
            outcome,
            ChainOutcome::Resolved {
                url: s("git@github.com:foo/ledger.git"),
                source: ResolutionSource::Recent,
            }
        );
    }

    /// Acceptance: product default is used when prompt and recent both
    /// whiff — the "ask-or-fail" branch is *not* entered.
    #[test]
    fn chain_default_overrides_ask_or_fail() {
        let outcome = run_chain(
            None,
            "rewrite the docs",
            &[],
            None,
            Some("git@github.com:foo/work.git"),
        );
        assert_eq!(
            outcome,
            ChainOutcome::Resolved {
                url: s("git@github.com:foo/work.git"),
                source: ResolutionSource::ProductDefault,
            }
        );
    }

    /// Acceptance: with no signals, the chain falls through to
    /// `AskOrFail`; the async wrapper picks ask-vs-fail based on the
    /// `interactive` flag.
    #[test]
    fn chain_falls_through_to_ask_or_fail_when_nothing_matches() {
        let outcome = run_chain(None, "rewrite the docs", &[], None, None);
        assert_eq!(outcome, ChainOutcome::AskOrFail);
    }
}
