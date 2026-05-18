//! Wrapper-script source + version stamping.
//!
//! The wrapper script (`tools/boss/engine/remote/boss-remote-run.sh`)
//! is the engine's contract with remote workers: env vars in, exec
//! shape out. The engine bundles the source verbatim via `include_str!`
//! and stamps the canonical version string into it before pushing to a
//! remote host. The pushed file is what the remote actually runs.
//!
//! Version policy (per the distributed-agent-execution design,
//! "Wrapper Distribution"):
//!
//! - The wrapper carries a `BOSS_REMOTE_RUN_VERSION` constant near the
//!   top, replaced at engine build time with a value derived from the
//!   engine's git short SHA (e.g. `eng-7a3f2c1`).
//! - `--version` prints exactly that string and exits zero.
//! - The engine's expected version is the same string baked into the
//!   engine binary; comparison is exact-equality, not semver.
//! - Any mismatch triggers a re-push.

/// Verbatim wrapper script source. Bundled at compile time so the
/// engine has one source of truth and no separate distribution path.
const WRAPPER_SOURCE: &str = include_str!("../remote/boss-remote-run.sh");

/// Sentinel string in the wrapper source that the engine replaces with
/// the canonical version string at push time. Defined once so a typo
/// in either side fails the unit test below at build time.
const VERSION_PLACEHOLDER: &str = "__BOSS_REMOTE_RUN_VERSION__";

/// The canonical wrapper version string. Built from the engine's git
/// short SHA when stamped; falls back to `eng-unknown` when the build
/// environment didn't stamp `BOSS_ENGINE_GIT_SHA`.
///
/// Exact-equality is the engine ↔ wrapper version contract. Any change
/// to the wrapper source that would invalidate the contract should
/// trigger a fresh engine build (and therefore a fresh SHA).
pub fn expected_version() -> String {
    format!("eng-{}", crate::build_info::git_sha())
}

/// Return the wrapper source ready to push to a remote host, with the
/// `__BOSS_REMOTE_RUN_VERSION__` placeholder replaced by [`expected_version`].
///
/// Panics if the placeholder isn't present in the source — that means
/// the wrapper script was edited in a way that broke the contract. The
/// unit test `placeholder_present_in_source` catches the same problem
/// at build time so a panic in production is unlikely.
pub fn rendered_wrapper() -> String {
    let version = expected_version();
    debug_assert!(
        WRAPPER_SOURCE.contains(VERSION_PLACEHOLDER),
        "wrapper source missing __BOSS_REMOTE_RUN_VERSION__ placeholder"
    );
    WRAPPER_SOURCE.replacen(VERSION_PLACEHOLDER, &version, 1)
}

/// Remote install path (per the design's "Install location on the remote").
pub const REMOTE_WRAPPER_DIR: &str = ".boss-remote/bin";

/// Filename of the wrapper on the remote.
pub const REMOTE_WRAPPER_NAME: &str = "boss-remote-run";

/// Absolute install path on the remote relative to `$HOME`. The remote
/// expansion happens via the wrapper invocation itself; the engine
/// always invokes with `~/.boss-remote/bin/boss-remote-run` so it
/// doesn't need to know the remote's `$HOME` value.
pub fn remote_wrapper_path() -> String {
    format!("~/{REMOTE_WRAPPER_DIR}/{REMOTE_WRAPPER_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_present_in_source() {
        assert!(
            WRAPPER_SOURCE.contains(VERSION_PLACEHOLDER),
            "wrapper source must contain the version placeholder so the engine \
             can stamp a real version before push; the build-time `include_str!` \
             would otherwise ship un-versioned bytes"
        );
    }

    #[test]
    fn rendered_wrapper_replaces_placeholder() {
        let rendered = rendered_wrapper();
        assert!(
            !rendered.contains(VERSION_PLACEHOLDER),
            "rendered wrapper still contains the placeholder; replacen failed"
        );
        let expected = expected_version();
        assert!(
            rendered.contains(&expected),
            "rendered wrapper should contain `{expected}` but did not"
        );
    }

    #[test]
    fn expected_version_has_eng_prefix() {
        let v = expected_version();
        assert!(v.starts_with("eng-"), "expected_version should start with `eng-`, got {v}");
    }

    #[test]
    fn wrapper_source_has_shebang() {
        // The remote ends up running the file directly via
        // `~/.boss-remote/bin/boss-remote-run`, so the shebang is
        // load-bearing. A refactor that strips the first line would
        // produce a wrapper that fails with "exec format error".
        assert!(
            WRAPPER_SOURCE.starts_with("#!/bin/sh\n"),
            "wrapper must start with `#!/bin/sh` so the kernel runs it via /bin/sh"
        );
    }
}
