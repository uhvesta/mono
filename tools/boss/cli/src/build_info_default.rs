// Default (unstamped) build info constants for non-Bazel builds.
// Bazel builds override these via the build_info_rs genrule wired through
// compile_data + BOSS_BUILD_INFO_RS in rustc_env.
pub const BOSS_VERSION: &str = "unknown";
// Part of the build-info interface; Bazel stamps these with real values.
// The CLI currently only surfaces BOSS_VERSION, but the constants must
// exist here so the default and stamped files share the same shape.
#[allow(dead_code)]
pub const BOSS_GIT_SHA: &str = "unknown";
#[allow(dead_code)]
pub const BOSS_BUILD_TIME: &str = "unknown";
