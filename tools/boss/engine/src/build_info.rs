//! Engine build identification.
//!
//! The live-status debug verb surfaces these so a stale-binary
//! problem ("you merged the fix but rebuilt this morning's engine?")
//! is immediately visible without guessing. We deliberately do NOT
//! add a Cargo build script — Bazel is the canonical build for this
//! repo and threading stamping through it is a separate piece of
//! work. Instead we lean on three signals, in order of usefulness:
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
//!
//! The CARGO_PKG_VERSION fallback is included so even a build with
//! none of the above produces something other than "unknown".

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
}
