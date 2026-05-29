# Design: OAuth Device-Flow Auth for Issue Sync

**Status:** Design. No code is written here. Output is this doc plus a
proposed breakdown into dependent implementation tasks.
**Parent project:** OAuth device-flow auth for issue sync.
**Builds on:** investigation T753 —
[`oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md`](../investigations/oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md)
(PR spinyfin/mono#897). Adopt its conclusions as constraints.
**Extends:** [`external-issue-tracker-sync-github-projects.md`](external-issue-tracker-sync-github-projects.md)
(the issue-sync design this auth work plugs into — Design Question 11,
"Credentials", is the seam this doc fills in).

---

## Overview

Boss's GitHub issue-sync reconciler currently authenticates **implicitly**:
`engine/src/external_tracker/credentials.rs` runs `gh auth status` once and,
on success, every `gh api` shellout inside `CommandGhRunner`
(`engine/src/external_tracker/github.rs`) inherits whatever login the user's
locally-installed `gh` happens to carry. The effective GitHub permissions are
invisible to Boss and unenforced: they are "whatever scopes the user's
`gh auth login` happened to grant." `TrackerCredential::ambient()`
(`external_tracker/mod.rs`) is literally an empty token marker that means
"use ambient auth."

This project replaces that implicit reliance with an **in-app OAuth device
authorization flow** that obtains an explicit GitHub **user token** with a
known scope set, stored securely, owned by the engine, and driven from the
**existing issue-sync settings** in the product UI (the `ExternalTrackerSection`
in `app-macos/Sources/ContentView.swift`, where a product is bound to an
org/repo/project).

Per T753, the chosen identity is an **OAuth App** (not a GitHub App): a GitHub
App user token only works on repos/orgs where the App is *installed*, and we
explicitly do not want per-repo installation. The device flow yields a
classic-scoped OAuth user token; the exact scope set T753 concluded is
**`repo project`**.

The split of responsibility follows the product's "engine owns reconciliation,
UI is a thin client" principle:

- **Engine owns** the device-flow state machine, the polling loop, the token,
  and keychain persistence. It exposes a few small RPC verbs and pushes auth
  state as events.
- **UI is a thin driver:** it sends "start authorization / cancel / disconnect"
  and renders whatever auth state the engine pushes (the `user_code`, the
  verification URL, polling/success/error). It never sees or stores the token.

---

## Goals

- Obtain a GitHub **user token** for issue sync via an **in-app OAuth device
  authorization flow**, with the **exact scope set T753 concluded** (`repo
  project`), so Boss controls and knows the token's scopes instead of
  inheriting whatever `gh auth login` carried.
- Drive the flow from the **existing issue-sync settings surface**
  (`ExternalTrackerSection`), not a new settings area: start authorization,
  show the user code + verification URL (optionally open the browser), show
  polling / success / error, and offer disconnect / re-authorize. Show current
  status: connected as which user, with which scopes.
- **Engine owns the flow and the token.** The device-code request, the poll
  loop (honoring `interval` / `authorization_pending` / `slow_down` / expiry),
  and token capture all run in the engine. The UI is a thin driver.
- **Secure token storage.** The token lives in the macOS Keychain (or
  equivalent), **never** in plaintext config and **never** in Boss runtime
  state (`state.db`) or any environment a worker can read.
- **Rewire issue sync** so the reconciler's GitHub calls use the stored OAuth
  token (REST + Projects v2 GraphQL) instead of ambient `gh`, with a defined
  migration/fallback for users currently relying on `gh`.
- **Surface org / SSO state.** When the OAuth App is not yet approved for the
  `spinyfin` org, or when SAML SSO authorization is required, the user is told
  exactly what org-owner / SSO action unblocks sync, and the flow recovers once
  it is taken.
- **Graceful failure handling** for network errors, denied/expired device
  codes, tokens rejected by org SSO, and revoked tokens — each with a distinct,
  actionable UI state.

## Non-Goals

- **A new settings area.** We extend the existing `ExternalTrackerSection`; we
  do not invent a separate "Accounts" or "Integrations" pane.
- **GitHub App / per-repo installation.** Rejected by T753 and re-confirmed
  here (see Alternatives). The `boss shake` GitHub App
  (`cli/src/github_app.rs`) that *creates* issues is a separate identity and is
  untouched by this work.
- **Consolidating the two GitHub identities.** After this ships there are two:
  the `shake` GitHub App (issue creation) and the sync OAuth App (read / close
  / status). Whether to merge them is deferred (T753 §4 open decision 6).
- **Fine-grained / least-privilege scoping.** T753 established that an OAuth App
  device flow can only mint coarse classic-scoped tokens; `repo` is
  all-or-nothing. Narrowing below `repo project` is impossible on this path and
  is explicitly out of scope.
- **Multi-account / multi-host.** v1 stores **one** github.com user token. GitHub
  Enterprise hosts, or per-product distinct accounts, are out of scope (the
  token is host-scoped to github.com and shared across all GitHub-bound
  products).
- **Programmatic server-side token revocation.** Revoking a token via
  `DELETE /applications/{client_id}/token` requires the OAuth App **client
  secret**, which we will not ship in the app (see Provisioning). "Disconnect"
  deletes the local token; full server-side revocation is a documented
  user-driven step in GitHub settings.
- **Registering the OAuth App.** Creating the App in the `spinyfin` org,
  enabling device flow, and obtaining the `client_id` is a human/setup
  prerequisite (see Provisioning). This design does **not** fabricate a
  `client_id` or any secret.
- **Refresh-token rotation.** OAuth App user tokens are non-expiring by default
  (see Token lifecycle); there is no refresh token to rotate. Expiring-token
  support is a GitHub App feature we are not using.

---

## Background: constraints inherited from T753

T753 audited the six GitHub operations the reconciler performs and concluded:

- **Scope string to request:** `repo project` (baseline: private
  `spinyfin/mono` + org-owned Project, current feature set). `repo` covers all
  issue read/write/comment/label operations across any org repo that can appear
  on the board; `project` covers the Projects v2 read **and** the
  `set_project_status` mutation (Behavior 6). Narrower variants exist
  (`public_repo` if every surfaced repo is public; `read:project` if Behavior 6
  is dropped) but the baseline is `repo project`.
- **OAuth App, not GitHub App.** Device flow for an OAuth App issues a
  classic-scoped token. A GitHub App user token only works where the App is
  installed — the per-repo install we are avoiding.
- **No least-privilege is achievable** on this path. Classic `repo` is coarse;
  the cross-repo nature of the board actually requires that breadth.
- **Org approval is a hard gate.** Until a `spinyfin` owner approves the Boss
  OAuth App, the token reaches only the org's *public* resources, which kills
  sync against a private repo + org-owned project. No OAuth-side workaround.
- **SAML SSO** (if `spinyfin` enforces it) requires the user to have an active
  SAML session when authorizing and to SSO-authorize the token; access can
  lapse and require re-authorization.

This design takes all of these as fixed and concerns itself with *how Boss
obtains, stores, and uses* such a token, and *how the UI surfaces the org/SSO
prerequisites*.

---

## Alternatives Considered

### Alternative A — GitHub App with per-repo (or org-wide) installation

Register a **GitHub App**, install it on `spinyfin`, and use a GitHub App user
token (fine-grained permissions: `issues:write`, `projects:write`,
`contents:read`).

**Why not.** This is the path T753 explicitly rejected and the project brief
re-rejects. A GitHub App user token only works on repos/orgs where the App is
*installed*; we do not want per-repo installation friction, and a fine-grained
token cannot enumerate "every repo that can appear on the board" because the
board's repo set is unbounded and grows over time. (T753 notes an *org-wide*
install could in principle be least-privilege and cover the cross-repo need —
but it is still an installation model the project decided against, and it
overlaps awkwardly with the existing `shake` GitHub App identity.) Fine-grained
permissions are attractive for least-privilege, but the install requirement is
the disqualifier.

