//! Cron + IANA-timezone occurrence math for the automation scheduler.
//!
//! This module is the correctness-critical heart of Maint task 5
//! (`tools/boss/docs/designs/maintenance-tasks.md` → "Scheduling
//! semantics" + "Timezone / DST handling"). It is deliberately pure: no
//! DB, no async, no wall-clock reads. "Now" is always passed in as UTC
//! epoch seconds, and occurrences are returned as UTC epoch seconds, so
//! every behaviour — including the DST edge cases — is exhaustively
//! unit-testable with fixed inputs.
//!
//! ## What it computes
//!
//! Given a 5-field cron expression (`min hour dom month dow`) and an IANA
//! timezone name (e.g. `"America/Los_Angeles"`), [`next_occurrence_after`]
//! returns the earliest occurrence strictly after a given instant,
//! interpreting the cron wall-clock fields *in the stored timezone*. So
//! "every weekday at 2pm" means 2pm local across DST transitions, not a
//! frozen UTC offset.
//!
//! ## DST semantics (the two hard cases)
//!
//! * **Spring-forward gap** — a wall-clock time that does not exist (e.g.
//!   02:30 on the US spring-forward day, when clocks jump 02:00→03:00).
//!   We advance to the next instant that *does* exist (here 03:00): the
//!   job runs once, slightly later, rather than zero times.
//! * **Fall-back overlap** — a wall-clock time that occurs twice (e.g.
//!   01:30 on the US fall-back day, when clocks repeat 01:00–01:59). We
//!   fire on the **earliest** of the two instants only. Because we never
//!   emit the later instant, and the caller advances past the earliest,
//!   the occurrence fires exactly once.
//!
//! The UTC epoch returned is therefore a stable dedupe key: a given cron
//! occurrence maps to exactly one UTC instant regardless of clock
//! weirdness, which is what the scheduler keys `automation_runs.scheduled_for`
//! on.

use std::collections::BTreeSet;
use std::str::FromStr;

use chrono::{DateTime, Datelike, Duration, MappedLocalTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;

/// How far ahead [`next_occurrence_after`] will scan for a matching day
/// before giving up. Five years comfortably covers the sparsest sane
/// cron (a leap-day `29 2` job recurs at most every 4 years); a cron that
/// can never match (e.g. `0 0 31 2 *` — Feb 31st) correctly yields
/// `None` after the horizon rather than looping forever.
const SCAN_HORIZON_DAYS: i64 = 366 * 5;

/// Maximum minutes we will advance a non-existent (spring-forward gap)
/// wall-clock time looking for the next valid instant. Real DST gaps are
/// ≤120 minutes; 240 is a generous safety bound.
const MAX_GAP_ADVANCE_MINUTES: i64 = 240;

/// A parsed, validated cron schedule. Construct via [`parse_cron`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: CronField,
    hour: CronField,
    day_of_month: CronField,
    month: CronField,
    day_of_week: CronField,
}

/// One parsed cron field: the set of values it matches, plus whether the
/// raw token was a bare `*`. The `is_star` flag is load-bearing for the
/// day-of-month / day-of-week union rule (Vixie cron semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CronField {
    allowed: BTreeSet<u32>,
    is_star: bool,
}

impl CronField {
    fn matches(&self, value: u32) -> bool {
        self.allowed.contains(&value)
    }
}

/// Error parsing a cron expression or timezone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleParseError(pub String);

impl std::fmt::Display for ScheduleParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ScheduleParseError {}

fn err(msg: impl Into<String>) -> ScheduleParseError {
    ScheduleParseError(msg.into())
}

/// Parse and validate a standard 5-field cron expression
/// (`minute hour day-of-month month day-of-week`).
///
/// Supported per field: `*`, single values, ranges (`a-b`), lists
/// (`a,b,c`), and steps (`*/n`, `a-b/n`, `a/n`). Day-of-week accepts
/// `0`–`7` where both `0` and `7` mean Sunday. Month/day-of-week *names*
/// are intentionally not supported in v1 — the CLI presets compile to
/// numeric cron, and raw-cron authors use numbers.
pub fn parse_cron(expr: &str) -> Result<CronSchedule, ScheduleParseError> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(err(format!(
            "cron must have exactly 5 space-separated fields \
             (minute hour day-of-month month day-of-week), got {}: {expr:?}",
            fields.len()
        )));
    }
    Ok(CronSchedule {
        minute: parse_field(fields[0], 0, 59, "minute")?,
        hour: parse_field(fields[1], 0, 23, "hour")?,
        day_of_month: parse_field(fields[2], 1, 31, "day-of-month")?,
        month: parse_field(fields[3], 1, 12, "month")?,
        day_of_week: parse_day_of_week(fields[4])?,
    })
}

