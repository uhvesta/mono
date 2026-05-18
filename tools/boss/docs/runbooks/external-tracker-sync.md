# External Tracker Sync

This runbook covers binding an external issue tracker (GitHub Projects) to a Boss product, manually linking work items, troubleshooting authentication, and interpreting attention items.

See [`tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md`](../designs/external-issue-tracker-sync-github-projects.md) for the architecture and design rationale.

## Binding an external tracker to a product

Use `boss product set-external-tracker` to bind an external GitHub project to a Boss product. All work items under the product inherit the binding.

```sh
boss product set-external-tracker <product-id-or-slug> \
  --kind github \
  --org <github-org> \
  --repo <github-repo> \
  --project <project-number>
```

**Parameters:**
- `<product-id-or-slug>`: The Boss product to bind (can be the product name, slug, or UUID).
- `--kind github`: Currently only GitHub is supported.
- `--org`: GitHub organization name (e.g., `spinyfin`).
- `--repo`: GitHub repository name (e.g., `mono`).
- `--project`: GitHub project number (visible in the project's URL: `github.com/orgs/<org>/projects/<number>`).

**Example:**
```sh
boss product set-external-tracker Boss --kind github --org spinyfin --repo mono --project 1
```

After binding, the reconciler automatically:
- Pulls new upstream issues into Boss as chores.
- Mirrors upstream status changes (when an issue closes upstream, the Boss work item moves to `done`).
- Attaches PR URLs to Boss work items when a PR is linked upstream.
- Closes upstream issues when a linked PR merges in Boss.

### Enabling reverse-close (optional)

By default, Boss only closes upstream issues when a linked PR merges. To also close upstream issues when a Boss work item is marked `done` *without* a merged PR, enable reverse-close:

```sh
boss product set-external-tracker <product-id-or-slug> --reverse-close
```

**What reverse-close does:**
When `--reverse-close` is enabled, marking a Boss work item as `done` (without a merged PR driving the transition) also closes the corresponding upstream issue. This is useful if your Boss and upstream tracker are tightly synchronized and you want all status changes to flow both ways.

**When to enable:**
- Enable if your team uses Boss as the primary source of truth and wants upstream issues closed automatically.
- Leave disabled (default) if upstream issues are filed publicly and you want to avoid closing them without explicit evidence of shipment.

**Important:** Closing a public GitHub issue is visible to other humans. Understand the implications before enabling this flag.

### Unbinding a tracker

To remove the binding:

```sh
boss product set-external-tracker <product-id-or-slug> --unset
```

This clears the binding but does not delete any Boss work items. Existing items retain their `external_ref` for potential re-binding.

## Manually linking work items

### Linking a Boss work item to an upstream issue

If a Boss work item was created before the tracker binding, or if you want to manually associate an existing item with an upstream issue:

```sh
boss task link-external <work-item-id> --kind github --id <owner>/<repo>#<number>
```

**Parameters:**
- `<work-item-id>`: The Boss task or chore ID.
- `--kind github`: Tracker kind (currently only GitHub).
- `--id <owner>/<repo>#<number>`: The upstream issue identifier (e.g., `spinyfin/mono#560`).

**Example:**
```sh
boss task link-external T-abc123 --kind github --id spinyfin/mono#560
```

The next reconcile pass will populate the `web_url` and `raw` fields with upstream metadata.

### Unlinking a work item

To clear the external reference:

```sh
boss task unlink-external <work-item-id>
```

This clears the active binding but retains the `canonical_id` columns so the reconciler can re-bind automatically if the upstream issue reappears in the project.

## Triggering an on-demand sync

By default, the reconciler runs every 2 minutes. To manually trigger a sync pass for a single product without waiting:

```sh
boss product sync-external-tracker <product-id-or-slug>
```

## Troubleshooting authentication failures

Boss uses your local `gh` authentication to access GitHub. If the reconciler cannot resolve credentials, no items sync and an attention item surfaces on the product.

### Checking your `gh` login

```sh
gh auth status
```

**Expected output:**
```
  ✓ Logged in to github.com as <your-username> (keyring)
  ✓ Git operations for github.com configured to use ssh protocol.
  ✓ Token: ghu_****
  ✓ Token scopes: repo
```

If you see `✗ You are not logged in to any GitHub hosts`, or if the `Token scopes` line does not include `repo`, log in:

```sh
gh auth login
```

When prompted, select:
- **Protocol:** `ssh` (or `https` if ssh is not configured).
- **Scopes:** Be sure to include `repo` (for public and private repositories).

### Verifying write scope for reverse-close

If reverse-close is enabled on the product, the credential must have write scope (`issues:write` is part of the `repo` scope). Confirm:

```sh
gh auth status
```

Should include `repo` in scopes. If you only have read access, re-run `gh auth login --scopes repo` to grant write permission.

### Engine startup and credential caching

The engine resolves `gh` credentials once at startup and caches the result. If you log in after the engine starts, restart the engine for the change to take effect:

```sh
# Kill any running Boss engine process
pkill -f 'boss.*engine' || true

# The next `boss` command will restart the engine
boss product show <product-id>
```

## Interpreting external tracker attention items

When the reconciler detects problems, it surfaces attention items on the product. Each attention item carries a `kind='external_tracker'` and a specific `reason`.

### `config_invalid`

**Message:** *"External tracker binding points at `<org>/<repo>` project `<number>` which does not exist or is not visible."*

**Cause:** The product's external tracker configuration points at a GitHub project that:
- Does not exist.
- Exists but is private and your `gh` login lacks access.
- Was moved to a different organization or deleted.

**Fix:**
1. Verify the project number in the GitHub UI (`github.com/orgs/<org>/projects/<number>`).
2. Confirm your `gh` login has access: `gh auth status`.
3. Update the binding with correct parameters: `boss product set-external-tracker <product> --kind github --org <org> --repo <repo> --project <number>`.

### `auth_failed`

**Message:** *"Boss could not resolve GitHub credentials. Run `gh auth status` and ensure you are logged in."*

**Cause:** The `gh auth status` check failed, typically because:
- You are not logged in (`gh auth login` not run).
- Your `~/.config/gh/` or `~/.ssh/` is corrupted.
- The engine started before you logged in and cached the failure.

**Fix:**
1. Run `gh auth login` and complete the login flow.
2. Confirm `gh auth status` shows `✓ Logged in to github.com as <username>`.
3. Restart the engine (see "Engine startup and credential caching" above).

### `permission_denied`

**Message:** *"Boss could not close upstream issue `<canonical_id>`: credential lacks write scope. Re-run `gh auth login --scopes repo` to grant write permission."*

**Cause:** Typically appears when:
- Reverse-close is enabled (`--reverse-close`) but your credential lacks write scope.
- A PR merge triggered close-on-merge (always on) but your credential lacks write scope.

**Fix:**
1. Run `gh auth login --scopes repo` to refresh your login with write scope.
2. Restart the engine.
3. The attention item clears once the next reconcile pass succeeds or observes the issue already closed.

### `transient_failure`

**Message:** *"Boss has been unable to close `<canonical_id>` after a merged PR. Last error: <classified-reason>. The Boss work item is already `done`; the upstream issue may be closed manually or Boss will retry indefinitely."*

**Cause:** A temporary network issue, GitHub API outage, or rate limit prevented the close attempt. Boss retried multiple times (default: 10 times, ~20 minutes) without success.

**Fix:**
1. Wait for the GitHub incident to resolve (check [GitHub Status](https://www.githubstatus.com/)).
2. Once GitHub is healthy, the reconciler automatically retries on the next tick.
3. If the issue persists, manually close the upstream issue via GitHub and the attention item will clear on the next sync.

## Reverse-close flag reference

| Flag | Default | Behavior | Example |
|------|---------|----------|---------|
| `--reverse-close` | Disabled | When a Boss work item is marked `done` (without a merged PR), Boss also closes the upstream issue. | `boss product set-external-tracker <product> --reverse-close` |
| (not specified) | Enabled | When a PR linked to a Boss work item merges, Boss closes the upstream issue (close-on-merge). | Always active; no flag needed. |

**When reverse-close closes upstream issues:**
- A Boss user marks a work item `done` in the app or CLI.
- The work item has an external reference (is bound to an upstream issue).
- The upstream issue is currently `Open`.
- The product has `reverse_close` enabled.

**When reverse-close does NOT close upstream issues:**
- The upstream issue is already `Closed`.
- The work item status is not `done`.
- `reverse_close` is disabled (default).
- The work item has no external reference.

**Important caveat:** Close-on-merge is always enabled and does not require `--reverse-close`. Behavior 5 (close-on-merge) fires independently of reverse-close, whenever a PR merge is detected.

## Seeing the reconciler in action

### Metrics

The reconciler emits Prometheus metrics. Common ones:
- `external_tracker.fetch_succeeded` — successful upstream fetches.
- `external_tracker.imported` — new upstream items imported as Boss chores.
- `external_tracker.closed` — work items flipped to `done` because upstream closed.
- `external_tracker.pr_attached` — PR URLs attached to work items.
- `external_tracker.pr_merge_close_succeeded` — upstream closes after PR merge (Behavior 5).
- `external_tracker.reverse_close_succeeded` — upstream closes from reverse-close (Behavior 3).

View metrics via the engine's metrics endpoint (if exposed).

### Logs

The engine logs each reconcile pass. Look for lines like:
```
external_tracker: reconciling product <product_id>
external_tracker: product <product_id> result: 5 imported, 2 closed, 1 pr_attached
```

## FAQ

**Q: Why doesn't my locally-created Boss work item sync upstream?**

A: The reconciler only pulls *downstream* — it imports upstream issues into Boss but does not create upstream issues when you create a Boss work item. To link a pre-existing Boss item to an upstream issue, use `boss task link-external`.

**Q: Can I bind the same upstream issue to multiple Boss products?**

A: No. One upstream issue can be bound to at most one Boss work item. If you link it to a second item, the first binding clears.

**Q: What happens if my upstream issue title changes?**

A: The Boss work item name is mirrored at *create time* only. Later upstream renames do not re-sync the name — you own the Boss title freely.

**Q: Can I use reverse-close to auto-reopen closed issues?**

A: No. Reverse-close is one-way: Boss → upstream close only. If an upstream issue closes and a Boss user later reverts the work item to `active`, Boss does not re-open the upstream issue.

**Q: How often does the reconciler run?**

A: Every 120 seconds (2 minutes) by default. Use `boss product sync-external-tracker <product>` for on-demand sync.
