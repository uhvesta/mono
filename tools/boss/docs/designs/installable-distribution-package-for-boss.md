# Boss: Installable Distribution Package

## Problem

Boss runs only on the machine it was built on. Getting it onto a new Mac today means cloning `mono`, installing the Zig + Metal + Bazel toolchains, running `tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh` to assemble `GhosttyKit.xcframework`, then `bazel build //tools/boss/app-macos:Boss` and `bazel build //tools/boss/engine:engine` and pointing the resulting `Boss.app` at the engine via `BOSS_ENGINE_CMD` — *and* arranging `boss`, `bossctl`, and `cube` on the user's PATH by hand (today that's via `repobin install`, which itself depends on a working bazel build of the repo).

That setup story is fine for `mono` developers. It is unviable for the immediate motivating use case: the user putting Boss on a work laptop where the only safe assumption is "macOS 15+, an Apple ID, and an internet connection." It is also unviable for sharing Boss with anyone else: there is no single artifact a recipient can run.

This doc proposes a **per-user signed and notarized macOS `.pkg`** as the v1 distribution format, the corresponding Bazel release pipeline that produces it, the on-disk layout the .pkg materialises, how the .app finds the engine and CLIs at runtime, how a single git SHA is stamped through every binary, and the uninstall path. The deliverable is one file the user downloads, double-clicks, and walks through three Installer screens — after which `Boss.app` is in `~/Applications`, `boss` / `bossctl` / `cube` are on PATH, and double-clicking the app launches the engine on first run with no manual `BOSS_ENGINE_CMD` or `bazel run`.

Three follow-up implementation chores will consume this design:

- A Bazel target that produces the unsigned `.pkg` payload from the four binaries + the `Boss.app` rule output.
- A `tools/boss/installer/release.sh` driver that signs every binary, signs the bundle, signs and notarizes the .pkg, and staples the ticket — driven by env-var-supplied certs and a `notarytool` API key.
- App-side changes so `EngineProcessController` discovers the engine inside `Boss.app/Contents/Resources/bin/engine` rather than shelling out to `bazel run`, plus a `boss uninstall` verb for the documented uninstall path.

## Goals

- A single artifact — `Boss-<version>+<short-sha>.pkg` — that, when run on a clean macOS 15 machine, installs Boss.app, the engine, and the three CLIs (`boss`, `bossctl`, `cube`) such that the user can immediately launch the .app or run any CLI from a fresh shell.
- **No `sudo` and no `/Applications` requirement.** Per-user install into `~/Applications` and `~/.local/bin`. The installer must work without admin rights.
- All five artifacts (`Boss.app`, `engine`, `boss`, `bossctl`, `cube`) are built from one git SHA in one Bazel invocation, and the SHA flows into each binary's `--version` output and the .app's `Info.plist`.
- The .app finds the engine and the CLIs by **bundle-relative path**, not by PATH search and not by `bazel run`. The shipped `Boss.app` is self-contained.
- Hardened runtime + Developer ID signing + notarization, so Gatekeeper opens the .pkg without "unidentified developer" friction.
- A documented uninstall path that removes binaries by default but leaves the user's state (`~/Library/Application Support/Boss/`) intact unless they opt in to purging it.
- The bazel rule for the .pkg is reproducible: given the same git SHA and the same signing identities, two invocations produce byte-identical payloads (and the surrounding signatures differ only by their timestamps).

## Non-goals

- **Windows and Linux.** macOS-only. The bazel rule should not pretend to be portable.
- **Auto-update / Sparkle / a release feed.** v1 is install-from-downloaded-artifact. Auto-update is a follow-on project that will consume this .pkg.
- **System-wide / `/Applications` / multi-user installs.** Per-user only. A future installer can re-target system locations, but the design here makes the per-user choice load-bearing because it removes the `sudo` step and the work-laptop "your admins forbid this" failure mode.
- **Vendoring third-party tools.** `jj`, `gh`, `bk`, `claude` (Claude Code CLI), and Xcode are *not* shipped inside the .pkg. The installer surfaces a setup checklist that names them, but does not install them. Bundling proprietary CLIs (`claude` is a binary distribution) and large open-source CLIs (`jj`, `gh`) blows out the artifact size and tangles us in their licensing.
- **Homebrew tap as the primary channel.** A tap is the right v2; it can wrap the .pkg, or shim around the same set of binaries, but the trust anchor for v1 is "user downloaded a signed .pkg from a URL I gave them."
- **Replacing `repobin install` for `mono` developers.** Developers who want to run Boss against a working copy continue to use `bazel run //tools/boss/app-macos:Boss` and `repobin install`. The .pkg is the *distribution* path; it does not displace the dev loop.
- **Engine state migration across versions.** v1 assumes the schema migrator in `boss-engine` is the authority for old → new state.db, and the installer does not touch state. Cross-version state migration belongs to the engine, not the installer.
- **Disk image cosmetics** (background art, custom volume icons, dragging-to-Applications animations). The .pkg's installer chrome is Apple's standard one; we do not invest in custom layouts in v1.