/// Validate a full schedule trigger: both the cron expression and the
/// IANA timezone name must parse. Used by the CLI to reject garbage
/// before it reaches the DB, and by the scheduler before computing
/// occurrences.
pub fn validate_schedule(cron: &str, timezone: &str) -> Result<(), ScheduleParseError> {
    parse_cron(cron)?;
    parse_timezone(timezone)?;
    Ok(())
}

/// Parse an IANA timezone name into a [`Tz`].
pub fn parse_timezone(name: &str) -> Result<Tz, ScheduleParseError> {
    Tz::from_str(name).map_err(|e| err(format!("unknown IANA timezone {name:?}: {e}")))
}

fn parse_day_of_week(token: &str) -> Result<CronField, ScheduleParseError> {
    // Parse over 0..=7, then fold 7 → 0 (both are Sunday). `is_star` is
    // preserved from the raw token so the union rule can detect it.
    let mut field = parse_field(token, 0, 7, "day-of-week")?;
    if field.allowed.remove(&7) {
        field.allowed.insert(0);
    }
    Ok(field)
}

fn parse_field(
    token: &str,
    min: u32,
    max: u32,
    label: &str,
) -> Result<CronField, ScheduleParseError> {
    let is_star = token == "*";
    let mut allowed = BTreeSet::new();

    for part in token.split(',') {
        if part.is_empty() {
            return Err(err(format!("empty term in {label} field: {token:?}")));
        }

        // Split optional step: "<range>/<step>".
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| err(format!("invalid step {s:?} in {label} field")))?;
                if step == 0 {
                    return Err(err(format!("step must be ≥1 in {label} field: {part:?}")));
                }
                (r, step)
            }
            None => (part, 1),
        };

        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            (parse_num(a, label)?, parse_num(b, label)?)
        } else {
            let v = parse_num(range_part, label)?;
            (v, v)
        };

        if lo < min || hi > max {
            return Err(err(format!(
                "{label} value out of range {min}..={max}: {range_part:?}"
            )));
        }
        if lo > hi {
            return Err(err(format!(
                "{label} range start {lo} is after end {hi}: {range_part:?}"
            )));
        }

        let mut v = lo;
        while v <= hi {
            allowed.insert(v);
            v += step;
        }
    }

    if allowed.is_empty() {
        return Err(err(format!("{label} field matched no values: {token:?}")));
    }

    Ok(CronField { allowed, is_star })
}

fn parse_num(s: &str, label: &str) -> Result<u32, ScheduleParseError> {
    s.trim()
        .parse()
        .map_err(|_| err(format!("invalid number {s:?} in {label} field")))
}