### Alternative B — Manual fine-grained PAT pasted into Settings

Have the user mint a fine-grained or classic PAT in GitHub's UI and paste it
into a text field in the issue-sync settings. Boss stores it in the keychain.

**Why not.** Three problems. (1) It pushes scope selection onto the user, who
will over- or under-scope it — the exact opacity this project exists to remove.
(2) It is a worse UX than device flow: copy-paste of a long secret vs. typing a
short user code into a browser already logged into GitHub. (3) Fine-grained PATs
on an org require org-owner *approval per token* and expire on a fixed
schedule, reintroducing recurring manual toil. Device flow gives us a known,
fixed scope string we choose, and a browser-based consent that carries SSO.
(Note: the keychain *storage* and the `TrackerCredentialResolver` plumbing
designed below are reusable if a "bring-your-own-PAT" escape hatch is ever
wanted — but it is not the primary path.)

### Alternative C — Keep ambient `gh`, or use the OAuth browser (web) flow

(C1) Status quo: keep relying on `gh auth status`. (C2) Use the standard OAuth
**web** flow (authorization-code with a redirect URI) instead of device flow.

**Why not C1.** It is precisely what the project replaces: invisible,
unenforced scopes; a hard dependency on a correctly-logged-in `gh` binary on
the user's machine; no in-app status or control. (C1 is, however, retained as
the *fallback* path for un-migrated users — see Wiring §"Migration".)

