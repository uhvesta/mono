# Design: Automatic Boss updates

Status: **Draft — for review**
Owner: Boss
Related: [`installable-distribution-package-for-boss.md`](installable-distribution-package-for-boss.md) (this is the deferred "Auto-update / Sparkle / a release feed" follow-on it names), [`buildkite-release-setup.md`](../buildkite-release-setup.md)

## Context

Boss is distributed as a macOS `.app` bundle. The Buildkite release pipeline (`.buildkite/steps/boss-release.sh`) cuts a GitHub Release on `spinyfin/mono`, tagged `boss-v1.0.N`, with a single asset `Boss-1.0.N.zip` — a zipped, codesigned `Boss.app`. Users today have no in-app way to learn a newer build exists; they must notice the release manually and re-download. The installable-package design explicitly deferred auto-update to a follow-on project — this is that project.

This document designs an in-app self-update mechanism with three settings-gated modes (manual check, periodic badge notification, automatic background install). It polls the **unauthenticated** GitHub REST API — no token, no `gh` dependency.

### What I verified against the live repo and codebase

These facts shaped the design; they are stated here so reviewers can re-check them:

- **`spinyfin/mono` is publicly readable unauthenticated.** `GET https://api.github.com/repos/spinyfin/mono/releases` returns `HTTP 200` with no `Authorization` header, and the response carries `X-RateLimit-Limit: 60` (the unauthenticated per-IP core limit). The unauthenticated premise is sound *as long as the repo stays public* (see Risks).
- **Releases are tagged `boss-v1.0.N`**, titled `Boss 1.0.N`, with one asset `Boss-1.0.N.zip` (~34 MB) downloadable at `https://github.com/spinyfin/mono/releases/download/boss-v1.0.N/Boss-1.0.N.zip`. The asset is a zipped `Boss.app` bundle produced by `bazel build //tools/boss/app-macos:Boss`.
- **`mono` is a monorepo with multiple release lines.** Alongside `boss-v*` it carries `checkleft-v*` tags (e.g. `checkleft-v0.1.0-alpha.8`). `GET /releases/latest` returns whichever line released most recently — it happened to be `boss-v1.0.27` when I checked, but a later `checkleft` release would make `/releases/latest` return a non-Boss tag. **`/releases/latest` is therefore unsafe; we must list releases and filter by the `boss-v` prefix.**
- **Boss tags are not returned in version order.** The `/releases` listing is sorted by publish date, and observed ordering interleaves (`…1.0.27, 1.0.26, …, 1.0.9, 1.0.8, 1.0.18, 1.0.17…`). We must parse *all* `boss-v` tags and pick the maximum, not take the first.
- **Some releases lack the asset.** `boss-v1.0.21` exists with no `Boss-1.0.21.zip`. The updater must treat "newest tag that actually has a downloadable asset" as the target and skip assetless releases.
- **The running version is in the bundle Info.plist.** `pkg.bzl`'s `boss_short_version_plist` stamps `CFBundleShortVersionString = CFBundleVersion = "1.0.N"` (numeric, even on dev builds — it uses `STABLE_BOSS_BASE_VERSION`) and `BossFullVersion = "1.0.N"` on a release tag or `"1.0.N-dev-<sha>"` on a dev build (`STABLE_BOSS_VERSION`). `BossMacApp.swift:24-31` already reads `BossFullVersion` for the About panel.
- **The published zip is _not_ notarized.** `boss-release.sh` zips the bazel build output directly; only the separate `installer/release.sh` (`.pkg` path) runs `codesign`/`notarytool`/`stapler`, and that `.pkg` is **not** what GitHub Releases publishes. This is fine for v1: the updater replicates the existing manual workflow (terminal `curl` + unzip + copy), which also works without notarization because command-line downloads do not set `com.apple.quarantine`. See §4 for how quarantine-stripping makes un-notarized releases work reliably.
- **The app may live in `~/Applications`, not `/Applications`.** `installer/release.sh` runs `pkgbuild --install-location ~/Applications` with the `currentUserHomeDirectory` domain ("install for me only", no admin password). The updater must resolve its own location via `Bundle.main.bundleURL` rather than hard-coding `/Applications`.
- **The engine is a bundled sub-binary.** `EngineProcessController` launches `<Bundle.main.resourcePath>/bin/engine` detached and owns its lifecycle. Swapping the app bundle replaces that binary too, so the swap must coordinate an engine restart.
- **Settings today are engine-side and boolean-only.** `engine/src/settings.rs` defines a static `REGISTRY` of boolean `SettingSpec`s persisted to `<state_root>/settings.toml` and surfaced via RPC in `SettingsView.swift`. The app separately uses `@AppStorage` for app-local UI state (e.g. `boss.activity.visible` in `BossMacApp.swift:108`).