impl CronSchedule {
    /// Does the calendar `date` match the month + day fields?
    ///
    /// Day matching follows Vixie cron's union rule: when **both**
    /// day-of-month and day-of-week are restricted (neither is `*`), a
    /// day matches if **either** field matches. When only one is
    /// restricted, only that one is consulted; when both are `*`, every
    /// day matches.
    fn matches_day(&self, date: NaiveDate) -> bool {
        if !self.month.matches(date.month()) {
            return false;
        }
        let dom_ok = self.day_of_month.matches(date.day());
        let dow_ok = self
            .day_of_week
            .matches(date.weekday().num_days_from_sunday());

        match (self.day_of_month.is_star, self.day_of_week.is_star) {
            (true, true) => true,
            (false, true) => dom_ok,
            (true, false) => dow_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }
}

/// Compute the earliest occurrence of `schedule` (interpreted in `tz`)
/// strictly after `after_epoch` (UTC seconds). Returns the occurrence as
/// UTC epoch seconds, or `None` if no occurrence exists within
/// [`SCAN_HORIZON_DAYS`] (e.g. an unsatisfiable cron like `0 0 31 2 *`).
///
/// "Strictly after" is what makes consecutive calls walk forward without
/// re-emitting the same instant — the scheduler advances `next_due_at` by
/// feeding the previous occurrence straight back in.
pub fn next_occurrence_after(schedule: &CronSchedule, tz: Tz, after_epoch: i64) -> Option<i64> {
    let after = DateTime::<Utc>::from_timestamp(after_epoch, 0)?;
    // Start scanning from the local calendar date of `after`. A candidate
    // wall time earlier in that date than `after` simply won't pass the
    // strict `> after_epoch` test once resolved to UTC, so starting at the
    // date (not the exact time) is both correct and simple.
    let start_date = after.with_timezone(&tz).date_naive();

    let mut date = start_date;
    for _ in 0..SCAN_HORIZON_DAYS {
        if schedule.matches_day(date) {
            // Collect every matching wall time on this day, resolve each to
            // a UTC instant (handling gap/fold), and take the smallest that
            // is strictly after `after`. Collecting per-day rather than
            // returning the first in naive order is robust to the rare
            // reordering a DST resolution can introduce.
            let mut best: Option<i64> = None;
            for &h in &schedule.hour.allowed {
                for &m in &schedule.minute.allowed {
                    let Some(naive) = date.and_hms_opt(h, m, 0) else {
                        continue;
                    };
                    if let Some(occ) = resolve_local_to_utc(naive, tz)
                        && occ > after_epoch
                        && best.is_none_or(|b| occ < b)
                    {
                        best = Some(occ);
                    }
                }
            }
            if let Some(occ) = best {
                return Some(occ);
            }
        }
        date = date.succ_opt()?;
    }
    None
}

/// Convenience wrapper that parses raw cron + timezone strings (as stored
/// on `AutomationTrigger::Schedule`) and computes the next occurrence.
pub fn next_occurrence_after_str(
    cron: &str,
    timezone: &str,
    after_epoch: i64,
) -> Result<Option<i64>, ScheduleParseError> {
    let schedule = parse_cron(cron)?;
    let tz = parse_timezone(timezone)?;
    Ok(next_occurrence_after(&schedule, tz, after_epoch))
}

/// Resolve a local wall-clock `naive` time in `tz` to a UTC epoch second,
/// applying the DST policy: gap → next valid instant (run later); fold →
/// earliest of the two instants (fire once).
fn resolve_local_to_utc(naive: chrono::NaiveDateTime, tz: Tz) -> Option<i64> {
    match tz.from_local_datetime(&naive) {
        MappedLocalTime::Single(dt) => Some(dt.timestamp()),
        MappedLocalTime::Ambiguous(earliest, _latest) => Some(earliest.timestamp()),
        MappedLocalTime::None => {
            // Spring-forward gap: this wall time does not exist. Advance
            // minute-by-minute to the first instant that does (the gap's
            // far edge), so the job runs once, slightly later.
            let mut candidate = naive;
            for _ in 0..MAX_GAP_ADVANCE_MINUTES {
                candidate += Duration::minutes(1);
                match tz.from_local_datetime(&candidate) {
                    MappedLocalTime::Single(dt) => return Some(dt.timestamp()),
                    MappedLocalTime::Ambiguous(earliest, _) => return Some(earliest.timestamp()),
                    MappedLocalTime::None => continue,
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::America::Los_Angeles;
    use chrono_tz::UTC;

    /// Epoch seconds for a UTC wall time, for readable assertions.
    fn utc_epoch(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp()
    }

    /// Epoch seconds for a local wall time in a given tz (single-resolution
    /// only — used to build unambiguous test anchors).
    fn local_epoch(tz: Tz, y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        tz.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap()
            .timestamp()
    }

    #[test]
    fn rejects_wrong_field_count() {
        assert!(parse_cron("0 14 * *").is_err());
        assert!(parse_cron("0 14 * * * *").is_err());
        assert!(parse_cron("").is_err());
    }

    #[test]
    fn rejects_out_of_range_and_garbage() {
        assert!(parse_cron("60 * * * *").is_err()); // minute 60
        assert!(parse_cron("* 24 * * *").is_err()); // hour 24
        assert!(parse_cron("* * 0 * *").is_err()); // dom 0
        assert!(parse_cron("* * 32 * *").is_err()); // dom 32
        assert!(parse_cron("* * * 13 *").is_err()); // month 13
        assert!(parse_cron("* * * * 8").is_err()); // dow 8
        assert!(parse_cron("*/0 * * * *").is_err()); // zero step
        assert!(parse_cron("abc * * * *").is_err());
        assert!(parse_cron("5-2 * * * *").is_err()); // reversed range
    }

    #[test]
    fn accepts_common_forms() {
        assert!(parse_cron("0 14 * * 1-5").is_ok()); // weekday 2pm
        assert!(parse_cron("*/15 * * * *").is_ok()); // every 15 min
        assert!(parse_cron("0 0 * * 0").is_ok()); // Sunday midnight
        assert!(parse_cron("0 9,17 * * *").is_ok()); // 9am and 5pm
        assert!(parse_cron("0 0 1,15 * *").is_ok()); // 1st and 15th
        assert!(parse_cron("0 0 * * 7").is_ok()); // dow 7 == Sunday
    }

    #[test]
    fn dow_seven_folds_to_sunday() {
        let a = parse_cron("0 0 * * 0").unwrap();
        let b = parse_cron("0 0 * * 7").unwrap();
        assert_eq!(a.day_of_week.allowed, b.day_of_week.allowed);
    }

    #[test]
    fn unknown_timezone_rejected() {
        assert!(parse_timezone("America/Nowhere").is_err());
        assert!(parse_timezone("America/Los_Angeles").is_ok());
        assert!(parse_timezone("UTC").is_ok());
    }

    #[test]
    fn weekday_2pm_next_occurrence_in_utc_zone() {
        let sched = parse_cron("0 14 * * 1-5").unwrap();
        // Thu 2026-05-28 13:00 UTC → next is same day 14:00 UTC.
        let after = utc_epoch(2026, 5, 28, 13, 0);
        let occ = next_occurrence_after(&sched, UTC, after).unwrap();
        assert_eq!(occ, utc_epoch(2026, 5, 28, 14, 0));
    }

    #[test]
    fn weekday_2pm_skips_weekend() {
        let sched = parse_cron("0 14 * * 1-5").unwrap();
        // Fri 2026-05-29 15:00 UTC (after 2pm) → next weekday is Mon 06-01.
        let after = utc_epoch(2026, 5, 29, 15, 0);
        let occ = next_occurrence_after(&sched, UTC, after).unwrap();
        assert_eq!(occ, utc_epoch(2026, 6, 1, 14, 0));
    }

    #[test]
    fn consecutive_calls_walk_forward_no_repeat() {
        let sched = parse_cron("0 * * * *").unwrap(); // hourly
        let mut cursor = utc_epoch(2026, 5, 28, 10, 30);
        let mut seen = Vec::new();
        for _ in 0..5 {
            let occ = next_occurrence_after(&sched, UTC, cursor).unwrap();
            assert!(occ > cursor, "must be strictly after");
            seen.push(occ);
            cursor = occ;
        }
        assert_eq!(
            seen,
            vec![
                utc_epoch(2026, 5, 28, 11, 0),
                utc_epoch(2026, 5, 28, 12, 0),
                utc_epoch(2026, 5, 28, 13, 0),
                utc_epoch(2026, 5, 28, 14, 0),
                utc_epoch(2026, 5, 28, 15, 0),
            ]
        );
    }

    #[test]
    fn two_pm_la_tracks_dst_offset() {
        let sched = parse_cron("0 14 * * *").unwrap();
        // Winter (PST, UTC-8): 2pm local = 22:00 UTC.
        let winter_after = local_epoch(Los_Angeles, 2026, 1, 15, 13, 0);
        let winter = next_occurrence_after(&sched, Los_Angeles, winter_after).unwrap();
        assert_eq!(winter, utc_epoch(2026, 1, 15, 22, 0));

        // Summer (PDT, UTC-7): 2pm local = 21:00 UTC.
        let summer_after = local_epoch(Los_Angeles, 2026, 7, 15, 13, 0);
        let summer = next_occurrence_after(&sched, Los_Angeles, summer_after).unwrap();
        assert_eq!(summer, utc_epoch(2026, 7, 15, 21, 0));
    }

    /// Spring-forward: on 2026-03-08 LA clocks jump 02:00→03:00, so 02:30
    /// does not exist. A `30 2 * * *` job must run once, at the next valid
    /// instant (03:00 PDT = 10:00 UTC), not be skipped.
    #[test]
    fn spring_forward_gap_runs_later() {
        let sched = parse_cron("30 2 * * *").unwrap();
        // Just after midnight local on the transition day.
        let after = local_epoch(Los_Angeles, 2026, 3, 8, 0, 30);
        let occ = next_occurrence_after(&sched, Los_Angeles, after).unwrap();
        // 03:00 PDT on 2026-03-08 = 10:00 UTC.
        assert_eq!(occ, utc_epoch(2026, 3, 8, 10, 0));
    }

    /// Fall-back: on 2026-11-01 LA clocks repeat 01:00–01:59 (02:00 PDT →
    /// 01:00 PST). A `30 1 * * *` job must fire exactly once, on the
    /// earliest (PDT) instant: 01:30 PDT = 08:30 UTC, NOT the later
    /// 01:30 PST = 09:30 UTC.
    #[test]
    fn fall_back_overlap_fires_once_on_earliest() {
        let sched = parse_cron("30 1 * * *").unwrap();
        let after = utc_epoch(2026, 11, 1, 7, 0); // 00:00 PDT on transition day
        let occ = next_occurrence_after(&sched, Los_Angeles, after).unwrap();
        assert_eq!(occ, utc_epoch(2026, 11, 1, 8, 30), "earliest of the two 01:30s");

        // Feeding that occurrence back in must NOT re-emit the second
        // 01:30 (09:30 UTC); the next occurrence is the following day.
        let next = next_occurrence_after(&sched, Los_Angeles, occ).unwrap();
        assert_eq!(next, local_epoch(Los_Angeles, 2026, 11, 2, 1, 30));
        assert_ne!(next, utc_epoch(2026, 11, 1, 9, 30));
    }

    #[test]
    fn dom_dow_union_when_both_restricted() {
        // "1st of the month OR any Monday" — Vixie union semantics.
        let sched = parse_cron("0 0 1 * 1").unwrap();
        // From Fri 2026-05-01 (the 1st) 01:00 UTC: next is Mon 2026-05-04.
        let after = utc_epoch(2026, 5, 1, 1, 0);
        let occ = next_occurrence_after(&sched, UTC, after).unwrap();
        assert_eq!(occ, utc_epoch(2026, 5, 4, 0, 0)); // Monday
        // And the 1st itself matched earlier in the day:
        let before_first = utc_epoch(2026, 4, 30, 23, 0);
        let first = next_occurrence_after(&sched, UTC, before_first).unwrap();
        assert_eq!(first, utc_epoch(2026, 5, 1, 0, 0));
    }

    #[test]
    fn dom_only_restricted_ignores_weekday() {
        // dow is `*`, so only the 15th matters regardless of weekday.
        let sched = parse_cron("0 0 15 * *").unwrap();
        let after = utc_epoch(2026, 5, 1, 0, 0);
        let occ = next_occurrence_after(&sched, UTC, after).unwrap();
        assert_eq!(occ, utc_epoch(2026, 5, 15, 0, 0));
    }

    #[test]
    fn multiple_hours_same_day_picks_earliest_future() {
        let sched = parse_cron("0 9,17 * * *").unwrap();
        let after = utc_epoch(2026, 5, 28, 10, 0); // past 9, before 17
        let occ = next_occurrence_after(&sched, UTC, after).unwrap();
        assert_eq!(occ, utc_epoch(2026, 5, 28, 17, 0));
    }

    #[test]
    fn unsatisfiable_cron_returns_none() {
        // Feb 31st never exists.
        let sched = parse_cron("0 0 31 2 *").unwrap();
        let after = utc_epoch(2026, 1, 1, 0, 0);
        assert_eq!(next_occurrence_after(&sched, UTC, after), None);
    }

    #[test]
    fn leap_day_recurs_across_years() {
        let sched = parse_cron("0 0 29 2 *").unwrap();
        // From 2026 (non-leap) the next Feb 29 is 2028.
        let after = utc_epoch(2026, 3, 1, 0, 0);
        let occ = next_occurrence_after(&sched, UTC, after).unwrap();
        assert_eq!(occ, utc_epoch(2028, 2, 29, 0, 0));
    }

    #[test]
    fn str_wrapper_validates_and_computes() {
        let after = utc_epoch(2026, 5, 28, 13, 0);
        let occ = next_occurrence_after_str("0 14 * * *", "UTC", after)
            .unwrap()
            .unwrap();
        assert_eq!(occ, utc_epoch(2026, 5, 28, 14, 0));
        assert!(next_occurrence_after_str("bogus", "UTC", after).is_err());
        assert!(next_occurrence_after_str("0 14 * * *", "Mars/Phobos", after).is_err());
    }
}