**Why not C2.** The authorization-code web flow needs a redirect URI and, at
the token-exchange step, the OAuth App **client secret**. A desktop app cannot
keep a client secret confidential — embedding it in the shipped `.app` means
anyone can extract it. **Device flow is the purpose-built grant for clients
that cannot hold a secret:** the device-code → token exchange requires only the
public `client_id`, no secret, and no redirect URI / loopback HTTP server. It
also presents a clean "type this code in your browser" UX that naturally
carries org SSO authorization. This is the decisive reason device flow wins for
a distributed desktop app.

### Chosen: OAuth App + device authorization flow, engine-owned

The remainder of this document specifies it.

---

## Chosen Approach

### 1. Component ownership and end-to-end sequence

```
 ┌─────────────┐   FrontendRequest        ┌──────────────────────────────┐
 │  macOS app  │ ───────────────────────► │   engine (boss-engine)        │
 │  (thin UI)  │   GitHubAuthStart{}       │                               │
 │             │ ◄─────────────────────── │   github_oauth::DeviceFlow    │
 │ Settings →  │   FrontendEvent::         │     ├─ POST /login/device/code│
 │ External    │   GitHubAuthState{...}    │     ├─ poll /login/oauth/...  │
 │ Tracker     │   (pending→authorized→err)│     └─ KeychainTokenStore     │
 └─────────────┘                           └──────────────────────────────┘
        │                                              │
        │ user opens verification_uri,                 │ on success: store token
        │ types user_code in browser  ───────────────►│ in OS keychain; reconciler
        │ (carries org-approval + SSO)                 │ uses it via GH_TOKEN
        ▼                                              ▼
   github.com  ◄──────────── gh api (GH_TOKEN=<oauth>) ──────────── reconciler
```

The engine owns everything security-sensitive. The token never crosses back to
the app; the app only ever receives display-safe fields (`user_code`,
`verification_uri`, the connected login, the granted scopes, and error states).

### 2. Device-flow client (engine)

Lives in a new module `engine/src/external_tracker/github_oauth.rs`. It uses
the engine's existing `reqwest` HTTP client (already a workspace dependency) —
**not** `gh` — because the device-flow endpoints are unauthenticated
`github.com` endpoints, not `api.github.com`, and we want full control over the
`Accept: application/json` header and the poll timing.

**Step 1 — request device + user code.**

```
POST https://github.com/login/device/code
Accept: application/json
Body (form): client_id=<CLIENT_ID>&scope=repo%20project
→ 200 {
    "device_code":      "<opaque>",
    "user_code":        "WDJB-MJHT",
    "verification_uri": "https://github.com/login/device",
    "expires_in":       900,        // seconds (typically 15 min)
    "interval":         5           // min seconds between polls
  }
```

The engine returns `user_code`, `verification_uri`, `expires_in`, and `interval`
to the UI (via the `GitHubAuthState` event, state = `PendingUserAuth`). It keeps
`device_code` private (it is a bearer-equivalent secret for the poll step).

**Step 2 — present to user.** The UI shows the `user_code` and
`verification_uri` and offers "Open in browser" (which opens
`verification_uri` — GitHub then prompts the user to enter the code; some flows
support `verification_uri_complete` with the code pre-filled, which we use when
present). The browser step is where the user (a) logs into GitHub, (b) consents
to the requested scopes, (c) authorizes the App for the `spinyfin` org, and (d)
completes SAML SSO if enforced — all in one place.

**Step 3 — poll for the token.**

```
POST https://github.com/login/oauth/access_token
Accept: application/json
Body (form): client_id=<CLIENT_ID>
             &device_code=<device_code>
             &grant_type=urn:ietf:params:oauth:grant-type:device_code
```

Poll loop, sleeping `interval` seconds between attempts, handling the documented
error codes:

| Response                    | Action                                                        |
|-----------------------------|---------------------------------------------------------------|
| `access_token` present      | **Success.** Validate (Step 4), store, emit `Authorized`.     |
| `error=authorization_pending` | User hasn't finished yet. Keep polling at current interval. |
| `error=slow_down`           | Increase interval by 5s (GitHub's required backoff) and continue. |
| `error=expired_token`       | Device code expired (`expires_in` elapsed). Emit `Expired`; user must restart. |
| `error=access_denied`       | User clicked "Cancel" / denied. Emit `Denied`; stop.          |
| `error=incorrect_device_code` / `unsupported_grant_type` | Programming/config error. Emit `Error`; log. |
| HTTP 5xx / network error    | Transient. Retry on the next interval; do not abort the flow. |

The loop has a hard wall-clock cap at `expires_in` (plus a small grace) so it
cannot spin forever. A `GitHubAuthCancel` RPC aborts it early.

**Step 4 — validate the captured token.** Before declaring success, the engine
calls `GET https://api.github.com/user` with the token to capture the login,
and reads the `X-OAuth-Scopes` response header to record what was actually
granted (GitHub may grant fewer scopes than requested). It then runs the
**org/SSO probe** (§7) so the very first status the user sees already reflects
whether org approval / SSO is outstanding. Only after this does it persist and
emit `Authorized`.

### 3. Auth state machine

The engine tracks a single `GitHubAuthState` per host (github.com), persisted
in memory plus the durable token in the keychain (the *in-progress* flow state
is intentionally **not** persisted — see Risk R4):

```
Disconnected
   └─(GitHubAuthStart)→ RequestingCode
        └─(device/code 200)→ PendingUserAuth { user_code, verification_uri, expires_at, interval }
             ├─(token 200 + validate)→ Authorized { login, scopes, org_state }
             ├─(expired_token)→ Expired ──(GitHubAuthStart)→ RequestingCode
             ├─(access_denied)→ Denied
             └─(GitHubAuthCancel)→ Disconnected
   Authorized
        ├─(GitHubAuthDisconnect)→ Disconnected  (delete keychain item)
        ├─(token rejected 401 during sync)→ Reauthorize  (token revoked/invalid)
        └─(org/SSO probe fails)→ Authorized { org_state: NeedsOrgApproval | NeedsSso }
```

`org_state` is a sub-state of `Authorized`: the token is valid for the *user*
but may not yet reach private `spinyfin` resources. This is what powers the
distinct UI messaging in §7.

### 4. Engine ↔ App RPC additions

These follow the **exact** pattern already used by `SetProductExternalTracker`:
a `FrontendRequest` variant in `protocol/src/wire.rs`, an input struct in
`protocol/src/types.rs`, a handler arm in `engine/src/app.rs`, a `send*` method
in `app-macos/Sources/EngineClient.swift`, and a bridge method in
`app-macos/Sources/ChatViewModel.swift`.

New **requests** (app → engine):

```rust
// protocol/src/wire.rs — FrontendRequest variants
GitHubAuthStart      {}            // begin device flow (host = github.com)
GitHubAuthCancel     {}            // abort an in-progress flow
GitHubAuthDisconnect {}            // delete stored token, return to Disconnected
GitHubAuthStatus     {}            // request current state (engine replies with an event)
```

New **event** (engine → app), pushed on the existing frontend socket the same
way `WorkItemUpdated` is:

```rust
// protocol/src/wire.rs — FrontendEvent variant
GitHubAuthState { state: GitHubAuthStateDto }

// protocol/src/types.rs
pub enum GitHubAuthStateDto {
    Disconnected,
    RequestingCode,
    PendingUserAuth { user_code: String, verification_uri: String,
                      verification_uri_complete: Option<String>,
                      expires_at: i64, interval_seconds: u32 },
    Authorized { login: String, granted_scopes: Vec<String>,
                 org_state: OrgAuthState },
    Expired,
    Denied,
    Error { message: String },
}

pub enum OrgAuthState { Ok, NeedsOrgApproval { request_url: String },
                        NeedsSso { sso_url: String }, Unknown }
```

The DTO carries **only display-safe** fields. `device_code` and the access
token are never in any DTO. This is the boundary that satisfies "the UI never
sees the token."

**Why a pushed event rather than a reply.** The flow is long-lived (the user
takes seconds-to-minutes in the browser). Modeling it as request→reply would
force the UI to poll. Instead the UI fires `GitHubAuthStart` once and then
re-renders on each `GitHubAuthState` event the engine pushes as the poll loop
advances — identical in spirit to how work-item updates stream today.

> **Note on the engine→app `EngineRequest` channel.** The engine-app-rpc design
> adds a separate `FrontendEvent::EngineRequest` / `FrontendRequest::EngineResponse`
> pair for engine-*initiated* calls (pane spawning). We do **not** need it here
> for the auth flow itself (auth is app-initiated). It *is* the mechanism we
> would use if we chose app-mediated keychain storage (see §5 alternative).

### 5. Token storage

**Requirements (from the brief):** secure storage; never plaintext config;
never in `state.db`; never readable by workers.

**How the constraints are met structurally.** The reconciler runs **inside the
engine process**, not inside worker (`claude`) processes. Workers are spawned
into libghostty panes with a specific env (`BOSS_LEASE_ID`,
`BOSS_EVENTS_SOCKET`, …) that does **not** include the token. The only process
that ever holds the token is the engine; the only place it is exposed to a
child is the `GH_TOKEN` env of the `gh` subprocesses the engine *itself* spawns
for sync (§6) — those are children of the engine, never of a worker. So the
"not readable by workers" guarantee is a property of *where the token is read
and used*, enforced independently of the at-rest store.

**Chosen at-rest store: engine-owned OS keychain via the `keyring` crate.**
The `keyring` crate (v3, `apple-native` feature) is already vendored in this
workspace and proven by `tools/hood/src/creds.rs` (which stores a Robinhood
OAuth token in the macOS keychain). The engine writes a generic-password item:

```
service: "dev.spinyfin.boss.github"
account: "oauth-user-token@github.com"
value:   JSON { token, granted_scopes, login, obtained_at }
```

A new `KeychainTokenStore` in `external_tracker/github_oauth.rs` wraps
`keyring::Entry` with `get` / `set` / `delete`. This keeps the entire flow
self-contained in the engine, with **no dependency on the app being connected**
to read the token at sync time, and matches the project's "engine owns the
token" directive and the `TrackerCredentialResolver` extension point that
`external-issue-tracker-sync` §11 already anticipated ("the PAT lives in the OS
keychain … resolved by a different `TrackerCredentialResolver` impl").

A new resolver replaces/augments `GhAuthStatusResolver`:

```rust
// external_tracker/credentials.rs
pub struct KeychainOAuthResolver { store: KeychainTokenStore, fallback: GhAuthStatusResolver }

impl TrackerCredentialResolver for KeychainOAuthResolver {
    async fn resolve(&self, kind, config) -> Result<TrackerCredential, _> {
        match self.store.get()? {
            Some(rec) => Ok(TrackerCredential { token: rec.token }),     // OAuth token wins
            None      => self.fallback.resolve(kind, config).await,      // ambient gh
        }
    }
}
```

`TrackerCredential.token` already exists and already means "non-empty = explicit
token, empty = ambient" — so the type does not change, only the resolver.

**Token lifecycle.**

- **Expiry / refresh.** OAuth **App** user tokens are **non-expiring** by default
  and carry **no refresh token**. (Expiring user tokens with refresh are a
  GitHub *App* feature; we are an OAuth App and will leave expiration off.) So
  there is no refresh loop. "Re-auth" is simply re-running the device flow,
  which overwrites the stored token.
- **Revocation / disconnect.** `GitHubAuthDisconnect` deletes the keychain item
  and returns to `Disconnected`. Because we ship no client secret, Boss cannot
  call `DELETE /applications/{client_id}/token` to revoke server-side; the
  disconnect UI therefore also links to GitHub → Settings → Applications →
  Authorized OAuth Apps so a user who wants full server-side revocation can do
  it. (A token detected as already-revoked during sync — 401 — transitions to
  `Reauthorize` and the keychain item is cleared.)
- **Clearing on disconnect** is unconditional and local even if the network is
  down.

**Alternative storage considered — app-mediated `APIKeyStore`.** The app
already has `APIKeyStore` (`app-macos/Sources/Settings/APIKeyStore.swift`),
which stores the Anthropic API key in the **data-protection keychain**
(`kSecUseDataProtectionKeychain`, `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`)
gated by the app's `keychain-access-groups` entitlement, with a `0600` file
fallback for ad-hoc dev builds. We could store the OAuth token there and have
the engine fetch it over the `EngineRequest` channel. **Trade-off:** the
data-protection keychain is *more* secure (device-only, entitlement-scoped,
unavailable to the unentitled engine binary), but it (a) crosses the
engine/app boundary for a token the brief says the engine should own, (b)
introduces an "app must be online" dependency for the engine to read the token
each sync, and (c) couples sync availability to the GUI. Given "engine owns the
token" + "engine reconciler must work whether or not the app is foregrounded,"
**engine-direct keychain is chosen**; app-mediated storage is the natural
upgrade if engine keychain access proves unreliable (see Risk R1) or if the
stronger data-protection guarantees become a requirement. **No plaintext file
fallback is offered for the OAuth token** — unlike the Anthropic key, a
`repo`-scoped GitHub token is write-capable and higher-value; in environments
without a usable keychain the resolver reports `Disconnected` and sync falls
back to ambient `gh` rather than writing the token to disk in plaintext.

### 6. Wiring into issue sync

The reconciler (`external_tracker/reconcile.rs`) already obtains a
`TrackerCredential` through a `TrackerCredentialResolver` and passes it in
`TrackerContext.credential`. Two changes thread the token to the wire:

1. **Resolver swap.** The engine constructs `KeychainOAuthResolver` (§5) instead
   of the bare `GhAuthStatusResolver`. When a stored OAuth token exists,
   `TrackerCredential.token` is the OAuth token; otherwise it is the ambient
   empty marker.

2. **`GhRunner` honors the token.** `CommandGhRunner`
   (`external_tracker/github.rs`) currently constructs each `gh` call as
   `Command::new("gh")` with no explicit env. Change it to carry the credential
   and, when `token` is non-empty, set `GH_TOKEN=<token>` on each invocation
   (`gh` honors `GH_TOKEN`). This is the minimal-blast-radius integration T753
   recommended: the GraphQL and REST call sites are otherwise unchanged, and the
   scope analysis is identical whether calls go through `gh` or raw HTTP.
   Concretely, `CommandGhRunner` gains a `token: Option<String>` field set from
   `TrackerContext.credential`, and each of `graphql` / `rest_get` /
   `rest_patch` / `rest_post` adds `.env("GH_TOKEN", token)` when present.

   > A future hardening (not v1) could drop `gh` for sync entirely and use
   > `reqwest` with an `Authorization: Bearer` header, removing the `gh` binary
   > dependency. Out of scope here; `GH_TOKEN` is the smaller, lower-risk step.

**Precedence and migration.**

- **Precedence:** a stored OAuth token always wins. If present, sync uses it; the
  ambient `gh` path is not consulted.
- **Migration for existing `gh` users:** until a user completes the device flow,
  `KeychainOAuthResolver` falls through to `GhAuthStatusResolver` and sync keeps
  working exactly as today. There is **no forced cutover** — existing users are
  unaffected until they click "Connect." The settings UI shows which mode is
  active ("Connected via OAuth as @user" vs. "Using local `gh` login").
- **After connecting:** the OAuth token takes precedence on the very next
  reconcile tick. Disconnecting reverts to the ambient `gh` fallback.
- The reconciler's existing failure classification (transient / permission /
  not-found) is reused; a new **401 (token invalid/revoked)** maps to an
  auth-attention item and flips the auth state to `Reauthorize` (§8).

### 7. Org / SSO handling

T753 established that a valid user token can still be inert against private
`spinyfin` resources until (a) an org owner approves the OAuth App and (b), if
SAML is enforced, the user SSO-authorizes the token. The design surfaces and
recovers from both:

**Detection (the org/SSO probe, run at Step 4 and on sync 403s).** The engine
issues a cheap probe against an org-private resource — the bound product's
`organization(login).projectV2` GraphQL query (the same call `fetch_items`
makes). It classifies the result:

- **Success** → `OrgAuthState::Ok`.
- **403 / 200-with-null-org** where the same token can read *public* resources
  → `NeedsOrgApproval`. GitHub's UI exposes a "request access" / owner-approval
  page for the org; we link to
  `https://github.com/orgs/spinyfin/policies/applications` (owner) and surface
  the user-facing "request approval" affordance.
- **403 with an `X-GitHub-SSO: required; url=<...>` header** → `NeedsSso`,
  carrying the SSO authorization URL GitHub provides in that header. The user
  opens it, establishes a SAML session, and authorizes the token for the org.

**Recovery UX.** Each non-`Ok` state renders a distinct, actionable banner in
the issue-sync settings:

- *NeedsOrgApproval:* "Connected as @user, but the Boss app is not yet approved
  for the **spinyfin** organization. An org owner must approve it before sync
  can read private issues. [Open org settings] [Re-check]."
- *NeedsSso:* "Your token needs SAML SSO authorization for **spinyfin**.
  [Authorize via SSO] [Re-check]." (The button opens the `sso_url` from the
  header.)

