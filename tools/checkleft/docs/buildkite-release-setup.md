# Buildkite: checkleft release setup

This document is the operator checklist for the **checkleft release pipeline** — the Buildkite pipeline that builds prebuilt `checkleft` binaries for Linux and macOS and publishes them as assets on a GitHub Release of `spinyfin/mono`. External repos consume these prebuilts instead of building checkleft from source.

It is modeled on the Boss release pipeline; for that reference see [`../../boss/docs/buildkite-release-setup.md`](../../boss/docs/buildkite-release-setup.md). The two differ in one important way: Boss releases run as a *step inside the existing `mono` pipeline*, while checkleft releases run as a **separate pipeline** with its own cron schedule and its own manual trigger. **Creating the in-repo pipeline file is not enough — the pipeline must be registered in Buildkite using the steps below.**

- Pipeline definition: [`../../../.buildkite/pipeline-checkleft-release.yml`](../../../.buildkite/pipeline-checkleft-release.yml)
- Release script: [`../../../.buildkite/steps/checkleft-release.sh`](../../../.buildkite/steps/checkleft-release.sh)
- Version source of truth: [`../Cargo.toml`](../Cargo.toml) (`version = "0.1.0-alpha.N"`)

---

## How releases are triggered

| Trigger | When | Change-detection |
|---|---|---|
| Buildkite cron schedule | e.g. daily | Skips if nothing under checkleft changed since the last `checkleft-v*` tag |
| Manual build (`bk build create`, BK UI **New Build**, or REST API) | On demand | Always releases (skips change-detection) |

The pipeline should **not** be wired to build on push. The version-bump commit it creates carries `[skip ci]`, and an idempotency guard no-ops any run whose `HEAD` is already the latest release commit — but the cleanest configuration is push-builds disabled, schedule + manual only.

The org slug is `flunge`; the GitHub repo is `spinyfin/mono`. (Boss's release build URLs look like `https://buildkite.com/flunge/mono/builds/N`.)

---

## One-time registration

All `bk` commands below assume the Buildkite CLI is authenticated. Verify with:

```sh
bk whoami
bk use flunge          # select the org these pipelines live in
```

### 1. Find the cluster the mono pipelines use

New pipelines must be created in the same cluster as the existing `mono` pipeline so they schedule onto the same agent fleet (the `bazel-any` and `macos-arm64` queues).

```sh
bk cluster list
```

Note the cluster name (or ID). It is passed as `-c` below and is also needed for the secret step.

### 2. Create the pipeline

```sh
bk pipeline create "mono-checkleft-release" \
  --description "Release pipeline for the checkleft prebuilt binaries" \
  --repository "git@github.com:spinyfin/mono.git" \
  --cluster-id "<cluster-name-or-id>"
```

This creates the pipeline and connects it to the GitHub repo (which provisions the webhook). Confirm with `bk pipeline view mono-checkleft-release`.

### 3. Point the pipeline at the release YAML

`bk pipeline create` does not upload the steps; like every pipeline in this repo, the registered pipeline must run a single bootstrap step that uploads the in-repo definition. Set the pipeline's **Steps** (Buildkite UI → Pipeline → **Settings** → **Steps**, or via the REST API) to exactly:

```yaml
steps:
  - label: ":pipeline: upload"
    command: "buildkite-agent pipeline upload .buildkite/pipeline-checkleft-release.yml"
```

(The default pipeline-upload command reads `.buildkite/pipeline.yml`; the explicit path is what makes this pipeline use the checkleft definition.)

To do it via the API instead of the UI:

```sh
bk api -X PATCH "organizations/flunge/pipelines/mono-checkleft-release" \
  -F 'configuration=steps:
  - command: "buildkite-agent pipeline upload .buildkite/pipeline-checkleft-release.yml"'
```

### 4. Disable push-triggered builds

In Pipeline **Settings** → **GitHub**, turn **off** "Trigger builds when branches are pushed" (and any PR triggers). Releases come only from the cron schedule and manual builds. This — together with the `[skip ci]` bump commit and the idempotency guard — prevents the version-bump commit from triggering the pipeline in a loop.

### 5. Create the cron schedule

In Pipeline **Settings** → **Schedules** → **New Schedule**:

- **Description:** `checkleft release check`
- **Cron interval:** `0 7 * * *` (daily 07:00 UTC — adjust to taste)
- **Branch:** `main`
- **Message:** `Scheduled checkleft release check`
- **Commit:** `HEAD`

If a scheduled run finds no checkleft-affecting changes since the last `checkleft-v*` tag, the build logs `release skipped: ...` and exits 0 without cutting a release.

### 6. Provision the release token

The pipeline pushes the version-bump commit + tag and creates the GitHub Release. The `bazel-any` queue mixes Mac (personal-key write) and Linux (read-only deploy-key) agents, so the agent's ambient git credentials are unreliable. The script therefore pushes over **HTTPS with a token in an `Authorization` header**, which works on any agent. The same token authenticates `gh`.

Provide it as the `CHECKLEFT_RELEASE_GH_TOKEN` secret (the script also accepts `GH_TOKEN` / `GITHUB_TOKEN`):

```sh
bk secret create \
  --cluster-id "<cluster-name-or-id>" \
  --key CHECKLEFT_RELEASE_GH_TOKEN \
  --description "checkleft release: push tag/commit + create GitHub Release" \
  --value "<token>"
```

**Required token scopes / permissions:**

- **Contents: write** on `spinyfin/mono` — push the bump commit and tag, create the release, upload assets.
- **Push to `main`** — the bump commit is pushed directly to `main`. If `main` is a protected branch, the token's identity must be allowed to bypass branch protection. Use a GitHub App installation token (App added to the repo's branch-protection bypass list) or a fine-grained PAT owned by an account with that bypass. A classic PAT with `repo` scope works only if the account can push to protected `main`.