---

## Goals

1. **Detect** when a newer Boss release exists in `spinyfin/mono`, using only the unauthenticated GitHub REST API.
2. **Three settings-gated modes**, matching the project brief:
   - **Manual check** — a "Check for Updates…" app-menu item that checks on demand and reports the result.
   - **Periodic badge notification** — interval polling that surfaces a badge/button in the main window's top-right chrome when an update is available; clicking it offers download/install.
   - **Automatic install** — a "Automatically install updates" toggle that downloads new releases in the background as they appear, into `~/Library/Application Support`, and swaps the installed bundle at the next safe boundary (quit or startup, whichever succeeds first).
3. **Respect the 60-req/hour unauthenticated rate limit** with a conservative interval and conditional requests.
4. **Safe install on macOS** — preserve code-signing/notarization validity, never leave the user with a broken/half-swapped bundle, and require no admin password in the common (`~/Applications`) case.
5. **Graceful failure** — network errors, partial downloads, failed swaps, and a new build that won't launch must all degrade to "keep running the current version" with a clear, non-blocking signal.

## Non-goals

- **Delta/binary-diff updates.** We download the full `Boss-1.0.N.zip` (~34 MB). No bsdiff/courgette.
- **Updating the engine or workers independently of the app.** The engine ships inside the app bundle; it is replaced atomically with the app. No separate engine update channel.
- **Downgrade / pin-to-version / channel selection (beta vs stable).** Only "is there a newer `boss-v1.0.N`" matters. No release channels in v1.
- **Auto-updating dev builds.** A build whose `BossFullVersion` contains `-dev-` is a local/unreleased build; the updater will *report* availability but never auto-install over it (see Failure handling).
- **A general-purpose updater framework** reusable by `checkleft` or other monorepo products. This is Boss-specific; a shared abstraction can come later if a second product needs it.
- **Changing the release pipeline's tag/version scheme or assets.** We consume `boss-v1.0.N` + `Boss-1.0.N.zip` exactly as they exist today; no pipeline changes are required for v1. Notarizing the zip is a potential future improvement, not a prerequisite.
- **Push notifications / server-initiated updates.** Detection is pull-only polling.

---

## Alternatives considered

### Alternative A — Adopt Sparkle

