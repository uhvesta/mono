//! Background monitor that detects when macOS's `syspolicyd` daemon is
//! pinned at ~100% CPU and surfaces a user-facing health alert.
//!
//! ## The fault this guards against
//!
//! `syspolicyd` is the system code-signing / Gatekeeper assessment
//! daemon. Boss runs many concurrent workers that rapidly launch
//! freshly-built, ad-hoc-signed binaries (engine, worker wrappers, test
//! binaries). That floods first-launch Gatekeeper assessment, and under
//! that load `syspolicyd` can wedge in a ~100% CPU spin. While it is
//! stuck it stops servicing assessment requests, so every `dlopen` of a
//! signature-checked dylib blocks in the kernel `fcntl(F_ADDFILESIGS)`
//! call. The visible result is a *machine-wide* build outage: every
//! Bazel server hangs at JVM startup ("Starting local Bazel server…
//! still trying to connect", then exit 37 "Server crashed during
//! startup") with an otherwise-empty `jvm.out`.
//!
//! This is not a Boss bug — the fault is in `syspolicyd` — but Boss's
//! workload reliably triggers it, and the failure is silent and easy to
//! misdiagnose (operators expunge Bazel caches and restart daemons,
//! none of which help, because the bottleneck is the system daemon).
//! See issue #965.
//!
//! ## What this module does
//!
//! It samples `syspolicyd`'s CPU usage on a fixed cadence and, once the
//! daemon has been saturated (≥ [`SATURATION_CPU_PCT`]) for
//! [`SATURATION_SAMPLES_TO_ALERT`] consecutive samples, flips a shared
//! [`SyspolicydHealth`] flag. [`crate::app::build_engine_health_report`]
//! reads that flag and emits an `EngineHealthIssue` so the macOS app
//! raises a banner naming the cause and the remedy.
//!
//! ## Why "sustained" rather than a single spike
//!
//! A healthy first-launch assessment burst can momentarily push
//! `syspolicyd` high, so a single sample over threshold is not enough to
//! distinguish the transient case from the wedge. Requiring several
//! consecutive saturated samples (~30s at the default cadence) matches
//! the "sustained at ~100% for more than a few seconds" signal from the
//! incident report while keeping the false-positive rate negligible.
//!
//! ## Detection only, no mitigation
//!
//! Recovery requires `sudo kill -9 <pid>` (SIP blocks
//! `launchctl kickstart` of the daemon), which the engine cannot and
//! must not attempt unattended. This module deliberately only *detects*
//! and *surfaces* — the remedy is left to the operator via the alert
//! body.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// CPU% at or above which `syspolicyd` is considered "saturated". The
/// wedge pins it at ~100%; 90% leaves headroom for sampling jitter while
/// staying well clear of the few-percent baseline a healthy daemon shows.
pub const SATURATION_CPU_PCT: f64 = 90.0;

/// Consecutive saturated samples required before the daemon is declared
/// wedged (and the health alert is raised). At [`DEFAULT_SAMPLE_INTERVAL`]
/// this is ~30s sustained — long enough to rule out a brief first-launch
/// assessment spike, short enough to alert well before an operator has
/// finished misdiagnosing it as "Bazel is broken".
pub const SATURATION_SAMPLES_TO_ALERT: u32 = 3;

/// Default sampling cadence. One cheap `ps` exec per tick.
pub const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// A single CPU observation of the `syspolicyd` process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyspolicydSample {
    pub pid: i32,
    pub cpu_pct: f64,
}

/// Snapshot of the monitor's current verdict. Read by the engine health
/// report builder; cheap to clone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyspolicydStatus {
    /// True once `syspolicyd` has been saturated for
    /// [`SATURATION_SAMPLES_TO_ALERT`] consecutive samples.
    pub wedged: bool,
    /// Pid of the `syspolicyd` process from the most recent sample, if it
    /// was found.
    pub pid: Option<i32>,
    /// CPU% from the most recent sample (0.0 when the daemon was absent).
    pub cpu_pct: f64,
    /// Epoch seconds when the current saturated run began (the first of
    /// the consecutive saturated samples). `None` when not saturated.
    pub saturated_since_epoch: Option<i64>,
}

/// Shared, interior-mutable monitor state. The sampler loop calls
/// [`SyspolicydHealth::record_sample`] / [`SyspolicydHealth::record_absent`];
/// the health report builder reads [`SyspolicydHealth::snapshot`].
#[derive(Debug, Default)]
pub struct SyspolicydHealth {
    inner: StdMutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    consecutive_saturated: u32,
    status: SyspolicydStatus,
}

