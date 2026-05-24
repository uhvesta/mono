# Buildkite: Boss shake secrets setup

This document explains how to provision the three Buildkite secrets that the
`boss-release` pipeline step needs to embed GitHub App credentials into a
published Boss.app release.

## Background

`boss shake` files bug reports against `spinyfin/mono` by authenticating as a
GitHub App. The App's credentials (`APP_ID`, `INSTALLATION_ID`, `PRIVATE_KEY_PEM`)
are embedded into the `boss` binary at compile time via Rust's `option_env!`
macro. A release binary built without these credentials will refuse to run
`boss shake` and print a pointer to `tools/boss/cli/README.md`.

The `boss-release` Buildkite step (`.buildkite/steps/boss-release.sh`) reads
the credentials from Buildkite's secret store at the start of every run, exports
them into the shell environment, and then invokes `bazel build -c opt`. The
`.bazelrc` propagates them to the compiler via `--action_env`, so they are
embedded into the binary and included in Bazel's cache key (keeping the
credential-embedded release build distinct from the credential-free PR build).

---

## Secret names

| Secret name | Description |
|---|---|
| `BOSS_SHAKE_APP_ID` | Numeric GitHub App ID (e.g. `12345`) |
| `BOSS_SHAKE_INSTALLATION_ID` | Numeric installation ID for the `spinyfin/mono` installation |
| `BOSS_SHAKE_PRIVATE_KEY_PEM` | Full RSA private key PEM including `-----BEGIN`/`-----END` markers |

These names are verbatim — copy them exactly into Buildkite.

---

## Option A — Buildkite native secrets store (recommended)

Buildkite's secrets store keeps values out of build logs and never exposes them
in environment dumps.

### 1. Open the secrets UI

In the Buildkite dashboard, navigate to:
**Organization Settings → Secrets**

Or go directly to:
`https://buildkite.com/organizations/<org>/secrets`

### 2. Create each secret

Click **New Secret** for each of the three names above.

For `BOSS_SHAKE_PRIVATE_KEY_PEM`, paste the entire PEM block including the
header and footer lines:

```
-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEA...
(many more base64 lines)
...
-----END RSA PRIVATE KEY-----
```

Buildkite's secrets store accepts multi-line values. Do **not** collapse it to
a single line or escape the newlines — the binary expects the multi-line PEM
format.

### 3. Scope the secrets to the pipeline

After creating each secret, assign it to the `spinyfin/mono` pipeline (or to
the whole `spinyfin` organisation if you prefer).

### 4. Verify the agent version

`buildkite-agent secret get` requires **Buildkite Agent ≥ 3.73.0**. On the
`macos-arm64` agent (Zakalwe-1) run:

```sh
buildkite-agent --version
```

Upgrade if needed.

---

## Option B — Pipeline Settings → Environment Variables

If the Buildkite plan does not include the native secrets store, set the three
variables as pipeline-level environment variables instead.

**Dashboard → spinyfin/mono pipeline → Settings → Environment Variables**

Add each name and value. The `boss-release.sh` script checks for pre-set env
vars before falling back to `buildkite-agent secret get`, so this works without
any script changes.

> **Security note:** Pipeline environment variables may appear in build logs
> when a step runs `env` or a framework prints the environment. Prefer Option A
> (native secrets) for production use.

---

## Verification

After provisioning, trigger a build on `main` (e.g. merge any trivial PR) and
confirm:

1. The `boss-release` step appears in the build and succeeds.
2. A new GitHub Release is created:

```sh
gh release view boss-v1.0.0 --repo spinyfin/mono
```

3. `Boss-1.0.0.zip` is attached to the release.

If the step fails with `Missing Buildkite secrets: …`, one or more secrets were
not found — recheck the names (they are case-sensitive) and the pipeline scope.

---

## Credential rotation

When the GitHub App's private key needs rotating:

1. Go to **GitHub → Settings → Developer settings → GitHub Apps → boss-shake-prod → Private keys**.
2. Click **Generate a private key** to create the replacement key.
3. Update the `BOSS_SHAKE_PRIVATE_KEY_PEM` secret in Buildkite with the new PEM.
4. Merge any commit to `main` to trigger a release build with the new key.
5. Once the release is published successfully, revoke the old key on the same
   GitHub App page.

---

## Related

- `tools/boss/cli/README.md` — developer setup for local builds with credentials
- `.buildkite/steps/boss-release.sh` — the step that reads these secrets and publishes the release
- `.buildkite/pipeline.yml` — `boss-release` step definition (runs only on `main`, after `bazel-test`)