[Sparkle](https://sparkle-project.org/) is the de-facto macOS auto-update framework. It handles the appcast feed, download, signature verification (EdDSA), the in-place swap via a separate `Autoupdate`/`Installer` XPC helper, and relaunch — exactly the hard parts.

**Why not chosen (for v1):**
- **Feed mismatch.** Sparkle consumes an *appcast* XML feed, not the GitHub Releases API. We'd have to generate and publish an appcast (e.g. via `generate_appcast` to GitHub Pages or a gh-pages branch) as a *new* pipeline artifact. That's a real release-pipeline change and a second source of truth to keep in sync with `boss-v1.0.N`.
- **Signing model mismatch.** Sparkle wants an EdDSA signature over each archive, with the public key embedded in the app, plus its own key-management. Boss already relies on Apple Developer-ID + notarization for trust; adding a parallel EdDSA scheme is more surface, not less.
- **UI mismatch.** The project specifies a *custom* 3-mode model and a badge in the window chrome. Sparkle's built-in UI is a modal "A new version is available" sheet; bending it to our chrome-badge + background-auto-install model means using Sparkle's lower-level API anyway, which erodes the "it does it for you" benefit.
- **Dependency weight & SPM.** Adds a sizable third-party dependency (and its XPC helper, which must itself be signed/sandboxed correctly) to an app that currently has exactly one SPM dependency (`textual`).

Sparkle remains the right call *if* we later want robust staged rollouts, phased percentages, and a battle-tested in-place swapper. It is explicitly revisitable (see Open questions). For v1 the custom approach is a better fit for the GitHub-Releases-as-feed + custom-UI requirements, and reuses trust infrastructure we already have.

### Alternative B — Re-run the installer `.pkg` (delegate the swap to `installer`)

Reuse the existing notarized `.pkg` path: have the updater download a `.pkg`, then shell out to `installer(8)` (or open it in `Installer.app`) to perform the swap, exactly as a fresh install does.

**Why not chosen:**
- **The `.pkg` isn't a release asset.** GitHub Releases publish `Boss-1.0.N.zip`, not a `.pkg`. Adopting this means *also* publishing the `.pkg` to every release — another pipeline change and a larger asset.
- **No silent path.** `installer -pkg … -target CurrentUserHomeDirectory` works without admin for the `~/Applications` domain, but driving it from a background "automatic install" without any user interaction is awkward and historically fragile; `Installer.app` is interactive by design.
- **Heavier than needed.** The zip *is* a complete, signed bundle. For self-update we don't need the packaging layer at all — we need an atomic directory swap of one `.app`. The `.pkg` machinery (receipts, scripts, distribution XML) buys us nothing here and adds moving parts.

It stays the right tool for *first* install (Gatekeeper-friendly, double-click UX). For *self*-update, a direct bundle swap is simpler and faster.

### Alternative C (chosen) — Custom in-app updater over the GitHub Releases API

A small Swift module in the app that polls `GET /repos/spinyfin/mono/releases` unauthenticated, compares versions, downloads `Boss-1.0.N.zip` to Application Support, verifies signature + notarization, and performs an atomic in-place bundle swap at a safe boundary. Rationale above; details below.

---

## Chosen approach

A new `Updater` actor in `tools/boss/app-macos/Sources/Update/`, plus a small UI layer (menu item, chrome badge, Settings pane). It has four responsibilities — **check**, **download/stage**, **swap**, **surface** — described in turn.

### 1. Version detection

**Current running version.** Read `CFBundleShortVersionString` (`"1.0.N"`, numeric) for comparison and `BossFullVersion` (`"1.0.N"` or `"1.0.N-dev-<sha>"`) to detect dev builds. Both are already in `Info.plist`; `BossMacApp.swift:24` shows the pattern. A build is "dev" iff `BossFullVersion` contains `-dev-`; dev builds short-circuit auto-install.

**Endpoint.** `GET https://api.github.com/repos/spinyfin/mono/releases?per_page=100` with headers:
- `Accept: application/vnd.github+json`
- `X-GitHub-Api-Version: 2022-11-28`
- `User-Agent: Boss/<version>` (GitHub requires a UA; missing UA → 403)
- `If-None-Match: <stored ETag>` when we have one (see rate limits)

We deliberately do **not** use `/releases/latest` (returns the wrong product in this monorepo) and do **not** use the tags API (no asset metadata). One page of 100 is far more than the ~28 `boss-v` releases that exist; if pagination is ever needed we stop as soon as we've seen a `boss-v` tag older than our current version (they're roughly date-descending).