impl SyspolicydHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current verdict. Cheap clone behind a short-held lock.
    pub fn snapshot(&self) -> SyspolicydStatus {
        self.inner.lock().expect("syspolicyd health poisoned").status.clone()
    }

    /// Feed one CPU sample, stamping the saturated-run start with
    /// `now_epoch`. Returns the post-update status. Kept independent of
    /// the sampling mechanism so tests drive verdicts directly without
    /// shelling out.
    pub fn record_sample(&self, sample: SyspolicydSample, now_epoch: i64) -> SyspolicydStatus {
        let mut inner = self.inner.lock().expect("syspolicyd health poisoned");
        if sample.cpu_pct >= SATURATION_CPU_PCT {
            inner.consecutive_saturated = inner.consecutive_saturated.saturating_add(1);
            if inner.status.saturated_since_epoch.is_none() {
                inner.status.saturated_since_epoch = Some(now_epoch);
            }
            if inner.consecutive_saturated >= SATURATION_SAMPLES_TO_ALERT {
                inner.status.wedged = true;
            }
        } else {
            // A single below-threshold sample clears the run: the daemon
            // is servicing requests again, so any in-progress build
            // stall is over.
            inner.consecutive_saturated = 0;
            inner.status.wedged = false;
            inner.status.saturated_since_epoch = None;
        }
        inner.status.pid = Some(sample.pid);
        inner.status.cpu_pct = sample.cpu_pct;
        inner.status.clone()
    }

    /// Record that `syspolicyd` was not found (e.g. not running, or this
    /// is not macOS). Treated as "not saturated": clears any in-progress
    /// run so a transient sampling miss cannot strand the wedged flag.
    pub fn record_absent(&self) {
        let mut inner = self.inner.lock().expect("syspolicyd health poisoned");
        inner.consecutive_saturated = 0;
        inner.status = SyspolicydStatus::default();
    }
}

/// Parse `ps -axo pid=,%cpu=,comm=` output, returning the `syspolicyd`
/// sample if present. Public so the parser is unit-tested without
/// shelling out.
///
/// `comm` (the executable path) is the last column; we require it to end
/// in `syspolicyd` so a worker whose command line merely *mentions*
/// syspolicyd cannot masquerade as the daemon.
pub fn parse_syspolicyd_sample(ps_output: &str) -> Option<SyspolicydSample> {
    for line in ps_output.lines() {
        let line = line.trim();
        if !line.contains("syspolicyd") {
            continue;
        }
        let mut fields = line.split_whitespace();
        let pid = fields.next()?.parse::<i32>().ok()?;
        let cpu_pct = fields.next()?.parse::<f64>().ok()?;
        let comm = fields.next().unwrap_or("");
        // `/usr/libexec/syspolicyd` (the real daemon) ends in the name;
        // an unrelated process whose args happened to contain the string
        // does not.
        if !comm.ends_with("syspolicyd") {
            continue;
        }
        return Some(SyspolicydSample { pid, cpu_pct });
    }
    None
}