### 7. (If musl is wanted) ensure the Linux agents have musl tooling

The static `x86_64-unknown-linux-musl` asset is **best-effort**: the script builds it with `cargo` and skips it (with a warning, not a failure) if the toolchain is missing. To enable it, the Linux agents need `rustup target add x86_64-unknown-linux-musl` to succeed and a musl C toolchain (`musl-tools` / `musl-gcc`) on `PATH` for the tree-sitter C deps. If you do not need a static binary, you can leave this unprovisioned.

---

## Triggering a release manually

```sh
bk build create \
  --pipeline mono-checkleft-release \
  --branch main \
  --message "Manual checkleft release"
```

Because `BUILDKITE_SOURCE` is `api`/`ui`, change-detection is skipped and a release is always cut. The BK UI **New Build** button does the same.

---

## Verifying the setup

1. Trigger a manual build (above) and open the build URL.
2. The **checkleft-release (linux)** step should compute the next version, build the Linux binaries, push the tag, create the release, and upload assets.
3. The **checkleft-release (darwin)** step should attach the macOS binaries.
4. Confirm the release and its assets:

```sh
gh release view checkleft-v0.1.0-alpha.9 --repo spinyfin/mono
```

Expected assets (named by Rust target triple, each with a `.sha256` sidecar):

- `checkleft-x86_64-unknown-linux-gnu`
- `checkleft-x86_64-unknown-linux-musl` (if musl tooling is present)
- `checkleft-aarch64-apple-darwin`
- `checkleft-x86_64-apple-darwin`

---

## Recovering from a partial release

The Linux phase is atomic: binaries are built **before** anything is pushed, so a Linux build failure leaves no tag, commit, or release behind. The macOS phase runs after the release already exists, so a darwin failure leaves a release with Linux assets only. To recover:

- **Re-run the darwin job in the same build** (`bk job retry <job-id>`) — the tag is read from build meta-data, so it picks up where it left off.
- **Or upload manually** from a Mac checked out at the tag:

  ```sh
  CHECKLEFT_RELEASE_TAG=checkleft-v0.1.0-alpha.9 \
    .buildkite/steps/checkleft-release.sh darwin
  ```

A brand-new build will **not** redo a skipped darwin upload: the idempotency guard sees `HEAD` already at the release commit and no-ops. Use the job retry or the manual override above.

---

## How it works (summary)

- **Version:** only the `-alpha.N` counter is revved (the base `X.Y.Z` is carried through). The next N is `max(Cargo.toml alpha, highest published checkleft-v* alpha) + 1`, so a stale Cargo.toml can never reuse a published alpha. The bump is committed to `tools/checkleft/Cargo.toml` + `Cargo.lock` and the commit is tagged `checkleft-vX.Y.Z-alpha.N`.
- **Build tool:** native binaries are built with `bazel build -c opt //tools/checkleft:checkleft` (matches how mono builds checkleft and reuses the CI disk cache); the cross targets (`x86_64-apple-darwin`, `x86_64-unknown-linux-musl`) are built with `cargo --target`, since those triples are not registered in mono's bazel toolchains. checkleft's CLI does not embed `CARGO_PKG_VERSION`, so all binaries are byte-identical regardless of the version string — both phases build at `BUILDKITE_COMMIT` and the version-bump commit never has to be built.
- **Loop prevention:** the bump commit carries `[skip ci]`; push-triggered builds are disabled; and the idempotency guard no-ops any run whose `HEAD` is already the latest release commit.

---

## Related

- [`../../../.buildkite/pipeline-checkleft-release.yml`](../../../.buildkite/pipeline-checkleft-release.yml) — pipeline definition
- [`../../../.buildkite/steps/checkleft-release.sh`](../../../.buildkite/steps/checkleft-release.sh) — release script
- [`../../boss/docs/buildkite-release-setup.md`](../../boss/docs/buildkite-release-setup.md) — the Boss release pipeline this is modeled on
