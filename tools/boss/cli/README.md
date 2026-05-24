# boss CLI

Command-line interface for the Boss task-management system.

## boss shake — one-time developer setup

`boss shake <file>` files a bug or feature request against `spinyfin/mono` without any end-user configuration. Credentials for the GitHub App that powers it are embedded in the binary at build time.

**End users do nothing extra**: install Boss, run `boss shake <file>`, and the issue is filed.

Developers who build Boss from source need to set three environment variables before building so the credentials are embedded in the binary. This is a one-time step.

### 1. Register the GitHub App

Go to **GitHub → Settings → Developer settings → GitHub Apps → New GitHub App** (or the `spinyfin` organisation equivalent).

Required settings:
- **App name**: anything recognisable, e.g. `boss-shake-prod`
- **Homepage URL**: `https://github.com/spinyfin/mono`
- **Permissions → Repository permissions → Issues**: `Read and write`
- All other permissions: `No access`
- **Webhook**: disabled (uncheck "Active")

Save the App. Note the **App ID** shown on the App's settings page.

### 2. Generate and download the private key

On the App's settings page, scroll to **Private keys** and click **Generate a private key**. A `.pem` file is downloaded. Keep this file secure; it is the credential that lets the binary authenticate.

### 3. Install the App on `spinyfin/mono`

On the App's settings page click **Install App**, select the `spinyfin` organisation, choose **Only select repositories**, and pick `mono`. Complete the install.

To find the **Installation ID**: go to `https://github.com/organizations/spinyfin/settings/installations`, click the App, and read the numeric ID from the URL (`/installations/<ID>`).

### 4. Stash the credentials in your environment

Set the three variables in your shell profile or via [direnv](https://direnv.net/) (`.envrc` at the repo root is gitignored and a good place for per-repo secrets):

```sh
export BOSS_SHAKE_APP_ID="<App ID from step 1>"
export BOSS_SHAKE_INSTALLATION_ID="<Installation ID from step 3>"
export BOSS_SHAKE_PRIVATE_KEY_PEM="$(cat /path/to/downloaded-key.pem)"
```

The PEM value must include the `-----BEGIN RSA PRIVATE KEY-----` and `-----END RSA PRIVATE KEY-----` markers.

### 5. Build

The three env vars must be set **before** running Bazel (the `.bazelrc` propagates them via `--action_env`):

```sh
bazel build //tools/boss/cli:boss
```

Builds without the env vars succeed but produce a binary where `boss shake` exits 1 with a pointer back to this README. CI and release builds set the vars so published binaries have credentials embedded.

---

### Abuse mitigations (reference)

- **`via-shake` label**: every issue filed by the binary carries this label unconditionally, even when the caller passes other `--label` flags. It can be used as a triage filter (`label:via-shake`) for bulk review or cleanup.
- **`<!-- via boss shake -->` comment**: appended to every issue body. The source remains attributable even if the label is manually removed.
- **GitHub App rate limit**: installation access tokens are subject to the GitHub App's per-installation rate limit (~5 000 requests/hour for standard Apps; confirm at `https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api`). The limit applies to the installation on `spinyfin/mono`, not per end-user.
- **Credential rotation**: if abuse becomes a real problem, generate a new private key for the App, embed it in the next release, and revoke the old key. No automation is needed; this is a straightforward maintenance task.
