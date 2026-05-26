# Buildkite: Boss release setup

This document covers the full setup for the Boss release pipeline: the hourly
cron schedule, the `BK_API_TOKEN` env var for `boss release`, and how to verify
everything is wired correctly.

The pipeline also needs the shake credentials (for embedding GitHub App
credentials into the release binary). Those are documented separately in
`tools/boss/docs/buildkite-shake-secrets-setup.md`.

---

## How releases are triggered

The `boss-release` step in `.buildkite/pipeline.yml` only fires when
`BUILDKITE_SOURCE` is `schedule`, `ui`, or `api`. It does **not** fire on every
merge to main. There are two normal trigger paths:

| Trigger | When | Change-detection |
|---|---|---|
| Hourly cron schedule | Every hour at :00 | Skips if no Boss-affecting changes since last `boss-v*` tag |
| `boss release` CLI (or BK UI/API) | On demand | Always releases (skips change-detection) |

---

## 1. Configure the hourly cron schedule in Buildkite

This is a one-time setup in the Buildkite web UI.

1. Go to the `spinyfin/mono` pipeline in the Buildkite dashboard.
2. Click **Settings** → **Schedules** → **New Schedule**.
3. Fill in the fields:
   - **Description:** `Boss hourly release`
   - **Cron interval:** `0 * * * *` (every hour on the hour)
   - **Branch:** `main`
   - **Message:** `Hourly Boss release check` (shown in the BK build list)
   - **Commit:** `HEAD`
4. Click **Create Schedule**.

The schedule fires every hour. If no Boss-affecting files have changed since
the last `boss-v*` tag, the script exits 0 without creating a release.

---

## 2. Provision `BK_API_TOKEN` for `boss release`

The `boss release` CLI subcommand calls the Buildkite REST API to trigger a
build. It reads the token from the `BK_API_TOKEN` environment variable.

### Create an API token

1. In the Buildkite dashboard, go to your **Personal Settings** → **API Access
   Tokens** → **New API Access Token**.
2. Give it a description: `boss release CLI`.
3. Grant the **Write Builds** scope on the `spinyfin` organization (or
   narrower: just the `mono` pipeline if Buildkite supports pipeline-scoped
   tokens in your plan).
4. Click **Create Token** and copy the value — it is only shown once.

### Set the env var

Add it to your shell profile (e.g. `~/.zshrc` or `~/.bashrc`):

```sh
export BK_API_TOKEN="your-token-here"
```

Reload your shell or run `source ~/.zshrc`.

---

## 3. Verify the setup

### Verify `boss release`

```sh
boss release
```

Expected output (on success):

```
triggered release build #42: https://buildkite.com/flunge/mono/builds/42
```

Open the URL and confirm the `boss-release` step appears and runs.

If `BK_API_TOKEN` is missing:

```
error: BK_API_TOKEN is not set. See tools/boss/docs/buildkite-release-setup.md ...
```

### Verify the cron schedule

After the next top-of-hour fires, check the BK builds list for a build with
message `Hourly Boss release check`. If Boss-affecting files changed since the
last release tag, a new `boss-v1.0.N` release will appear on GitHub. If not,
the build will show a line like:

```
release step skipped: no Boss-affecting changes since boss-v1.0.3 (touched: ...)
```

---

## 4. Trigger a release manually (without the CLI)

You can also trigger a release directly from the Buildkite UI:

1. Go to the `spinyfin/mono` pipeline.
2. Click **New Build**.
3. Set **Branch** to `main` and add a message, then click **Create Build**.

Because `BUILDKITE_SOURCE` will be `ui`, the boss-release script skips
change-detection and always produces a release.

---

## Related

- `tools/boss/docs/buildkite-shake-secrets-setup.md` — shake credential setup
  (required for embedding GitHub App credentials into the release binary)
- `.buildkite/steps/boss-release.sh` — the release script
- `.buildkite/pipeline.yml` — `boss-release` step definition