"Re-check" re-runs the probe without re-doing the whole device flow. The
reconciler also re-probes automatically when a sync call returns 403, so the
banner clears on its own once the owner approves / the user SSO-authorizes.

### 8. Failure handling (consolidated)

| Failure | Where detected | Engine behavior | UI state |
|---|---|---|---|
| Network error on `device/code` | Step 1 | Retry briefly, then abort flow | `Error{message}` + retry affordance |
| Network error / 5xx while polling | Step 3 | Keep polling at interval; do not abort | stays `PendingUserAuth` |
| `slow_down` | Step 3 | interval += 5s | unchanged |
| `authorization_pending` | Step 3 | keep polling | `PendingUserAuth` (spinner) |
| Device code expired | Step 3 | stop poll | `Expired` → "Start over" |
| User denied in browser | Step 3 | stop poll | `Denied` |
| Granted scopes < requested | Step 4 | store anyway, record actual scopes | `Authorized` + "limited scopes" note |
| Org not approved | Step 4 / sync 403 | `org_state=NeedsOrgApproval` | org-approval banner |
| SAML SSO required | Step 4 / sync 403 | `org_state=NeedsSso` (capture `sso_url`) | SSO banner |
| Token revoked / invalid | sync 401 | clear keychain item; `Reauthorize` | "Reconnect" prompt + attention item |
| Permission denied on write (403, has scope but org policy) | sync (close/label) | existing attention item; do not retry | product attention item |
| Keychain unavailable | resolve | log; fall back to ambient `gh` | "Using local gh login" + warning |

