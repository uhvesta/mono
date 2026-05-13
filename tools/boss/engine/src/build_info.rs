//! Engine build identification.
//!
//! The live-status debug verb surfaces these so a stale-binary
//! problem ("you merged the fix but rebuilt this morning's engine?")
//! is immediately visible without guessing. We deliberately do NOT
//! add a Cargo build script — Bazel is the canonical build for this
//! repo and threading stamping through it is a separate piece of
//! work. Instead we lean on four signals, in order of usefulness:
//!
//! 1. `BOSS_ENGINE_GIT_SHA` — opt-in compile-time env var, set when
//!    the build environment knows the SHA (`BOSS_ENGINE_GIT_SHA=$(git
//!    rev-parse --short HEAD) cargo build …` or the equivalent bazel
//!    stamp). Empty / unset falls through.
//! 2. `BOSS_ENGINE_BUILD_TIME` — opt-in compile-time env var with the
//!    build timestamp in ISO-8601 UTC.
//! 3. The engine binary's filesystem mtime, evaluated at startup.
//!    Imperfect (a rebuild that produces a bit-identical binary may
//!    not bump it on every platform; running through `bazel run` from
//!    a stale cache shows the cache hit's mtime) but vastly better
//!    than nothing.
//! 4. **Binary content fingerprint** — short SHA-256 of the engine
//!    binary's bytes, computed once at first call to
//!    [`binary_fingerprint`]. Survives a bazel cache hit that
//!    doesn't bump mtime: two identical binaries produce the same
//!    fingerprint, a rebuild that actually changes any code
//!    produces a different one. This is the unambiguous "am I
//!    running the binary I think I am?" signal — `(1)` and `(2)`
//!    can both lie when the build environment doesn't stamp them.
//!
//! The CARGO_PKG_VERSION fallback is included so even a build with
//! none of the above produces something other than "unknown".
//!
//! Follow-up tracked in PR body: the Bazel `rust_binary` rule should
//! be extended to thread `BOSS_ENGINE_GIT_SHA` and
//! `BOSS_ENGINE_BUILD_TIME` via a workspace-status command, at which
//! point `git_sha()` will stop returning "unknown" for the canonical
//! release engine path. Until then, the runtime fingerprint covers
//! the operationally-important question ("is this the binary I
//! shipped?").

use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Format the canonical `--version` string for a bundled Boss binary.
///
/// Output: `<name> 0+<sha> built <time>`, e.g.
/// `boss-engine 0+abc1234 built 2026-05-12T11:14:02Z`.
///
/// The leading `0` is a placeholder major version per the design doc Q7:
/// "until we cut a real v1.0 release with a versioning policy, every
/// artifact is '0+<sha>'."
///
/// TODO(chore-1): switch sha/time to the genrule-linked constants once
/// the workspace-status.sh + `build_info_rs` genrule from chore 1 lands.
/// Until then, the values are "unknown" unless the build environment
/// stamps BOSS_ENGINE_GIT_SHA / BOSS_ENGINE_BUILD_TIME.
pub fn version_string(binary_name: &str) -> String {
    format!("{binary_name} 0+{} built {}", git_sha(), build_time())
}

/// Short git SHA the engine binary was built from, baked at compile
/// time. Returns `"unknown"` when the build environment did not
/// stamp `BOSS_ENGINE_GIT_SHA`.
pub fn git_sha() -> &'static str {
    match option_env!("BOSS_ENGINE_GIT_SHA") {
        Some(s) if !s.is_empty() => s,
        _ => "unknown",
    }
}

/// Best-effort build timestamp string. Prefers the
/// `BOSS_ENGINE_BUILD_TIME` env var captured at compile time; falls
/// back to the binary mtime sampled at first call.
pub fn build_time() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        if let Some(t) = option_env!("BOSS_ENGINE_BUILD_TIME") {
            if !t.is_empty() {
                return t.to_owned();
            }
        }
        binary_mtime_iso8601().unwrap_or_else(|| {
            format!("unknown (CARGO_PKG_VERSION {})", env!("CARGO_PKG_VERSION"))
        })
    })
    .as_str()
}

fn binary_mtime_iso8601() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let metadata = std::fs::metadata(&path).ok()?;
    let mtime = metadata.modified().ok()?;
    let dur = mtime.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    Some(format_iso8601_utc(dur.as_secs() as i64))
}