**Selection algorithm:**
1. Filter releases to those whose `tag_name` matches `^boss-v(\d+)\.(\d+)\.(\d+)$`, excluding `draft` and `prerelease`.
2. Parse each to a `(major, minor, patch)` tuple.
3. Discard any release that has no asset named `Boss-<major>.<minor>.<patch>.zip` (handles the `boss-v1.0.21`-style assetless release).
4. Pick the **maximum** tuple (not the first in the list — the list isn't version-sorted).
5. Compare against the running `(major, minor, patch)` parsed from `CFBundleShortVersionString` using tuple ordering (semver-style; future-proof if `minor`/`major` ever move beyond the current `1.0.x`).
6. If `latest > current` → an update is available; carry the `tag_name`, version, asset `browser_download_url`, asset `size`, and release `body` (markdown notes, for the UI).

**Why tuple compare, not "max integer N":** today everything is `1.0.x` so comparing the patch integer suffices, but encoding the assumption "major and minor are always 1.0" into the comparator is a latent bug the day someone cuts `1.1.0`. Tuple compare costs nothing and removes the trap.

**Rate limits.** Unauthenticated = **60 requests/hour/IP**, shared across everything on that IP. Mitigations:
- **Conservative default interval: every 6 hours**, plus one check shortly after launch (jittered 30–120 s so a fleet of machines behind one NAT doesn't thundering-herd the same minute). 6 h ⇒ ~4 checks/day/app, leaving ample headroom even with several Boss installs and other GitHub usage sharing the IP.
- **Conditional requests.** Persist the response `ETag`; send `If-None-Match`. A `304 Not Modified` is cheap and, per GitHub's documented policy, conditional `304`s do not count against the primary rate limit — so steady-state polling of an unchanged release list is effectively free. (We still treat the limit as real and back off regardless; see below.)
- **Honor `Retry-After` / secondary limits.** On `403`/`429` with `Retry-After`, or when `X-RateLimit-Remaining: 0`, suspend polling until `X-RateLimit-Reset`. Never retry-storm.
- **Manual checks bypass the interval** but still share the budget; a manual check that hits the limit reports "rate-limited, try again at HH:MM" rather than erroring opaquely.

### 2. Settings model

The brief asks whether the three modes are independent settings or one selector. **Chosen: a single 3-state mode selector**, because the modes are a strict escalation ladder, not orthogonal switches:

| Mode | Periodic poll? | Badge on new version? | Auto-download? | Auto-swap? |
|---|---|---|---|---|
| **Manual only** | no | no | no | no |
| **Notify** *(default)* | yes (6 h) | yes | no | no |
| **Automatic** | yes (6 h) | yes (until swapped) | yes | yes (quit/startup) |

The "Check for Updates…" menu item works in **all** modes — it is always-on and not gated. Modeling this as three independent booleans would allow nonsensical combinations (auto-install ON but polling OFF), so a single enum is clearer and impossible to misconfigure.

**Default = Notify.** Rationale: detection with zero silent mutation of the user's `/Applications` is the least surprising default; auto-install is opt-in. (Reviewer decision point — see Open questions; an argument exists for `Automatic` as default once notarization lands.)

**Where the setting lives — app-side `@AppStorage`, not engine settings.** The existing engine `settings.rs` registry is **boolean-only** and a 3-state enum doesn't fit it without extending the value type. More fundamentally, the updater is a pure *app* concern: it polls, downloads, and swaps from the app process, and it must read its mode **at startup and at quit, when the engine may not be running** (indeed, post-swap the engine binary on disk is the *new* one). Tying the update mode to engine RPC state would couple a UI-process decision to a separate process's lifecycle for no benefit. The app already uses `@AppStorage` for app-local state (`BossMacApp.swift:108`). We add:

```
@AppStorage("boss.update.mode")              // "manual" | "notify" | "automatic"; default "notify"
@AppStorage("boss.update.lastCheck")         // epoch seconds, for the Settings "last checked" line
@AppStorage("boss.update.skippedVersion")    // optional "1.0.N" the user dismissed in Notify mode
// ETag + staged-download bookkeeping live in the staging dir manifest, not UserDefaults.
```

**UI placement:** a new **"Updates"** tab in `SettingsView`'s `TabView` (alongside Workers / Engine / Feature Flags), bound directly to `@AppStorage`. It shows the mode picker (segmented or radio), the current version, "Last checked: …", a "Check now" button, and — in Automatic mode — a one-line status ("Up to date" / "1.0.N downloaded, will install on quit"). This pane reads/writes `UserDefaults` directly and does **not** go through `chatModel.refreshSettings()`/RPC, unlike the other panes.

### 3. Download & staging

**Location.** `~/Library/Application Support/Boss/Updates/`. (`Application Support/Boss/` is already the app's home for `state.db`, `release-config.toml`, etc.) Layout:

```
~/Library/Application Support/Boss/Updates/
  staging/                       ← in-progress download (temp), never a complete version
  1.0.28/
    Boss-1.0.28.zip              ← downloaded asset (kept until superseded)
    Boss.app/                    ← extracted, verified bundle ready to swap in
    manifest.json                ← { version, tag, etag, sha256, sourceURL, verifiedAt, state }
```

**Atomicity of download.** Download with `URLSession` (background `URLSessionDownloadTask` so it survives app-state changes and supports resume) to a temp file under `staging/`. Only after the download completes **and** integrity verification passes do we `rename(2)` it into `Updates/<version>/` — a rename is atomic within the same filesystem, so a crash mid-download never leaves a partial file masquerading as a ready version. The `manifest.json` `state` field (`downloading` → `verifying` → `ready` → `failed`) is the source of truth on next launch; any directory whose manifest isn't `ready` is garbage-collected.

> **Quarantine-stripping (chosen approach for v1):** The current manual update flow — `curl` download → `unzip` → `cp -R` into `/Applications` — works today without notarization because command-line tools do **not** set `com.apple.quarantine`. Gatekeeper only assesses notarization when that xattr is present; without it, the app launches freely. The updater replicates this: after extracting the staged bundle, we explicitly run `xattr -dr com.apple.quarantine <bundle>` before any swap. This is *necessary* (not just defensive) because an app-initiated `URLSession` download **can** receive the quarantine xattr from the system; stripping it ensures Gatekeeper never blocks an un-notarized release. Notarization remains a deferred future improvement (trust signal + robustness), but it is explicitly **not a blocker for v1**.

**Integrity verification (in order, all must pass before a download is marked `ready`):**
1. **Transport** — HTTPS to `api.github.com` / `github.com` / `objects.githubusercontent.com`. (We follow GitHub's redirect to the signed object-store URL.)
2. **Size** — bytes received == asset `size` from the API.
3. **Unzip integrity** — extract with `ditto -x -k` (the same tool `boss-release.sh` uses to build/verify the zip), failing on any error.
4. **Code signature** — `codesign --verify --deep --strict` on the extracted `Boss.app`, and confirm the signing identity's Team ID matches the *currently running* bundle's Team ID (so a swap can never move us to a differently-signed bundle).
5. **Quarantine strip** — `xattr -dr com.apple.quarantine <bundle>` (see the quarantine-stripping note above). This is the final step before the bundle is marked `ready`, ensuring Gatekeeper never has a chance to assess notarization on the swapped-in bundle. `spctl --assess` is intentionally **not** run in v1; it would fail on un-notarized releases and is not needed given the quarantine-strip guarantee.

There is **no published checksum or detached signature asset** today (only the `.zip`). Integrity therefore rests on HTTPS + Apple code-signing (step 4), which combined with quarantine-stripping (step 5) is sufficient for v1. *If* we later want defense-in-depth, the cheapest additions are a `Boss-1.0.N.zip.sha256` asset checked at step 3 and notarization (deferred; see §4 and Risks); both are optional pipeline enhancements, not v1 requirements.

**Cleanup rule ("delete older versions on success").** After a download is marked `ready`, delete every other `Updates/<version>/` directory whose version is **≤** the newly-staged version. We keep exactly one staged version (the newest ready one). We never delete a directory mid-verify. Cleanup is also run at launch to sweep any `state != ready` leftovers from an interrupted run.

### 4. Install / swap mechanics

The unit of swap is the app's *own* bundle, located via `Bundle.main.bundleURL` (works whether installed in `~/Applications` or `/Applications`).

**Privilege.**
- `~/Applications` (the installer's default) is **user-writable** → swap needs no admin password. This is the common case and the only one we make seamless in v1.
- `/Applications` may require admin rights to replace. If `Bundle.main.bundleURL` is not writable by the current user, we **do not** silently escalate. We surface "Update ready — quit Boss and replace it via the downloaded copy" and reveal the staged `Boss.app` in Finder (or fall back to opening the `.pkg`-style flow). Privileged swap via `SMJobBless`/an admin helper is explicitly out of scope for v1 (Open questions).

**The core problem: you can't replace a running bundle's executable while it's mapped.** macOS lets you `rename` a running `.app` directory (the open file handles keep referencing the old inode), but you cannot guarantee a clean swap of a *running* process's own bundle and then relaunch from it in one step without a tiny external helper. So the swap happens at a **boundary where Boss is (about to be) not running**, per the brief's "quit or startup, whichever can be performed successfully":

**Swap strategy — a minimal external relaunch helper script.** When a `ready` update exists and the mode is `automatic` (or the user clicked "Install & Relaunch"):

- **On quit** (`applicationWillTerminate` / a pre-quit hook): if no agents are running (the app already gates quit on `activeAgentCount`, `BossMacApp.swift:203`), write the swap plan and `Process`-launch a detached `/bin/sh` helper that: waits for Boss's PID to exit → moves the current bundle to `Updates/<oldversion>/Boss.app.bak` (atomic rename) → moves the staged `Boss.app` into the install location (atomic rename) → `open`s the new bundle → on any failure, rolls back by renaming `.bak` back. The helper is tiny, shipped as a resource, and runs after we're gone. This is the "swap on quit" path and is preferred because the swap completes immediately and the user lands on the new version next time they launch (the helper relaunches only on explicit "Install & Relaunch"; a plain quit just swaps and exits).
- **On startup** (`applicationDidFinishLaunching`, *before* the engine launches): if a `ready` update exists *and* the swap-on-quit helper didn't already apply it (e.g. the app crashed, or the bundle was locked last time), apply it now while the bundle is freshly launched and least likely to be busy, then **relaunch into the new version** and exit the old one.

"Whichever can be performed successfully" ⇒ **quit is tried first; startup is the fallback.** If the quit-time swap fails (bundle busy, rename across volumes, permission denied), the staged `ready` version simply remains and the next startup retries. We never block quit on a failed swap — quit always proceeds.

**Engine coordination.** `EngineProcessController` owns a detached engine spawned from `<bundle>/Contents/Resources/bin/engine`. The swap replaces that binary on disk. Sequence: the helper relies on the app having already called `EngineProcessController.stop()` during normal termination; on the startup path, the swap happens *before* the engine is launched, so the new engine binary is what gets spawned. Either way the new app launches the new engine — no stale-engine window.

**Gatekeeper, quarantine, and signing — three risks that must be handled regardless of notarization status.**

**(1) Quarantine-stripping (App Translocation avoidance).** The staged bundle is stripped of `com.apple.quarantine` before swap (see §3 step 5). This matters for two reasons: (a) it prevents Gatekeeper from blocking an un-notarized bundle at launch, and (b) it prevents **App Translocation** — macOS's feature that runs a quarantined app from a randomized read-only shadow mount instead of its actual path. An updater that runs from a Translocation mount cannot reliably locate or replace its own bundle, because `Bundle.main.bundleURL` would point to the shadow path, not `/Applications/Boss.app`. Stripping quarantine before the first launch of the staged bundle ensures Translocation never activates.

**(2) Quit-vs-startup swap of a running bundle.** macOS allows `rename(2)` on a running `.app` directory (open file handles keep referencing the old inode), but replacing a running process's own executable in-place while it is mapped is unsafe. The chosen boundary strategy (swap-on-quit via detached helper, swap-on-startup as fallback, see above) ensures the bundle is not actively running when the rename is performed. The helper explicitly waits for Boss's PID to exit before touching the directory.

**(3) Signature invalidation.** If the bundle is Developer-ID-signed, in-place *modification* of any signed file (as opposed to an atomic rename of the whole bundle) would break the signature and could cause the kernel to SIGKILL the new process on first exec. The chosen approach avoids this: the staged bundle is a complete, self-contained replacement and is swapped in via atomic `rename(2)` of the top-level `.app` directory — the signature is never touched. Boss releases built with `bazel build //tools/boss/app-macos:Boss` are currently **ad-hoc-signed** (build output, not Developer-ID; `boss-release.sh` does not call `codesign`), so signature invalidation is not an active concern today, but the atomic-rename strategy remains correct regardless of signing level.

**Notarization is deferred.** Un-notarized bundles work fine after quarantine-stripping: Gatekeeper only assesses notarization when the quarantine xattr is present. Notarization would add a trust signal (cryptographic proof of Apple scan) and robustness (survives future Gatekeeper policy tightening), but it is explicitly **not required for v1** and is recorded in Risks as a future improvement, not a blocker.

**Rollback.** The `.bak` of the previous bundle is retained until the new version has launched successfully once (a "first launch OK" flag written by the new version on `applicationDidFinishLaunching`). If the new version fails to launch (see Failure handling), the startup helper restores `.bak`.

### 5. UI surfaces

**(a) Menu item — always available.** Add `Button("Check for Updates…")` in a `CommandGroup(after: .appInfo)` in `BossMacApp.swift` (directly under "About Boss", the macOS-conventional spot). Clicking runs a check and presents result state:
- *Up to date* — a brief sheet/alert "Boss 1.0.N is the latest version."
- *Update available* — a sheet showing the new version, the release notes (`body` markdown, rendered with the existing `Textual`/markdown stack), and buttons: **Install & Relaunch** (if a swap is feasible) / **Download** / **Later** / **Skip this version**.
- *Error / rate-limited* — inline message with the reason and, if rate-limited, when to retry.
- *Dev build* — "You're running a development build (1.0.N-dev-…). Latest release is 1.0.M." with download disabled by default.

**(b) Window-chrome badge — Notify/Automatic modes.** When a newer version is known and not yet applied (and not "skipped"), show a button in the main window's **trailing toolbar region** — a new `ToolbarItem(placement: .primaryAction)` in `ContentView`'s `.toolbar` block (the same region as the existing `ToolbarItemGroup(placement: .primaryAction)`, which renders top-right under `.windowToolbarStyle(.unified)`). Appearance: a small pill/button, `arrow.down.circle.fill` SF Symbol with an accent tint (and, in Automatic mode once the download is staged, a subtle "ready" check). Clicking it opens the same update sheet as the menu item. This mirrors the existing `EngineHealthBanner` precedent (`ContentView.swift:77`) for non-modal chrome signaling, but as a compact trailing button rather than a full-width banner, since an available update is informational, not an error.

**(c) Progress & error feedback.**
- Download progress (Automatic mode or after "Download") surfaces as determinate progress on the chrome badge's popover and in the Settings "Updates" pane; we reuse the `ProgressView` idiom already in `SettingsView`.
- Swap-in-progress is brief; the main feedback is simply relaunching into the new version.
- Errors are non-blocking: a transient status line in the Settings pane + the badge reverting to "update available" (or disappearing if the check now says up-to-date). We never throw a modal that interrupts work.

### Component summary (for the implementation tasks)

```
tools/boss/app-macos/Sources/Update/
  UpdateChecker.swift     — unauth GitHub Releases fetch, ETag cache, filter+select, version compare
  UpdateModel.swift       — ObservableObject: mode, availableVersion, downloadState, badge visibility
  UpdateDownloader.swift  — background URLSession download → staging → verify → ready, cleanup rule
  UpdateInstaller.swift   — swap plan, relaunch-helper script, quit/startup boundary logic, rollback
  UpdateSettingsView.swift— the "Updates" Settings tab
  Resources/relaunch-helper.sh — tiny detached swap+relaunch script
```
Plus edits to `BossMacApp.swift` (menu item, startup swap hook, `@AppStorage` wiring), `ContentView.swift` (chrome badge toolbar item), and `SettingsView.swift` (register the new tab).

---

## Failure handling

| Failure | Detection | Behavior |
|---|---|---|
| **Network unreachable / DNS / timeout** | `URLSession` error | Silent in periodic mode (log only); explicit "couldn't reach GitHub" in manual mode. Retry on next interval. |
| **Rate-limited (`403`/`429`, `X-RateLimit-Remaining: 0`)** | response headers | Suspend polling until `X-RateLimit-Reset`/`Retry-After`. Manual check reports "try again at HH:MM". |
| **Malformed / unexpected API response** | JSON decode / no `boss-v` releases | Treat as "no update"; log. Never crash on schema drift. |
| **Newest release has no usable asset** | asset filter (§1 step 3) | Skip it; consider the next-newest `boss-v` release with an asset. |
| **Partial / interrupted download** | size mismatch or task error; manifest `state != ready` | Discard staging temp; retry next interval. The atomic-rename rule means a partial never looks ready. |
| **Integrity check fails** (bad unzip / `codesign` reject / Team-ID mismatch) | §3 steps 3–5 | Mark `failed`, delete the staged dir, do **not** swap, surface "update could not be verified". |
| **Swap fails** (bundle busy, cross-volume rename, permission denied) | helper exit code / pre-flight writability check | Leave `ready` staged version in place; retry at next boundary. Quit still proceeds. For `/Applications`-without-write, fall back to "reveal in Finder / manual install". |
| **New version won't launch** (crashes before "first launch OK" flag) | startup helper sees prior launch left no success flag within a watchdog window | Roll back: restore `Boss.app.bak`, mark that version `failed` so we don't re-attempt it, surface "update rolled back". |
| **Dev build** | `BossFullVersion` contains `-dev-` | Report availability but never auto-install; "Download" still allowed for manual testing. |
| **Agents running at quit** | existing `activeAgentCount` gate (`BossMacApp.swift:203`) | Do not swap-on-quit if the user cancels quit; the staged version waits for the next clean quit/startup. |

---

## Risks / open questions

1. **🟢 Notarization deferred — quarantine-stripping is sufficient for v1.** The published `Boss-1.0.N.zip` is un-notarized, and the updater does **not** require notarization. Quarantine-stripping (`xattr -dr com.apple.quarantine`) before swap replicates the existing manual workflow (terminal `curl` + unzip + copy), which works today for the same reason. Notarization would add a trust signal (Apple's malware scan) and robustness against future Gatekeeper policy changes, making it a worthwhile future improvement — but it is explicitly **not a blocker** for the first version, and no pipeline changes are required to ship v1. When notarization is added later, §3's integrity verification can add a `spctl --assess` step and the quarantine-strip remains correct as a belt-and-suspenders measure.
2. **🟠 Repo must stay public.** The whole design relies on `spinyfin/mono` being unauthenticated-readable (verified: HTTP 200, 60/hr). If the repo is ever made private, every check returns 404 and updates silently stop. *Mitigation if that happens:* publish releases (or a manifest) to a dedicated public repo (e.g. `spinyfin/boss-releases`) or a public bucket/Pages site, and point the updater there. *Reviewer: is "public forever" an acceptable assumption, or should we design the public-manifest indirection now?*
3. **🟠 `/Applications` privileged swap is out of scope.** v1 is seamless only for the user-writable `~/Applications` install (the installer's default). For a `/Applications` install we degrade to a guided manual swap. *Reviewer: is that acceptable for v1, or do we need an `SMAppService`/privileged-helper swap?*
4. **🟡 Default mode.** Proposed default is **Notify** (no silent mutation of installed apps). Is **Automatic** the better default for an internal tool where everyone wants latest? *Reviewer decision.*
5. **🟡 Shared-IP rate limiting.** 60/hr is per-IP, shared across all GitHub traffic from that IP (multiple Boss installs behind one office NAT, plus `gh`, plus browsers). The 6 h interval + conditional `304`s should keep us well under budget, but a large co-located fleet is a theoretical concern. *Mitigation already in design: jittered launch check + ETag + hard backoff on `X-RateLimit-Remaining: 0`.*
6. **🟡 Engine-restart UX during swap.** Swapping replaces the bundled engine binary; the new app spawns the new engine. Edge case: an in-flight engine operation at quit. The existing `activeAgentCount` quit-gate covers running agents, but non-agent engine work (e.g. an in-progress RPC) isn't gated. *Likely fine — the engine is restart-tolerant by design — but worth a reviewer nod.*
7. **🟢 Optional integrity hardening.** No checksum/signature asset is published today; integrity rests on HTTPS + notarization. A cheap `Boss-1.0.N.zip.sha256` asset would add defense-in-depth. Not required for v1; flag if reviewers want it.
8. **🟢 Sparkle revisit.** If we later want phased rollouts / staged percentages / a hardened third-party swapper, migrating to Sparkle (Alternative A) is the natural path. The custom module is intentionally small to keep that door open.

---

## Proposed implementation task breakdown

These are the follow-on tasks to file once this design is approved. No pipeline changes are prerequisites; all tasks can proceed against the existing un-notarized releases.

- **T1:** `UpdateChecker` — unauthenticated `/releases` fetch with required headers, ETag conditional requests, `boss-v` filtering, assetless-release skipping, max-tuple selection, semver-tuple comparison vs `CFBundleShortVersionString`, dev-build detection. Unit tests with canned API JSON (including the interleaved-order and missing-asset cases observed live).
- **T2:** `UpdateModel` (`ObservableObject`) + `@AppStorage` mode/skipped-version/last-check wiring; the 3-state mode enum and the polling scheduler (6 h interval, jittered launch check, rate-limit backoff).
- **T3:** "Check for Updates…" menu item (`CommandGroup(after: .appInfo)`) + the update result sheet (up-to-date / available + notes / error / dev-build), reusing the markdown renderer for release notes.
- **T4:** Window-chrome badge — trailing `ToolbarItem(placement: .primaryAction)` in `ContentView`, visibility driven by `UpdateModel`, popover with download/install affordance.
- **T5:** "Updates" Settings tab (`UpdateSettingsView`) registered in `SettingsView`'s `TabView`; mode picker, current version, last-checked, "Check now", staged-status line.
- **T6:** `UpdateDownloader` — background `URLSession` download to `Updates/staging/`, size + `ditto` + `codesign --verify` + Team-ID verification, quarantine strip (`xattr -dr com.apple.quarantine`), atomic rename to `Updates/<version>/`, manifest state machine, and the "delete ≤ current staged version" cleanup rule.
- **T7:** `UpdateInstaller` — `relaunch-helper.sh` resource, swap-on-quit hook (App Translocation guard: quarantine already stripped at stage time, so the helper never runs from a shadow mount), swap-on-startup fallback (before engine launch), `.bak` rollback + "first launch OK" flag + failed-version blocklist, `/Applications`-not-writable graceful degradation. *Depends on T6.*
- **T8:** End-to-end manual verification on `~/Applications` and `/Applications` installs; failure-injection tests (kill mid-download, corrupt staged zip, non-launching build → rollback).
- **T-future (pipeline, not a v1 blocker):** Extend `boss-release.sh` to Developer-ID-sign + notarize + staple the `Boss.app` before zipping, so `Boss-1.0.N.zip` passes `spctl --assess`. When this lands, add a `spctl --assess` step to §3's integrity verification.