All sync-time auth failures also raise a **`WorkAttentionItem`** on the affected
product (reusing the existing attention-item surface that
`ExternalTrackerAttentionTests` already covers), so the problem is visible even
if the user isn't in the settings sheet.

### 9. OAuth App provisioning prerequisite (human/setup task)

**This is a setup prerequisite, not something the implementation can do, and no
`client_id` or secret is fabricated here.** Before the device-flow code can
function end to end, a human must:

1. **Register an OAuth App in the `spinyfin` org** (Settings → Developer
   settings → OAuth Apps → New). Name it (e.g. "Boss"), set a homepage URL. A
   callback URL is required by the form but unused by device flow (any valid URL
   is fine).
2. **Enable device flow** on the App ("Enable Device Flow" checkbox) — without
   it, `POST /login/device/code` is rejected.
3. **Provision the resulting `client_id` into the app.** The `client_id` is
   **public** (not a secret) and is the only credential the device flow needs.
   It should be supplied to the engine the same way other build-time identifiers
   are — e.g. a compile-time `option_env!("BOSS_GITHUB_OAUTH_CLIENT_ID")`
   constant (mirroring how `cli/src/github_app.rs` embeds
   `BOSS_SHAKE_APP_ID`) — or an engine config field. **No client secret is
   embedded** (device flow doesn't need one, and a desktop app can't keep one).