/// Short SHA-256 fingerprint of the engine binary's on-disk bytes.
/// Computed once on first call and cached for the lifetime of the
/// process. Returns `"unknown"` if `current_exe()` is unreadable
/// (extremely rare on macOS — the binary is mapped into our address
/// space and the path is accessible by our own uid).
///
/// Twelve hex characters is enough to distinguish builds for human
/// inspection without producing a wall of hash text in the debug
/// output. Two distinct binaries collide with probability ~2^-48 in
/// the worst case — fine for "is the binary I'm running the one I
/// expected" questions and unsuitable for cryptographic uses.
///
/// Reads at most [`FINGERPRINT_CAP_BYTES`] of the binary so we don't
/// stall startup on an unusually large file; the engine binary in
/// practice is well under that cap. Capping is documented in the
/// fingerprint string itself when it bites (`"…-truncated"` suffix)
/// so a mismatch isn't mistaken for two different binaries when one
/// of them was simply over the cap.
pub fn binary_fingerprint() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| match compute_binary_fingerprint() {
        Some(s) => s,
        None => "unknown".to_owned(),
    })
    .as_str()
}

/// Cap on the number of bytes the fingerprinter reads from the
/// engine binary. The boss-engine binary is currently well under
/// this; capping keeps an outlier (a debug build with symbols, a
/// future binary that grows) from delaying engine startup.
const FINGERPRINT_CAP_BYTES: u64 = 64 * 1024 * 1024;

fn compute_binary_fingerprint() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let mut file = std::fs::File::open(&path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut read_total: u64 = 0;
    let mut truncated = false;
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        let remaining = FINGERPRINT_CAP_BYTES.saturating_sub(read_total);
        let take = (n as u64).min(remaining) as usize;
        hasher.update(&buf[..take]);
        read_total += take as u64;
        if take < n {
            truncated = true;
            break;
        }
        if read_total >= FINGERPRINT_CAP_BYTES {
            // We've consumed exactly the cap; check if another byte
            // remains by attempting one more read.
            let probe = file.read(&mut buf).ok()?;
            if probe > 0 {
                truncated = true;
            }
            break;
        }
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(12 + 10);
    for byte in &digest[..6] {
        hex.push_str(&format!("{byte:02x}"));
    }
    if truncated {
        hex.push_str("-truncated");
    }
    Some(hex)
}

/// One-time stamp for "when did this engine process start". Captured
/// at first call so the debug verb reports a stable timestamp even
/// after the engine has been up for hours.
pub fn process_started_at() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        format_iso8601_utc(secs)
    })
    .as_str()
}

/// Mirror of `live_worker_state::format_iso8601_utc`. Duplicated here
/// to avoid pulling that module's path through a public API path
/// rename later.
fn format_iso8601_utc(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let seconds_in_day = epoch_secs.rem_euclid(86_400);
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let second = seconds_in_day % 60;
    let (year, month, day) = ymd_from_days_since_1970(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    )
}

fn ymd_from_days_since_1970(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_sha_returns_non_empty_string() {
        let s = git_sha();
        assert!(!s.is_empty());
    }

    #[test]
    fn build_time_returns_non_empty_string() {
        let s = build_time();
        assert!(!s.is_empty());
    }

    #[test]
    fn process_started_at_returns_iso8601_shape() {
        let s = process_started_at();
        assert_eq!(s.len(), 20, "expected YYYY-MM-DDTHH:MM:SSZ, got {s}");
        assert!(s.ends_with('Z'), "expected trailing Z, got {s}");
    }

    #[test]
    fn binary_fingerprint_is_stable_within_a_process() {
        // Two reads must agree; the first read populates the
        // OnceLock and the second hits the cache. Catches a refactor
        // that drops the cache and starts hashing on every call.
        let a = binary_fingerprint();
        let b = binary_fingerprint();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn binary_fingerprint_has_expected_shape() {
        // 12 hex chars (6 bytes), optionally followed by
        // "-truncated" if the cap kicked in. Test binaries are well
        // under the cap so we expect the bare 12-char form.
        let s = binary_fingerprint();
        if s == "unknown" {
            // `current_exe()` failed for whatever reason — fingerprint
            // is best-effort, not a hard requirement, so a sandboxed
            // test runner is allowed to land here.
            return;
        }
        let core = s.strip_suffix("-truncated").unwrap_or(s);
        assert_eq!(
            core.len(),
            12,
            "expected 12 hex chars (optionally suffixed with -truncated), got {s}"
        );
        assert!(
            core.chars().all(|c| c.is_ascii_hexdigit()),
            "fingerprint must be hex digits only, got {s}"
        );
    }
}
