#!/usr/bin/env bash
# checkleft-release.sh — cross-platform release step for the `checkleft` binary.
#
# Modeled on .buildkite/steps/boss-release.sh. checkleft ships as a prebuilt
# binary consumed by external repos, so this step bumps the version, tags the
# release, builds the binaries for every supported platform, and publishes them
# as assets on a GitHub Release.
#
# Unlike boss (a single macOS .app produced on one agent), checkleft needs
# binaries for both Linux and macOS, so the work is split into three phases:
#
#   prepare — the orchestrator. Runs the skip-logic, computes the alpha version,
#             tags the release commit, and creates the GitHub Release. Hands the
#             tag to the build phases via buildkite-agent meta-data.
#   linux   — builds the Linux binaries and uploads them to the release.
#   darwin  — builds the macOS binaries and uploads them to the release.
#
# The linux and darwin build phases both depend only on `prepare`, so they run
# in PARALLEL on separate agents; wall-clock is prepare + max(linux, darwin)
# rather than the sum.
#
# The version bump is NEVER committed to main. It is patched into the build
# checkout only (so the release builds from a tree carrying the new version) and
# recorded in the git tag + GitHub Release. The tag points at the release commit
# (BUILDKITE_COMMIT) itself, so pushing it needs only `contents: write` — no
# branch-protection bypass, unlike pushing a commit to main. Developer builds
# off main therefore carry whatever (possibly stale) version Cargo.toml holds;
# that is intentional and harmless: checkleft's CLI does not embed
# CARGO_PKG_VERSION (no `#[command(version)]`), so the compiled binary is
# byte-identical regardless of the version string. All phases build from the
# SAME commit (BUILDKITE_COMMIT) and need not share a checkout.
#
# Trigger model (see tools/checkleft/docs/buildkite-release-setup.md):
#   - scheduled (cron) builds  → skip if nothing under checkleft changed since
#                                the last checkleft-v* tag.
#   - manual builds (ui / api) → always release.
#
# Auth: releases run on the CI agents' ambient git + `gh` credentials (every
# worker can push to the repo), exactly like boss-release.sh — the tag is pushed
# with `git push origin` and the GitHub Release is created with `gh`. No
# dedicated release token is needed.
set -euo pipefail

# ── configuration ─────────────────────────────────────────────────────────────
REPO="spinyfin/mono"
TAG_PREFIX="checkleft-v"
CARGO_TOML="tools/checkleft/Cargo.toml"
CARGO_LOCK="Cargo.lock"
BIN_TARGET="//tools/checkleft:checkleft"
ASSET_PREFIX="checkleft"
META_TAG_KEY="checkleft-release-tag"

# Mutable state read by the EXIT trap. Globals (not function-locals) so the trap
# can reference them under `set -u` after the phase function has returned.
STAGE=""
TAG_PUSHED=0
NEW_TAG=""

# checkleft-affecting paths for change-detection. Mirrors boss-release.sh's
# scoping: the binary's source, the release script, and the pipeline wiring.
# `tools/checkleft/` (trailing slash) deliberately excludes tools/checkleft_package/.
CHANGE_PATHS_RE='^(tools/checkleft/|\.buildkite/steps/checkleft-release\.sh|\.buildkite/pipeline-checkleft-release\.yml)'

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

die() { echo "ERROR: $*" >&2; exit 1; }
log() { echo "--- $*"; }

# ── buildkite meta-data helpers (env-overridable for local dry runs) ──────────

meta_set() {
  command -v buildkite-agent &>/dev/null || return 0
  buildkite-agent meta-data set "$1" "$2"
}

meta_get() {
  local key="$1"
  if command -v buildkite-agent &>/dev/null; then
    buildkite-agent meta-data get "$key" --default "" 2>/dev/null || true
  fi
}

# ── version helpers ───────────────────────────────────────────────────────────