4. **Org owner approves the App for `spinyfin`** (OAuth App access policy) — the
   hard gate from T753. Document this in a runbook.
5. **Document the SAML SSO authorization step** if `spinyfin` enforces SSO.

Until (1)–(3) are done, the device-flow code has no `client_id` and the
"Connect" button should be disabled with an explanatory tooltip ("GitHub OAuth
App not configured in this build"). The implementation tasks below treat the
`client_id` as an injected constant and **must not** invent one.

---

## Proposed Implementation Task Breakdown

Each task is one PR. Ordering reflects dependencies; tasks at the same depth can
proceed in parallel.

**T-0 (prerequisite, human/setup — not a code PR).** Register the `spinyfin`
OAuth App, enable device flow, obtain `client_id`, get org-owner approval,
write the org-approval + SSO runbook. Blocks end-to-end verification of T2–T5
but **not** their implementation (which can be developed against a test/personal
OAuth App `client_id`).

**T-1 — Protocol additions** (`boss-protocol`). Add the `GitHubAuthStart`,
`GitHubAuthCancel`, `GitHubAuthDisconnect`, `GitHubAuthStatus` `FrontendRequest`
variants; the `GitHubAuthState` `FrontendEvent` variant; and the
`GitHubAuthStateDto` / `OrgAuthState` types with serde + round-trip tests.
Wire-format only; no behavior. *Depends on: none.*

**T-2 — Device-flow client + state machine** (engine). New
`external_tracker/github_oauth.rs`: `DeviceFlow` (device-code request, poll loop
honoring `interval`/`authorization_pending`/`slow_down`/expiry/`access_denied`),
token validation (`GET /user`, `X-OAuth-Scopes`), and the `GitHubAuthState`
machine. `client_id` read from an injected constant/config. Unit tests with a
mock HTTP server covering each poll branch. *Depends on: T-1.*

**T-3 — Keychain token storage** (engine). `KeychainTokenStore` over
`keyring::Entry` (service/account from §5); `KeychainOAuthResolver` that prefers
the stored token and falls back to `GhAuthStatusResolver`. Tests follow the
`hood`/`APIKeyStore` pattern (inject a fake backend; never touch the real
keychain in CI). *Depends on: T-2 (consumes the captured token); can develop in
parallel with T-2 against a stub.*

**T-4 — Engine RPC handlers + auth-flow orchestration** (engine, `app.rs`).
Handle the four new requests; own the single per-host `GitHubAuthState`; push
`GitHubAuthState` events as the flow advances; run the org/SSO probe; raise
attention items on auth failure. *Depends on: T-1, T-2, T-3.*

**T-5 — Sync rewiring** (engine, `external_tracker/github.rs` + reconciler
construction). Thread `TrackerCredential.token` into `CommandGhRunner` as
`GH_TOKEN`; swap in `KeychainOAuthResolver`; map sync-time 401 → `Reauthorize` +
attention item; map 403 → org/SSO re-probe. Tests assert `GH_TOKEN` is set when
a token is present and unset (ambient) when not. *Depends on: T-3.* *(Can land
before or after T-4; they touch different engine areas.)*

**T-6 — Settings UI** (`app-macos`). Extend `ExternalTrackerSection` in
`ContentView.swift`: a "GitHub account" subsection with Connect / Disconnect /
Re-authorize, the `user_code` + verification URL display with "Open in
browser," polling/success/error/expired/denied states, the org-approval and SSO
banners, and a status line ("Connected as @user · scopes: repo, project" /
"Using local gh login"). Add `send*` methods to `EngineClient.swift` and bridge
methods to `ChatViewModel.swift`; handle the `GitHubAuthState` event. Swift
tests mirror `ExternalTrackerTests` (DTO decode, state rendering). *Depends on:
T-1 (wire types), T-4 (engine handlers).*

**T-7 — `boss` CLI parity + runbook** (cli + docs). Optional `boss github auth
{login,status,logout}` verbs that drive the same engine RPCs (useful for
headless/testing), and the org-owner approval + SSO runbook under
`tools/boss/docs/runbooks/`. *Depends on: T-4.*

Critical path: **T-1 → T-2 → T-3 → T-4 → T-6**. T-5 branches off T-3; T-7
branches off T-4. T-0 gates *acceptance* (real end-to-end auth against
`spinyfin`) but not development.

---

## Risks / Open Questions

- **R1 — Engine keychain access reliability.** The engine is a child of the
  app but is a distinct binary without the app's `keychain-access-groups`
  entitlement, so it uses the **login** keychain (generic password) via
  `keyring`, not the app's data-protection keychain. If the engine binary's
  code-signing identity / path changes between builds, macOS may prompt or deny
  on re-read. **Open question:** is engine-direct keychain access stable enough
  across our dev + Developer-ID builds, or should we adopt the app-mediated
  `APIKeyStore` route (§5 alternative) despite the app-online dependency?
  *Needs a reviewer decision before T-3.*

- **R2 — `gh` vs raw HTTP for sync.** v1 keeps `gh` + `GH_TOKEN` for minimal
  blast radius. Does `gh` reliably prefer `GH_TOKEN` over an ambient `gh auth`
  login in all configurations (e.g. when `gh` has its own keyring entry)? If
  not, we may need `GH_TOKEN` + `GH_CONFIG_DIR` isolation, or to move sync to
  `reqwest`. *Verify empirically in T-5.*

- **R3 — Scope confirmation (carried from T753).** Baseline `repo project`
  assumes private `spinyfin/mono` + Behavior 6 on. If `mono` is public,
  `public_repo` suffices; if Behavior 6 is dropped, `read:project` suffices.
  Also re-confirm `read:org` is genuinely not required against the *real* org
  project (T753 marked this high-confidence but not empirically verified).
  *Resolve before T-0 finalizes the App's requested scopes.*

- **R4 — In-progress flow not persisted across engine restart.** If the engine
  restarts mid-flow (between device-code issuance and token capture), the
  in-progress state is lost and the user must click "Connect" again; the worst
  case is a dangling unused device code that expires harmlessly. **Decision:**
  acceptable for v1 (the durable thing — the token — *is* persisted). Flag if a
  reviewer wants restart-survivable flow state.

- **R5 — Org approval is an existential dependency (T753 §4.4).** If the
  `spinyfin` owner will not approve a third-party OAuth App, this entire
  direction is blocked and we fall back to an org-wide GitHub App install or a
  member-minted SSO-authorized PAT. **Confirm `spinyfin`'s OAuth App access
  policy and SSO posture before investing in T-2+.**

- **R6 — Two GitHub identities.** `shake` (GitHub App, issue creation) and sync
  (OAuth App, read/close/status) will both touch the org/project. Keep them
  separate (chosen for v1: simpler, two consents) or consolidate later (T753 §4
  open decision 6)? *Out of scope here; noted for a future project.*

- **R7 — No programmatic revocation.** Because we ship no client secret,
  "Disconnect" only deletes the local token; full server-side revocation is a
  user-driven step in GitHub settings. **Open question:** is local deletion +
  documented manual revoke acceptable, or do we need a tiny server-side
  component holding the client secret to offer one-click revoke? *v1 recommends
  local-only; confirm with reviewer.*

## References

- T753 investigation —
  [`oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md`](../investigations/oauth-device-flow-scopes-vs-issue-sync-2026-05-28.md)
  (PR spinyfin/mono#897).
- [`external-issue-tracker-sync-github-projects.md`](external-issue-tracker-sync-github-projects.md)
  — the sync design this auth work plugs into (esp. §11 Credentials).
- [`engine-app-rpc.md`](engine-app-rpc.md) — the frontend-socket request/event
  pattern these RPC additions follow.
- Code touched by the implementation tasks:
  `engine/src/external_tracker/{credentials.rs, github.rs, reconcile.rs, mod.rs}`,
  `engine/src/app.rs`, `protocol/src/{wire.rs, types.rs}`,
  `app-macos/Sources/{ContentView.swift, EngineClient.swift, ChatViewModel.swift,
  Models.swift}`; storage precedents `tools/hood/src/creds.rs` and
  `app-macos/Sources/Settings/APIKeyStore.swift`; provisioning precedent
  `cli/src/github_app.rs`.
- GitHub docs: [Authorizing OAuth apps — device flow](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow),
  [Scopes for OAuth apps](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/scopes-for-oauth-apps),
  [About OAuth app access restrictions](https://docs.github.com/en/organizations/managing-oauth-access-to-your-organizations-data/about-oauth-app-access-restrictions).