## Naming

- **Artifact**: `Boss-<short-sha>.pkg`, e.g. `Boss-abc1234.pkg`. We do not maintain a separate semver string in v1 — the short git SHA *is* the version. Once the product reaches a stable release cadence we can layer `vX.Y.Z+<sha>` semantics on top; until then, "the SHA is the version" is the simplest scheme that satisfies the goal "all binaries report the same version."
- **Bundle identifier**: `dev.spinyfin.bossmacapp` (unchanged from today's `Info.plist`).
- **Installer identifier**: `dev.spinyfin.boss.installer`. Distinct from the bundle id because the .pkg signature carries its own identifier.
- **Install root for binaries**: `~/Applications/Boss.app/` for the .app, with engine and CLIs *inside the bundle* at `Boss.app/Contents/Resources/bin/`. The `~/.local/bin/` directory holds three small shim symlinks pointing into the bundle (Q3).
- **State root**: `~/Library/Application Support/Boss/`, unchanged. The engine already resolves this in `config.rs:147` and `audit.rs:89`.
- **Bazel targets**:
  - `//tools/boss/installer:boss_pkg_payload` — the staged payload directory (a `pkg_filegroup` / `genrule` output).
  - `//tools/boss/installer:boss_pkg_unsigned` — the unsigned .pkg, built by `pkgbuild` from the payload.
  - `//tools/boss/installer:release` — a `sh_binary` driver that builds everything, signs it, notarizes it, and writes the final .pkg to `bazel-bin/tools/boss/installer/Boss-<sha>.pkg`. This is what humans run.
- **Release driver script**: `tools/boss/installer/release.sh`. Invoked by `bazel run //tools/boss/installer:release` so that bazel's runfiles tree is the source of binaries; never run directly.
- **Uninstall verb**: `boss uninstall` (with optional `--purge-state`). No separate `boss-uninstall` binary; piggybacks on the existing CLI surface so the user does not have to remember a one-shot tool name.

---

## Design Question 1 — Installer Format

### Options

- **(a) Signed/notarized `.pkg` (distribution-style, per-user domain).** Productbuild + pkgbuild produce a flat .pkg; `productsign` signs it with a Developer ID Installer cert; `notarytool` notarizes; `stapler` staples.
- **(b) Drag-to-Applications `.dmg`.** Single file; user drags `Boss.app` to a symlinked `Applications` shortcut. CLI installation is a separate post-launch step, typically a "first-run setup" in the app that copies CLIs out to `~/.local/bin`.
- **(c) Tarball + `install.sh`.** Smallest, most portable. User downloads `boss-<sha>.tar.gz`, runs `./install.sh`. Gatekeeper does not gate shell scripts the way it gates .apps, but it *does* refuse newly-downloaded executables, so the recipient still has to run `xattr -d com.apple.quarantine` or right-click → Open.
- **(d) Homebrew tap.** A `homebrew-boss` tap with a cask formula that downloads a pre-built tarball or .pkg. Beautiful UX for users who already use Homebrew; requires hosting infrastructure (a public tap repo, a release server, a checksums file per release).
- **(e) Mac App Store.** Discoverable, no Gatekeeper friction, free distribution. Sandbox forbids most of what Boss does (process spawning, libghostty's needs, arbitrary repo filesystem access). Pass.

### Discussion

(e) is a hard no. The App Store sandbox would gut every load-bearing capability of Boss — spawning `claude`, embedding `libghostty`, leasing cube workspaces on arbitrary repo paths, writing `~/Library/Application Support/Boss/state.db`. Hardened-runtime entitlements outside the App Store cover those; the sandbox does not.

(d) is the right *future* channel. The user already uses Homebrew at home, and "second machine? `brew install boss` from your tap" is the dream end-state. We reject it for v1 for one reason: a tap is a *delivery* mechanism, not a build artifact. The cask still has to point at a downloadable binary; the binary is what this design has to produce. Once the .pkg from (a) exists and is being produced reliably, a follow-up project can write a tap formula that downloads it.

(c) is tempting because it sidesteps `productbuild`. The blockers: (i) the user is on a fresh work laptop, where the first thing they do with a downloaded `.tar.gz` is wonder how to extract it; (ii) Gatekeeper quarantines the extracted binaries and the .app, and the recipient needs the right-click-Open dance for the first launch; (iii) we lose the standard Installer.app summary screen that lists what's about to be installed. v1 prioritises "user-cannot-fail" over "smallest possible artifact."

(b) is the classic Mac install pattern and is correct for an app with no CLIs. Boss is not that app. The CLIs are not optional ergonomics — the entire `boss` taxonomy surface lives there, and the user runs them from terminals all day. A .dmg that does not put the CLIs on PATH delegates a load-bearing step to the user; one that *does* needs a postinstall scriptlet, at which point we are reinventing .pkg with worse plumbing. The "first-run setup copies CLIs out" variant has a worse failure mode: if the user launches a CLI before launching the .app, the CLI does not exist, and the user gets `command not found` instead of an install confirmation. .pkg postinstall is atomic: by the time the Installer.app summary screen says "Done," every CLI is on PATH.

(a) is the right shape for a multi-component product on macOS. `productbuild --component` lets us declare the app bundle and the CLI binaries as one logical install, the postinstall scriptlet places per-user symlinks in `~/.local/bin`, and the signed/notarized .pkg gives the recipient the standard "this is from <Developer ID>" Gatekeeper experience. The cost is the build pipeline complexity, which we have to absorb regardless because the binaries must be signed and notarized for *any* distribution channel — a tap or a .dmg would face the same `codesign` + `notarytool` work.

### Decision

**(a) — per-user signed and notarized `.pkg`.** Domain `currentUserHomeDirectory`. Postinstall scriptlet creates `~/.local/bin/{boss,bossctl,cube}` symlinks pointing into the bundle. (d) is filed as a follow-up that consumes (a)'s output.

### What "per-user domain" buys us

`pkgbuild --install-location` of `~/Applications` and the distribution XML's `domains` element set to `enable_currentUserHome="true" enable_localSystem="false"` means Installer.app shows the "Install for me only" target by default and never asks for an admin password. On a managed work laptop where the user is not a local admin, this is the difference between "Boss installs" and "Boss does not install at all."

The trade-off is that the .pkg cannot drop files into `/usr/local/bin` (system-wide) or write outside `$HOME`. We take that trade — every Boss artifact already wants to live under `$HOME` (state.db is in `~/Library/Application Support/Boss/`, workspaces are in `~/Documents/dev/workspaces/`), so there is no file Boss wants to drop in a system location.

---

## Design Question 2 — Signing and Notarization Strategy

Three signing operations and one notarization, in this order:

1. **`codesign` each binary** with the Developer ID Application cert + hardened runtime + the entitlements the engine and the .app need.
2. **`codesign` the `Boss.app` bundle** (which transitively re-signs anything inside `Contents/MacOS/` and `Contents/Resources/`).
3. **`pkgbuild` + `productbuild` + `productsign`** with the Developer ID Installer cert.
4. **`notarytool submit --wait`** the signed .pkg, then `stapler staple`.

### Certs

Two distinct Apple Developer certs, both issued by Apple's CA:

- **Developer ID Application** — signs the .app and every Mach-O inside it (the engine + the CLIs).
- **Developer ID Installer** — signs the .pkg itself.

These are different certs with different purposes; productsign refuses an Application cert and codesign refuses an Installer cert. Both live in the developer's login keychain on whatever machine drives the release; CI is a future concern.

The release driver script reads the cert identities from env vars (`BOSS_SIGN_APPLICATION_IDENTITY`, `BOSS_SIGN_INSTALLER_IDENTITY`) — it does not hard-code names, because the user's cert names will differ between home (personal Developer ID) and work (a corporate-issued cert if it comes to that).

### Entitlements

The .app and the engine both need the hardened runtime (notarization mandates it). The minimum entitlements set we have to ship:

- `com.apple.security.cs.disable-library-validation` on the .app and the engine — required because `libghostty` loads `GhosttyKit.xcframework` Mach-Os that are signed with the developer's cert, not Apple's, and the default hardened runtime refuses non-Apple-signed libraries.
- `com.apple.security.cs.allow-jit` on the .app — Ghostty's renderer uses Metal shader compilation paths that may require this; needs validation against an actual `xcrun notarytool submit` run before we commit (Q-Risk-1).
- `com.apple.security.cs.allow-unsigned-executable-memory` — same risk-list reason as `allow-jit`; we add it only if notarization rejects the bundle without it.
- The engine does **not** need `allow-jit` or `allow-unsigned-executable-memory` — it's a plain Rust binary with no JIT and no foreign dylibs.

These go in two `Boss.entitlements` plist files (one for the .app, one for the engine), checked in at `tools/boss/installer/entitlements/`. Keeping them as files (not inline in BUILD.bazel) means a reviewer can read what we're asking Apple to trust us with without parsing Starlark.

### Notarization

`xcrun notarytool submit --wait` against an App Store Connect API key. The API key (a `.p8` file plus key id + issuer id) is read by `release.sh` from env vars (`BOSS_NOTARY_API_KEY_FILE`, `BOSS_NOTARY_API_KEY_ID`, `BOSS_NOTARY_API_KEY_ISSUER`). The `--wait` flag turns the script synchronous; if Apple's notary service rejects the submission, we get the JSON log inline and bail.

`xcrun stapler staple` is the final step: it embeds the notarization ticket into the .pkg so the recipient can verify offline. Without stapling, a recipient on an airplane sees a Gatekeeper hang while it tries to reach Apple.

### Order matters

Notarization examines the *contents* of the .pkg, not just the .pkg envelope. Every Mach-O inside (engine, three CLIs, the .app's main binary, every dylib in `GhosttyKit.xcframework`) must already be `codesign`'d with the hardened runtime *before* the .pkg is built. If any one is not, notarization rejects the whole bundle. The release driver enforces this with a "verify everything" pass between step 2 and step 3:

```
codesign --verify --strict --deep --verbose=2 <each Mach-O>
spctl --assess --verbose <Boss.app>
```

Fail-fast on either check.

---

## Design Question 3 — On-Disk Layout and How the App Finds Things

### The layout

```
~/Applications/Boss.app/
  Contents/
    Info.plist                 (CFBundleShortVersionString stamped with <sha>)
    MacOS/
      Boss                     (the SwiftUI binary)
    Resources/
      bin/
        engine                 (the Rust engine binary)
        boss                   (the user CLI)
        bossctl                (the coordinator CLI)
        cube                   (workspace manager)
        boss-event             (the hook shim the engine resolves into the worker's PATH)
      Frameworks/
        GhosttyKit.framework   (built by bootstrap-ghosttykit.sh, copied in)
      Boss.entitlements        (informational; for `codesign --display`)
    PkgInfo

~/.local/bin/
  boss        -> /Users/<user>/Applications/Boss.app/Contents/Resources/bin/boss
  bossctl     -> /Users/<user>/Applications/Boss.app/Contents/Resources/bin/bossctl
  cube        -> /Users/<user>/Applications/Boss.app/Contents/Resources/bin/cube

~/Library/Application Support/Boss/
  state.db                     (created by engine on first run; not touched by installer)
  engine-audit.log
  events.sock                  (runtime)
  dispatch-events              (runtime, deleted on stop)
  executions/
```

### Why CLIs live inside the bundle, not in `~/.local/bin` directly

Two reasons.

First, **uninstall.** With the CLI binaries inside the bundle, removing the .app removes the binaries; the `~/.local/bin` symlinks dangle, and the uninstall script removes those dangling symlinks. If the CLIs were *copies* in `~/.local/bin`, removing the .app would leave stale binaries running against a now-gone engine path, and the uninstall script would have to know to delete them.

Second, **version coherence.** The whole point of building everything in one Bazel invocation from one git SHA is that the four binaries are byte-coherent. If the user upgrades by re-running a new .pkg, the old .app's bundle is replaced atomically; the new CLIs are immediately the new ones because the symlinks point into the new bundle. There is no window where the user has a new app and old CLIs.

The symlinks themselves are unconditional — the postinstall scriptlet `rm -f` them first, then `ln -s` afresh. That handles the "user previously installed Boss" case cleanly.

### How the .app finds the engine

Today, `EngineProcessController.swift:42` runs `bazel run //tools/boss/engine:engine -- --socket-path <path>` from `BUILD_WORKSPACE_DIRECTORY`. That has to go.

After this change, `EngineProcessController` resolves the engine binary in this order:

1. **`BOSS_ENGINE_CMD` env override** — unchanged; still wins so a developer running the in-source app against a custom engine works. (`bazel run //tools/boss/app-macos:Boss` continues to use this.)
2. **Bundle-relative path** — `[Bundle.main.resourcePath]/bin/engine`. This is the path the installed app uses. Inside the bundle, `Bundle.main.resourcePath` is `<Boss.app>/Contents/Resources`, so the engine lives at a fixed offset from the .app's main binary. No PATH search, no env var required.
3. **(Fallback for dev builds) `bazel run //tools/boss/engine:engine`** — what we have today, kept for the case where someone runs the .app out of `bazel-bin/` and the bundle is the unsigned dev bundle.

The same logic applies to the existing `boss-event` resolver in `engine/src/runner.rs::resolve_boss_event_binary`. Resolution order today already has a "sibling of engine_path" check (rule #4 at line 194). When the engine binary lives at `Boss.app/Contents/Resources/bin/engine`, its sibling at `Boss.app/Contents/Resources/bin/boss-event` is exactly where the shim ships, and rule #4 finds it without a new code path. Free wins are nice.

### How the worker pane finds `claude`, `jj`, `gh`, `cube`

This part does *not* change. Workers still see whatever `PATH` the parent shell exports. The installer's postinstall scriptlet emits a "setup checklist" note (Q7) that names the third-party tools the user must install themselves. Boss does not vendor them and does not modify `~/.zshrc`.

The one CLI Boss ships that workers need on their PATH is `cube` (workers call `cube workspace lease` / `release`). The `~/.local/bin/cube` symlink the installer creates handles this — `~/.local/bin` is on most users' PATH already, and if it is not, the setup checklist tells them so (Q7).

### Why `~/.local/bin` (not `/usr/local/bin`, not `~/bin`, not a custom dir)

Three constraints converge:

- The .pkg cannot write to `/usr/local/bin` in the per-user domain.
- `~/bin` is on many but not all macOS users' PATH by default; `~/.local/bin` is the modern XDG-flavoured convention and is what `pipx`, `cargo install`, `pnpm setup`, etc. use.
- The user's CLAUDE.md workspace rules already reference `~/.local/bin` ("Per-user install in `~/Applications` and `~/.local/bin` (or wherever design lands) is fine"). Picking it matches the user's prior intent.

If `~/.local/bin` is not on PATH, the checklist tells the user the one line to add to `~/.zshrc`. We do not mutate dotfiles.

---

## Design Question 4 — Build Pipeline

The release pipeline has four phases. Each runs from a clean Bazel tree with a single resolved git SHA.

### Phase 1 — Bazel produces the binaries

A new package `//tools/boss/installer/` with these targets:

```python
# tools/boss/installer/BUILD.bazel
load("@build_bazel_rules_apple//apple:macos.bzl", "macos_application")
load(":pkg.bzl", "boss_pkg")

# The set of binaries that must end up inside Boss.app/Contents/Resources/bin/.
filegroup(
    name = "bundled_binaries",
    srcs = [
        "//tools/boss/engine:engine",
        "//tools/boss/cli:boss",
        "//tools/boss/bossctl:bossctl",
        "//tools/cube:cube",
        "//tools/boss/event-shim:boss-event",
    ],
    visibility = ["//tools/boss/installer:__pkg__"],
)

# Repack the existing Boss.app target so the binaries above are
# copied into Contents/Resources/bin/. The macos_application rule
# accepts `additional_contents` for exactly this.
macos_application(
    name = "Boss.app",
    bundle_id = "dev.spinyfin.bossmacapp",
    infoplists = ["//tools/boss/app-macos:Info.plist"],
    minimum_os_version = "15.0",
    deps = ["//tools/boss/app-macos:boss_mac_app_lib"],
    additional_contents = {
        ":bundled_binaries": "Resources/bin",
        "//tools/boss/app-macos:ghosttykit_framework": "Frameworks",
    },
    entitlements = "entitlements/app.entitlements",
    visibility = ["//tools/boss/installer:__pkg__"],
)

# A genrule that runs pkgbuild against the Boss.app + a small
# postinstall scripts directory; produces an *unsigned* .pkg.
boss_pkg(
    name = "boss_pkg_unsigned",
    app = ":Boss.app",
    scripts = "scripts/",   # contains postinstall + preinstall
    visibility = ["//tools/boss/installer:__pkg__"],
)

# Driver for humans. Builds boss_pkg_unsigned, then runs release.sh
# which does codesign + productsign + notarytool.
sh_binary(
    name = "release",
    srcs = ["release.sh"],
    data = [":boss_pkg_unsigned", ":Boss.app", ":bundled_binaries"],
    visibility = ["//visibility:public"],
)
```

The `boss_pkg` macro is a thin wrapper around a `genrule` that invokes `pkgbuild --root <staging> --identifier dev.spinyfin.boss.installer --install-location <dest> --scripts <scripts> <output>`. We do not pull in a third-party Bazel pkg ruleset because the rule we need is one shell-out; vendoring `rules_pkg` for one genrule is overkill.

### Phase 2 — Workspace status threads the git SHA in

Bazel's `--workspace_status_command` is what we use to propagate `git rev-parse --short HEAD` into every binary that wants to know its build SHA. Today this is unwired (`engine/src/build_info.rs` documents that `BOSS_ENGINE_GIT_SHA` is opt-in and the canonical engine path returns `"unknown"`).

We add `tools/boss/installer/workspace-status.sh`:

```sh
#!/usr/bin/env bash
set -euo pipefail
SHA=$(git rev-parse --short HEAD)
BUILD_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)
echo "STABLE_BOSS_GIT_SHA $SHA"
echo "STABLE_BOSS_BUILD_TIME $BUILD_TIME"
```

…and wire it in `.bazelrc` for the release config:

```
build:release --workspace_status_command=tools/boss/installer/workspace-status.sh
build:release --stamp
```

`rules_rust`'s `rust_binary` reads stamping via `--@rules_rust//rust/settings:rustc_version` and `cargo_build_script`-style env injection. We do not need that; we instead pass the values into the binaries via a tiny `cc_library` / `genrule` that emits a `build_info.rs` file with the stamped constants, depended on by `engine_lib`, `boss`, `bossctl`, and `cube`. Each binary's `--version` prints `boss-engine 0+abc1234 built 2026-05-12T11:14:02Z` (or the equivalent for its name).

For the .app's `Info.plist`, the same SHA is substituted into `CFBundleShortVersionString` and `CFBundleVersion` via the existing `infoplists` mechanism in `macos_application`. `rules_apple` supports `${BAZEL_STAMP}` substitution; we use it.

### Phase 3 — Release driver signs and notarizes

`tools/boss/installer/release.sh` runs after `bazel build --config=release //tools/boss/installer:boss_pkg_unsigned` and:

1. Stages the .app and the binaries into a writable scratch dir (bazel outputs are read-only).
2. `codesign --options runtime --entitlements <e>.entitlements -s "$BOSS_SIGN_APPLICATION_IDENTITY"` every Mach-O.
3. `codesign --options runtime --entitlements app.entitlements -s "$BOSS_SIGN_APPLICATION_IDENTITY" Boss.app`.
4. `codesign --verify --strict --deep` and `spctl --assess` to fail-fast on any signing mistake.
5. `pkgbuild` to produce the payload .pkg from the signed bundle; `productbuild --distribution distribution.xml` to wrap it; `productsign -s "$BOSS_SIGN_INSTALLER_IDENTITY"` to sign the wrapper.
6. `xcrun notarytool submit --wait --key "$BOSS_NOTARY_API_KEY_FILE" --key-id "$BOSS_NOTARY_API_KEY_ID" --issuer "$BOSS_NOTARY_API_KEY_ISSUER" Boss-<sha>.pkg`. Fail if the response isn't `Accepted`.
7. `xcrun stapler staple Boss-<sha>.pkg`.
8. Print the final path and `shasum -a 256` it.

The script is idempotent: re-running with the same SHA produces the same payload digest; only the signature timestamps differ.

### Phase 4 — Publish (out of scope here)

v1 publishes by hand: the user uploads `Boss-<sha>.pkg` somewhere (private GitHub release, internal share, dropbox link) and shares the URL. The publish channel is intentionally not in this design — it's a downstream concern. The acceptance criterion for the build pipeline is "the .pkg exists on disk, notarized and stapled."

### Why not productbuild without pkgbuild?

`productbuild --component <bundle>` *can* take a .app directly and skip the pkgbuild step. We use pkgbuild + productbuild because the productbuild path can't directly express the postinstall scriptlet for the `~/.local/bin` symlinks; pkgbuild can. The two-step is annoying but well-trodden — every Apple sample for "app + CLI + scriptlet" uses it.

---

## Design Question 5 — First-Run Bootstrap

### What "first run" means

The user has just installed the .pkg. Three states need to be initialised on first launch:

- **`~/Library/Application Support/Boss/`** — the directory plus its children.
- **`state.db`** — the engine's SQLite database.
- **The engine itself** — needs to be running before the .app can talk to it.

### Today's behaviour, which already mostly handles this

`engine/src/config.rs:147` resolves `state.db` lazily — if the file does not exist, the engine creates the parent directory and the DB at startup; the schema migrator builds the tables. `audit.rs:89` does the same for the audit log. So the *engine* already handles the on-disk bootstrap end of first-run: nothing the installer needs to do.

The .app side is currently broken-by-design for a fresh install: `EngineProcessController.swift:42` falls through to `bazel run`, which has no meaning on a fresh laptop. The bundle-relative engine resolution from Q3 fixes this — on first launch, the .app sees the engine at `Boss.app/Contents/Resources/bin/engine`, spawns it, the engine creates state.db, and we're up.

### What we deliberately do *not* do

- **No default product or project.** A fresh Boss has zero work items; the user creates their first product with `boss product create`. Auto-seeding "your first product is called `default`" creates the worse user experience — the user thinks `default` is a magic name and pollutes their kanban.
- **No first-run "welcome" wizard.** v1 launches into the standard Work-mode UI with an empty kanban and a hint in the empty-state ("no products yet — `boss product create` to start, or visit the Setup Checklist"). The Setup Checklist link is the only special first-run UI affordance.
- **No state migration from another machine.** A user moving from one Mac to another with their full Boss history is a separate project; v1 says "second machine starts empty, populate it via your usual product/project creation."

### Detecting an existing install

The .pkg preinstall script runs `[ -e ~/Applications/Boss.app ]` and `[ -e ~/Library/Application\ Support/Boss/state.db ]`. The first triggers a "this looks like a re-install; replacing the bundle." The second is *ignored* — re-installing must never wipe state. The postinstall scriptlet only ever creates symlinks and stamps a setup-completed marker; it does not initialise the state directory.

### Setup checklist

The .pkg's postinstall scriptlet writes a one-page text file to the .app's bundle:

```
~/Applications/Boss.app/Contents/Resources/SETUP.txt
```

…and the .app shows its contents in a "Setup" tab the first time it launches without a state.db present. The checklist names:

- **Required tools**: `jj`, `gh`, `claude`, Xcode command-line tools (for `xcrun notarytool`-adjacent things in the worker workflow), the user's preferred shell PATH knob if `~/.local/bin` is not on PATH.
- **Required env vars**: `ANTHROPIC_API_KEY` for pane summaries (read by the engine).
- **Optional**: `bk` for Buildkite querying, `repobin install` for in-source dev work.

The checklist is informational. Boss does not block on it.

---

## Design Question 6 — Uninstall

### The supported uninstall path

`boss uninstall` is the supported way to remove Boss. It does, in order:

1. Asks the user to confirm (prints what it will delete, requires `y/N`).
2. Stops the engine if running (`kill <pid>` from `/tmp/boss-engine.pid`).
3. Removes the `~/.local/bin/{boss,bossctl,cube}` symlinks.
4. Removes the `~/Applications/Boss.app` bundle.
5. **If `--purge-state` was passed**: removes `~/Library/Application Support/Boss/`. Otherwise, leaves it.
6. Prints a one-line summary.

`boss uninstall` is the same binary that's about to delete itself. The flow handles this by `exec`'ing a small `rm -rf` shell script that runs after the parent process exits — same trick `pyenv uninstall` and `nvm uninstall` use. The script lives at `~/.cache/boss-uninstall.sh` for the duration of the uninstall, then deletes itself.

### Why not Installer.app's "uninstall package"?

There is no such feature in Installer.app. macOS .pkg installers are intentionally one-way; the user is expected to use the app's own uninstaller or a manual `rm -rf` of the install root. We pick the former because the manual route requires the user to remember exactly which directories were touched.

### The fallback manual route

For users who lost the `boss` CLI (e.g. they ran `rm -rf ~/Applications/Boss.app` first), `SETUP.txt` includes a "Manual uninstall" section listing the exact paths to remove. There is no second-level magic here — Boss only writes to three places, and the checklist names all three.

### What state purge means

`--purge-state` removes:

- `~/Library/Application Support/Boss/state.db`
- `~/Library/Application Support/Boss/engine-audit.log`
- `~/Library/Application Support/Boss/executions/`
- `~/Library/Application Support/Boss/events.sock` (if present — typically removed on engine stop)
- `~/Library/Application Support/Boss/dispatch-events` (runtime artefact)

It does **not** touch `~/Documents/dev/workspaces/`. Those are cube's territory and they hold genuine user work (uncommitted edits, jj working copies). The uninstall script names this caveat in its summary so a user who wants to also clean up workspaces knows where to look.

### Re-install semantics

A user can re-install Boss after uninstalling without `--purge-state`, and their kanban / executions / history are intact. This is the "I'm trying a new version of Boss, I don't want to lose my work" path. State purge is opt-in precisely because the failure mode of accidentally purging state (lost history) is much worse than the failure mode of accidentally keeping stale state (the user has to opt in to a second `boss uninstall --purge-state`).

---

## Design Question 7 — Version Stamping

### One git SHA, four binaries, one .app, one .pkg

The contract: the SHA reported by `engine --version`, `boss --version`, `bossctl --version`, `cube --version`, the `CFBundleShortVersionString` in `Boss.app/Contents/Info.plist`, and the filename of the .pkg are all the *same* short SHA.

### How the SHA flows

1. **`tools/boss/installer/workspace-status.sh`** runs at the start of every `bazel build --config=release` and emits `STABLE_BOSS_GIT_SHA <sha>` to bazel's stamping mechanism.
2. **A `genrule`** at `//tools/boss/installer:build_info_rs` consumes `bazel-out/stable-status.txt` and produces a Rust source file with `pub const BOSS_GIT_SHA: &str = "<sha>";` etc. All four binaries depend on this genrule and link the generated code in. The existing `engine/src/build_info.rs` switches from `option_env!("BOSS_ENGINE_GIT_SHA")` to reading the linked-in constant.
3. **The .app's `Info.plist`** receives the SHA via `rules_apple`'s `${BAZEL_STAMP}` substitution: `CFBundleShortVersionString = ${BAZEL_STAMP_BOSS_GIT_SHA}`, `CFBundleVersion = ${BAZEL_STAMP_BOSS_GIT_SHA}`.
4. **The .pkg filename** is `Boss-${BOSS_GIT_SHA}.pkg`. The release driver script reads the SHA from the stable-status file before invoking `productbuild`.

### `--version` shape

Every CLI prints, in response to `--version`:

```
boss 0+abc1234 built 2026-05-12T11:14:02Z
```

Format: `<name> <semver>+<short-sha> built <iso8601>`. The leading `0` is the placeholder major version — until we cut a real v1.0 release with a versioning policy, every artifact is "0+<sha>". This is intentional: telegraphing "you have a git sha, not a semantic version" is more honest than pretending we have stable v0.x.y semantics.

The engine's existing `build_info::git_sha()`, `build_time()`, `binary_fingerprint()`, `process_started_at()` continue to work; they switch their compile-time `option_env!` reads to the linked-in constants from the genrule. The runtime fingerprint stays in place — it's still the canonical "is this the binary I think it is" signal independent of build-time stamping.

### What about a future real semver?

The day we want to call something "v1.0," we drop a `VERSION` file at the repo root, the workspace-status script reads it, the constants pick up `v1.0+abc1234`, and the .pkg becomes `Boss-v1.0+abc1234.pkg`. No design change needed; the format already accommodates it.

---

## Open questions

### Q-Risk-1 — Notarization may refuse GhosttyKit without permissive entitlements

`libghostty` is a Zig-built dynamic library that does its own GPU shader compilation via Metal. Apple's hardened-runtime defaults forbid runtime code generation and runtime-loaded executable memory. Whether notarization accepts the bundle with just `disable-library-validation`, or whether we additionally need `allow-jit` and/or `allow-unsigned-executable-memory`, can only be answered by an actual `notarytool submit` against an actual built bundle.

**Resolution plan**: the implementation chore for `release.sh` runs the entire pipeline against a test Apple ID first (the user's personal Developer ID), iterating on the entitlements set until notarization returns `Accepted`. The final entitlements files are committed; the design accepts that the entitlements set may grow by one or two keys.

### Q-Risk-2 — `~/.local/bin` PATH situation on macOS is heterogeneous

Some users have `~/.local/bin` on PATH (via Homebrew or manual `.zshrc` edits). Many do not. The setup checklist names the exact `.zshrc` line, but it does not run it. Mutating dotfiles from an installer is a one-way bug magnet (no clean uninstall, sets user expectations that we manage their shell, etc.).

A v1.x option is to detect "user does not have `~/.local/bin` on PATH" in the postinstall scriptlet and write a `~/.zshenv` file with a single `export PATH=$HOME/.local/bin:$PATH` line. `~/.zshenv` is loaded for *every* zsh invocation including non-interactive ones, which is exactly what we want for `cube workspace lease` from inside a libghostty subshell. The risk: the user already has a `~/.zshenv` and we don't want to clobber it. Workable resolution: write `~/.zshenv` only if it does not exist; otherwise leave a note in the checklist.

This is a "ship v1 with the manual-PATH-fix advice in the checklist, watch for friction, then ship the optional `~/.zshenv` write in v1.1" decision. Not gating on v1.

### Q-Risk-3 — Bazel + the Apple toolchain pin

`tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh` requires Xcode's Metal Toolchain component (it bails with a `xcodebuild -downloadComponent MetalToolchain` hint if missing). The release driver inherits this requirement. CI that runs this pipeline must have the Metal Toolchain installed; a stale `DEVELOPER_DIR` will cause bazel to fail in confusing ways.

The user's CLAUDE.md already names the remedy (`bazel clean --expunge` for Apple toolchain pin mismatches), so the maintenance story for this design is "if you see Xcode/CC toolchain weirdness in release, expunge bazel state before debugging anything else." No new design risk; just inheriting the existing one.

### Q-Risk-4 — Notary API credentials live somewhere

The release driver expects `BOSS_NOTARY_API_KEY_FILE` to be a real `.p8` file on disk. For the user's first release, that's their own laptop with the file in `~/.private/`. For an eventual CI release, the key needs to be in a secret store — this is a real cost we accept later. v1 acknowledges that releases come from one (the user's) laptop.

### Q-Risk-5 — Engine `--version` does not exist yet

The engine binary today does not honour a `--version` flag (the existing `build_info` module is exposed via the live-status debug RPC, not a CLI flag). The implementation chore for "thread the SHA through" must add the flag to the engine main + each CLI's argparser. Trivial work; called out so a reviewer doesn't read "every binary has --version" and assume it's already true.

### Q-Open-1 — Versioning when there is no clean tag

If a user runs `bazel run //tools/boss/installer:release` from a dirty working copy, the SHA is the parent commit's SHA but the binaries reflect uncommitted changes. The workspace-status script could detect dirtiness (`git status --porcelain`) and append `-dirty`, producing `Boss-abc1234-dirty.pkg`. Recommend: yes, do that; refuse to notarize a dirty build (Apple notarization succeeds either way, but we should be opinionated). Final call: dirty builds produce a `-dirty` artifact and the release script logs `WARN: skipping notarization for dirty build`. The artifact is still installable; the user has been told.

### Q-Open-2 — Whether to ship a Distribution.xml welcome / readme

`productbuild --distribution` accepts a `distribution.xml` that can declare `welcome`, `readme`, `license`, and `conclusion` HTML/text files shown during the install. v1 ships none: the Installer.app default chrome is sufficient. v1.1 might add a `conclusion.txt` ("Boss is installed. Open the Setup tab in the app for the checklist of third-party tools you'll need."). Not gating on v1.

### Q-Open-3 — Building the .pkg on Linux CI

The notarization step requires `xcrun notarytool`, which is macOS-only. The `pkgbuild` / `productbuild` steps similarly. v1 accepts that releases run on macOS. A future CI move to Linux runners is blocked by Apple's tooling and is out of scope.

---

## Follow-up implementation chores

This design unblocks three implementation chores, filed against this project after the design lands:

1. **Bazel installer rule + payload assembly.** New package `tools/boss/installer/` with the `boss_pkg_unsigned` rule, the `bundled_binaries` filegroup, the `additional_contents` change to the existing `macos_application`, and `workspace-status.sh` + stamping wiring. Output: `bazel build //tools/boss/installer:boss_pkg_unsigned` produces an unsigned .pkg.

2. **Signing + notarization driver.** `tools/boss/installer/release.sh` with the codesign / productsign / notarytool / stapler dance. Output: `bazel run //tools/boss/installer:release` produces a signed and stapled `Boss-<sha>.pkg`.

3. **App + engine resolution path.** Modify `EngineProcessController.swift` to prefer `Bundle.main.resourcePath/bin/engine` over `bazel run`; thread the stamped git SHA into each CLI's `--version` output; add the `boss uninstall` verb. The `Info.plist` stamping for `CFBundleShortVersionString` lands here.

The chores are independent enough to run in parallel after the design is approved, but chore 1 produces the artifact that chore 2 signs; chore 3 is fully independent. Recommended order: 3 first (smallest blast radius, validates the bundle-relative resolution under `bazel run`), 1 second, 2 last.