# sha256 of a file, on either platform.
_sha256() {
  if command -v sha256sum &>/dev/null; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# compute_next_version — sets BASE, NEW_VERSION, NEW_TAG from the current
# Cargo.toml alpha version, cross-checked against existing checkleft-v* releases
# so a stale Cargo.toml can never reuse an already-published alpha. For now only
# the `-alpha.N` counter is bumped; MAJOR.MINOR.PATCH are carried through.
compute_next_version() {
  local cur
  cur="$(grep -E '^version = "' "${CARGO_TOML}" | head -1 | sed -E 's/^version = "(.*)"/\1/')"
  [[ -n "${cur}" ]] || die "could not read version from ${CARGO_TOML}"

  if [[ ! "${cur}" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-alpha\.([0-9]+)$ ]]; then
    die "checkleft version '${cur}' is not in the expected X.Y.Z-alpha.N form; this pipeline only revs the alpha counter — bump the base version by hand if intended"
  fi
  BASE="${BASH_REMATCH[1]}"
  local cargo_alpha="${BASH_REMATCH[2]}"

  # Highest existing alpha among published releases for this base version.
  local release_max=-1 tag n
  while IFS= read -r tag; do
    if [[ "${tag}" =~ ^${TAG_PREFIX}${BASE}-alpha\.([0-9]+)$ ]]; then
      n="${BASH_REMATCH[1]}"
      if (( n > release_max )); then release_max="${n}"; fi
    fi
  done < <(gh release list --repo "${REPO}" --limit 300 --json tagName --jq '.[].tagName' 2>/dev/null || true)

  local highest="${cargo_alpha}"
  if (( release_max > highest )); then highest="${release_max}"; fi
  local next=$(( highest + 1 ))

  CUR_VERSION="${cur}"
  NEW_VERSION="${BASE}-alpha.${next}"
  NEW_TAG="${TAG_PREFIX}${NEW_VERSION}"
}

# apply_version_edits — rewrite the version in Cargo.toml and Cargo.lock in the
# release checkout so the binaries build from a tree carrying NEW_VERSION and
# `cargo --locked` / crate_universe stay self-consistent. This edit is NEVER
# committed — it lives only in the CI working copy for the duration of the build.
apply_version_edits() {
  # Package version line in Cargo.toml (anchored; deps use indented `version =`).
  sed -i.bak -E "s|^version = \"${CUR_VERSION}\"|version = \"${NEW_VERSION}\"|" "${CARGO_TOML}"
  rm -f "${CARGO_TOML}.bak"
  # The version line immediately following the checkleft package entry in the lock.
  sed -i.bak -E "/^name = \"checkleft\"$/{n;s|^version = \".*\"|version = \"${NEW_VERSION}\"|;}" "${CARGO_LOCK}"
  rm -f "${CARGO_LOCK}.bak"
  grep -q "version = \"${NEW_VERSION}\"" "${CARGO_TOML}" || die "Cargo.toml version edit failed"
}

# ── build helpers ─────────────────────────────────────────────────────────────

# build_native_bazel — optimized native binary via bazel, echoes its path.
build_native_bazel() {
  log "[checkleft-release] bazel build -c opt ${BIN_TARGET}" >&2
  bazel build -c opt "${BIN_TARGET}" >&2
  local path
  # `|| true`: head can SIGPIPE cquery under pipefail; the guard below reports.
  path="$(bazel cquery -c opt --output=files "${BIN_TARGET}" 2>/dev/null | head -1 || true)"
  [[ -n "${path}" && -f "${path}" ]] || { echo "could not locate bazel binary output" >&2; return 1; }
  echo "${path}"
}

# build_cross_cargo TRIPLE — cross binary via cargo, echoes its path.
# Returns non-zero (without aborting the script) if the target toolchain is
# unavailable, so optional targets (musl) can be skipped gracefully.
build_cross_cargo() {
  local triple="$1"
  log "[checkleft-release] rustup target add ${triple}" >&2
  rustup target add "${triple}" >&2 || { echo "rustup target add ${triple} failed" >&2; return 1; }
  log "[checkleft-release] cargo build --release -p checkleft --target ${triple}" >&2
  cargo build --release --locked -p checkleft --target "${triple}" >&2 || {
    echo "cargo build for ${triple} failed" >&2; return 1; }
  local path="target/${triple}/release/checkleft"
  [[ -f "${path}" ]] || { echo "expected ${path} not produced" >&2; return 1; }
  echo "${path}"
}

# stage_asset SRC NAME — copy SRC to the staging dir as NAME and write NAME.sha256.
stage_asset() {
  local src="$1" name="$2" sum
  cp -L "${src}" "${STAGE}/${name}"
  chmod +x "${STAGE}/${name}"
  sum="$(_sha256 "${STAGE}/${name}")"
  # Sidecar references the bare asset name so `sha256sum -c` works from the dir.
  echo "${sum}  ${name}" > "${STAGE}/${name}.sha256"
  echo "[checkleft-release] staged ${name} ($(du -h "${STAGE}/${name}" | cut -f1))"
}

# upload_release_assets — upload every staged file to the release, with retry.
upload_release_assets() {
  local ok=0 attempt
  for attempt in 1 2 3; do
    if gh release upload "${NEW_TAG}" "${STAGE}"/* --repo "${REPO}" --clobber; then
      ok=1; break
    fi
    echo "[checkleft-release] upload attempt ${attempt} failed; sleeping $((attempt * 15))s"
    sleep $((attempt * 15))
  done
  (( ok == 1 )) || die "asset upload failed after 3 attempts for ${NEW_TAG}; retry the job, or upload manually with 'gh release upload ${NEW_TAG} <files> --repo ${REPO} --clobber'"
}

# ── skip-logic (linux phase only) ─────────────────────────────────────────────

is_manual() {
  [[ "${BUILDKITE_SOURCE:-}" == "ui" || "${BUILDKITE_SOURCE:-}" == "api" ]]
}

# resolve_last_release — sets LAST_TAG and LAST_SHA for the newest checkleft-v* release.
resolve_last_release() {
  log "[checkleft-release] resolving last ${TAG_PREFIX}* release"
  LAST_TAG="$(gh release list --repo "${REPO}" --limit 300 --json tagName \
    --jq "[.[] | select(.tagName | startswith(\"${TAG_PREFIX}\"))] | .[0].tagName" 2>/dev/null || true)"
  LAST_SHA=""
  if [[ -n "${LAST_TAG}" ]]; then
    git fetch origin "refs/tags/${LAST_TAG}:refs/tags/${LAST_TAG}" 2>/dev/null || true
    LAST_SHA="$(git rev-list -n 1 "${LAST_TAG}" 2>/dev/null || true)"
    if [[ -z "${LAST_SHA}" ]]; then
      LAST_SHA="$(gh api "repos/${REPO}/commits/${LAST_TAG}" --jq '.sha' 2>/dev/null || true)"
    fi
  fi
}

# should_skip — echoes a skip reason and returns 0 when this run is a no-op.
should_skip() {
  local head_sha
  head_sha="$(git rev-parse HEAD 2>/dev/null || echo "${BUILDKITE_COMMIT:-}")"

  # Idempotency guard (all trigger paths, including manual): never re-release a
  # commit that is already the head of the latest release tag.
  if [[ -n "${LAST_SHA}" && "${head_sha}" == "${LAST_SHA}" ]]; then
    echo "release skipped: HEAD (${head_sha:0:12}) is already ${LAST_TAG} — re-releasing the same commit is a no-op"
    return 0
  fi

  if is_manual; then
    echo ""  # manual trigger always proceeds
    return 1
  fi

  if [[ -z "${LAST_TAG}" ]]; then
    echo ""  # no prior release; proceed with the first
    return 1
  fi
  if [[ -z "${LAST_SHA}" ]]; then
    echo ""  # could not resolve tag SHA; proceed rather than silently stall
    return 1
  fi

  if git rev-parse --is-shallow-repository 2>/dev/null | grep -q true; then
    git fetch --unshallow origin 2>/dev/null || true
  fi

  local touched checkleft_touched
  touched="$(git diff --name-only "${LAST_SHA}..HEAD" 2>/dev/null || true)"
  checkleft_touched="$(echo "${touched}" | grep -E "${CHANGE_PATHS_RE}" || true)"
  if [[ -z "${checkleft_touched}" ]]; then
    echo "release skipped: no checkleft-affecting changes since ${LAST_TAG}"
    return 0
  fi
  echo ""
  return 1
}

# cleanup — single EXIT trap. Removes the staging dir and, if a tag was pushed
# but the release never completed, deletes the leaked remote tag. No commit is
# ever pushed to main, so there is nothing else to unwind. All state is read
# defensively for `set -u` safety.
#
# `return 0` is REQUIRED: as the EXIT trap, this function's exit status becomes
# the script's. Phases that never set STAGE (prepare; a skipped run; a build
# phase that finds no tag) would otherwise end on the false `[[ -n "" ]]` test
# and exit 1 despite succeeding.
cleanup() {
  if [[ "${TAG_PUSHED}" == "1" && -n "${NEW_TAG}" ]]; then
    echo "[checkleft-release] release did not complete — deleting leaked tag ${NEW_TAG}" >&2
    git push origin ":refs/tags/${NEW_TAG}" 2>/dev/null || true
  fi
  [[ -n "${STAGE}" ]] && rm -rf "${STAGE}"
  return 0
}

# ── phases ────────────────────────────────────────────────────────────────────

phase_prepare() {
  echo "[checkleft-release] agent: $(uname -a)"
  resolve_last_release

  local skip_reason
  skip_reason="$(should_skip)" || true
  if [[ -n "${skip_reason}" ]]; then
    echo "${skip_reason}"
    exit 0  # no tag published to meta-data → the build phases skip too
  fi

  compute_next_version
  log "[checkleft-release] ${CUR_VERSION} -> ${NEW_VERSION} (tag ${NEW_TAG})"

  # Tag the existing commit on main (no bump commit is created). Pushing a tag
  # (unlike a commit to main) needs no branch-protection bypass.
  local release_sha
  release_sha="$(git rev-parse HEAD 2>/dev/null || echo "${BUILDKITE_COMMIT:-}")"
  [[ -n "${release_sha}" ]] || die "could not resolve the commit to release/tag"

  # ── point of no return: tag the release commit, push the tag, create release ─
  # The cleanup trap deletes the tag if this phase dies before the release is
  # created (the window guarded by TAG_PUSHED). Once the release exists, the
  # build phases attach assets to it; a build failure is recoverable by retrying
  # that job, so the tag/release are intentionally left in place.
  log "[checkleft-release] tagging ${NEW_TAG} at ${release_sha:0:12}"
  git tag "${NEW_TAG}" "${release_sha}"
  git push origin "refs/tags/${NEW_TAG}" \
    || die "tag push rejected for ${NEW_TAG}. The tag may already exist (a prior run pushed it — delete it and retry), or the agent's git credentials cannot push to ${REPO}."
  TAG_PUSHED=1

  log "[checkleft-release] creating GitHub Release ${NEW_TAG}"
  gh release create "${NEW_TAG}" --repo "${REPO}" \
    --title "checkleft ${NEW_VERSION}" --generate-notes

  # Hand the tag to the parallel build phases.
  meta_set "${META_TAG_KEY}" "${NEW_TAG}"
  TAG_PUSHED=0  # release created; stop guarding the tag
  log "[checkleft-release] prepare done — ${NEW_TAG} created; build phases will attach assets"
}

# resolve_release_tag — set NEW_TAG/NEW_VERSION from the tag prepare published to
# meta-data (or a CHECKLEFT_RELEASE_TAG override for manual recovery). Exits 0
# when there is no tag — prepare skipped this run, so the build phase is a no-op.
resolve_release_tag() {
  NEW_TAG="${CHECKLEFT_RELEASE_TAG:-$(meta_get "${META_TAG_KEY}")}"
  if [[ -z "${NEW_TAG}" ]]; then
    echo "[checkleft-release] no release tag from the prepare phase (it skipped or did not run) — nothing to do"
    exit 0
  fi
  NEW_VERSION="${NEW_TAG#"${TAG_PREFIX}"}"
}

phase_linux() {
  [[ "$(uname -s)" == "Linux" ]] || die "linux phase landed on $(uname -s); the step must target an os=linux agent (see agents: in .buildkite/pipeline-checkleft-release.yml)"

  echo "[checkleft-release] agent: $(uname -a)"
  resolve_release_tag

  # Patch the release version into the build checkout (NEVER committed; the
  # binary is version-independent, so this is for tree self-consistency).
  CUR_VERSION="$(grep -E '^version = "' "${CARGO_TOML}" | head -1 | sed -E 's/^version = "(.*)"/\1/')"
  apply_version_edits

  log "[checkleft-release] building Linux assets for ${NEW_TAG}"
  STAGE="$(mktemp -d)"

  local gnu_path
  gnu_path="$(build_native_bazel)"
  stage_asset "${gnu_path}" "${ASSET_PREFIX}-x86_64-unknown-linux-gnu"

  # musl is best-effort: a static build is nice-to-have, not release-blocking.
  local musl_path
  if musl_path="$(build_cross_cargo x86_64-unknown-linux-musl)"; then
    stage_asset "${musl_path}" "${ASSET_PREFIX}-x86_64-unknown-linux-musl"
  else
    echo "[checkleft-release] WARNING: musl build unavailable (install musl-tools on the agent to enable); shipping without it"
  fi

  upload_release_assets
  log "[checkleft-release] linux phase done — Linux assets attached to ${NEW_TAG}"
}

phase_darwin() {
  [[ "$(uname -s)" == "Darwin" ]] || die "darwin phase must run on a macOS agent (got $(uname -s)); the step must target an os=darwin agent (see agents: in .buildkite/pipeline-checkleft-release.yml)"

  echo "[checkleft-release] agent: $(uname -a)"
  resolve_release_tag
  log "[checkleft-release] building macOS assets for ${NEW_TAG}"

  STAGE="$(mktemp -d)"

  # Native arm64 via bazel (matches how mono builds checkleft).
  local arm_path
  arm_path="$(build_native_bazel)"
  stage_asset "${arm_path}" "${ASSET_PREFIX}-aarch64-apple-darwin"

  # x86_64 via cargo cross — Apple's toolchain builds both arches natively.
  local x86_path
  if x86_path="$(build_cross_cargo x86_64-apple-darwin)"; then
    stage_asset "${x86_path}" "${ASSET_PREFIX}-x86_64-apple-darwin"
  else
    echo "[checkleft-release] WARNING: darwin x86_64 build failed; shipping arm64 only"
  fi

  upload_release_assets
  log "[checkleft-release] darwin phase done — macOS assets attached to ${NEW_TAG}"
}

# ── entrypoint ────────────────────────────────────────────────────────────────

main() {
  local phase="${1:-}"
  trap cleanup EXIT
  case "${phase}" in
    prepare) phase_prepare ;;
    linux)   phase_linux ;;
    darwin)  phase_darwin ;;
    *) die "usage: $0 <prepare|linux|darwin>" ;;
  esac
}

main "$@"