/// Sample `syspolicyd`'s CPU via `ps`. Returns `None` when the daemon is
/// absent or `ps` fails — both treated by the loop as "not saturated".
/// Uses `ps` for the same reason [`crate::main`]'s `parent_command_line`
/// does: it is reliably available on macOS and pulls in no extra crate.
async fn sample_via_ps() -> Option<SyspolicydSample> {
    let output = tokio::process::Command::new("ps")
        .args(["-axo", "pid=,%cpu=,comm="])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_syspolicyd_sample(&stdout)
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Spawn a tokio task that samples `syspolicyd` forever at `interval`,
/// updating `health`. Logs once on each transition into the wedged state
/// (not every tick) so the engine log carries the signature without
/// flooding while the wedge persists.
pub fn spawn_loop(health: Arc<SyspolicydHealth>, interval: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut was_wedged = false;
        loop {
            match sample_via_ps().await {
                Some(sample) => {
                    let status = health.record_sample(sample, now_epoch_secs());
                    if status.wedged && !was_wedged {
                        tracing::error!(
                            pid = status.pid,
                            cpu_pct = status.cpu_pct,
                            "syspolicyd is pinned at ~100% CPU and wedged — all code-signing \
                             (and therefore all Bazel builds) is stalled machine-wide. Remedy: \
                             `sudo kill -9 <pid>` (launchd relaunches a fresh syspolicyd; SIP \
                             blocks launchctl kickstart), or reboot. Killing/expunging Bazel \
                             will NOT help.",
                        );
                    } else if !status.wedged && was_wedged {
                        tracing::info!("syspolicyd CPU recovered below saturation threshold");
                    }
                    was_wedged = status.wedged;
                }
                None => {
                    health.record_absent();
                    was_wedged = false;
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_syspolicyd_line() {
        let out = "\
  101  0.1 /sbin/launchd
  520 99.4 /usr/libexec/syspolicyd
  733  2.3 /usr/libexec/trustd
";
        let sample = parse_syspolicyd_sample(out).expect("syspolicyd row present");
        assert_eq!(sample.pid, 520);
        assert!((sample.cpu_pct - 99.4).abs() < 1e-9);
    }

    #[test]
    fn returns_none_when_syspolicyd_absent() {
        let out = "  101  0.1 /sbin/launchd\n  733  2.3 /usr/libexec/trustd\n";
        assert!(parse_syspolicyd_sample(out).is_none());
    }

    #[test]
    fn ignores_process_merely_mentioning_syspolicyd() {
        // A worker tailing syspolicyd logs must not be mistaken for the
        // daemon: its `comm` does not end in `syspolicyd`.
        let out = "  900 12.0 /usr/bin/log stream --predicate process==syspolicyd\n";
        assert!(parse_syspolicyd_sample(out).is_none());
    }

    #[test]
    fn does_not_alert_below_threshold() {
        let health = SyspolicydHealth::new();
        for i in 0..10 {
            let status = health.record_sample(SyspolicydSample { pid: 1, cpu_pct: 12.0 }, i);
            assert!(!status.wedged);
            assert_eq!(status.saturated_since_epoch, None);
        }
    }

    #[test]
    fn alerts_only_after_sustained_saturation() {
        let health = SyspolicydHealth::new();
        // First (SATURATION_SAMPLES_TO_ALERT - 1) saturated samples must
        // NOT trip the alert, but must start tracking the run.
        for i in 0..(SATURATION_SAMPLES_TO_ALERT - 1) {
            let status = health.record_sample(
                SyspolicydSample {
                    pid: 520,
                    cpu_pct: 99.0,
                },
                1000 + i as i64,
            );
            assert!(!status.wedged, "must not alert before sustained threshold");
            assert_eq!(status.saturated_since_epoch, Some(1000));
        }
        // The Nth consecutive saturated sample flips wedged.
        let status = health.record_sample(
            SyspolicydSample {
                pid: 520,
                cpu_pct: 100.0,
            },
            2000,
        );
        assert!(status.wedged, "must alert once sustained");
        assert_eq!(status.pid, Some(520));
        // Run-start timestamp is the FIRST saturated sample, not the one
        // that crossed the alert count.
        assert_eq!(status.saturated_since_epoch, Some(1000));
    }

    #[test]
    fn recovery_clears_wedged_flag() {
        let health = SyspolicydHealth::new();
        for i in 0..SATURATION_SAMPLES_TO_ALERT {
            health.record_sample(
                SyspolicydSample {
                    pid: 520,
                    cpu_pct: 99.0,
                },
                i as i64,
            );
        }
        assert!(health.snapshot().wedged);

        // A single below-threshold sample clears the wedge.
        let status = health.record_sample(SyspolicydSample { pid: 520, cpu_pct: 5.0 }, 999);
        assert!(!status.wedged);
        assert_eq!(status.saturated_since_epoch, None);
        assert!(!health.snapshot().wedged);
    }

    #[test]
    fn saturation_run_must_be_consecutive() {
        let health = SyspolicydHealth::new();
        health.record_sample(
            SyspolicydSample {
                pid: 520,
                cpu_pct: 99.0,
            },
            1,
        );
        health.record_sample(
            SyspolicydSample {
                pid: 520,
                cpu_pct: 99.0,
            },
            2,
        );
        // Dip below threshold resets the counter…
        health.record_sample(
            SyspolicydSample {
                pid: 520,
                cpu_pct: 10.0,
            },
            3,
        );
        // …so this fresh saturated sample is only the first of a new run.
        let status = health.record_sample(
            SyspolicydSample {
                pid: 520,
                cpu_pct: 99.0,
            },
            4,
        );
        assert!(!status.wedged);
        assert_eq!(status.saturated_since_epoch, Some(4));
    }

    #[test]
    fn absent_clears_state() {
        let health = SyspolicydHealth::new();
        for i in 0..SATURATION_SAMPLES_TO_ALERT {
            health.record_sample(
                SyspolicydSample {
                    pid: 520,
                    cpu_pct: 99.0,
                },
                i as i64,
            );
        }
        assert!(health.snapshot().wedged);
        health.record_absent();
        let status = health.snapshot();
        assert!(!status.wedged);
        assert_eq!(status.pid, None);
        assert_eq!(status.cpu_pct, 0.0);
        assert_eq!(status.saturated_since_epoch, None);
    }
}
