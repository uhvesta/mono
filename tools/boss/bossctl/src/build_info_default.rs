// Default (unstamped) build info constants for non-Bazel builds.
// Bazel builds override these via the build_info_rs genrule wired through
// compile_data + BOSS_BUILD_INFO_RS in rustc_env.
pub const BOSS_VERSION: &str = "unknown";
pub const BOSS_GIT_SHA: &str = "unknown";
pub const BOSS_BUILD_TIME: &str = "unknown";
