//! Persistence bridge between the in-memory [`Registry`] and the
//! `metrics_counter` / `metrics_gauge` tables in `state.db`.
//!
//! - [`seed_from_db`] is called once on engine startup, after
//!   `metrics::init_all` has registered every handle.
//! - [`spawn_flush_task`] runs every 30 seconds and upserts every
//!   registered counter / gauge snapshot in a single transaction.
//! - [`flush_all`] is called from the graceful-shutdown path so the
//!   last 0–30 s of increments survive a normal exit. Crash-loss is
//!   bounded to the flush interval — acceptable for monotonic counts
//!   (see design §"Persistence: state.db table").

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::task::JoinHandle;

use crate::work::{MetricsCounterRow, MetricsGaugeRow, WorkDb};

use super::registry::{Registry, now_ms};

/// How often the periodic flush task wakes up and snapshots the
/// registry into `state.db`. Picked for the
/// "did the reconstruction path fire?" use case: a 30 s window means
/// at most ~30 s of increments are lost on crash, and the cost is
/// one transaction every 30 s — negligible against the engine's
/// existing write traffic.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Read every persisted counter / gauge row and seed the in-memory
/// registry. Rows whose name matches a registered handle update
/// that handle's value in place; rows without a matching handle are
/// inserted as "stale" so a future `bossctl metrics list` can still
/// see them (design §"Risks / open questions" item 3).
///
/// Call after `metrics::init_all` so every binary-known handle is
/// registered before the rehydrate decides what counts as stale.
pub fn seed_from_db(registry: &Registry, work_db: &WorkDb) -> Result<()> {
    let (counters, gauges) = work_db.metrics_load_all()?;
    for row in counters {
        if !registry.seed_counter(&row.name, row.value, row.updated_at_ms) {
            registry.insert_stale_counter(
                &row.name,
                &row.description,
                row.value,
                row.updated_at_ms,
            );
        }
    }
    for row in gauges {
        if !registry.seed_gauge(&row.name, row.value, row.observed_at_ms) {
            registry.insert_stale_gauge(
                &row.name,
                &row.description,
                row.value,
                row.observed_at_ms,
            );
        }
    }
    Ok(())
}

/// Spawn the periodic flush task on the current tokio runtime. The
/// returned `JoinHandle` is held by `ServerState` until shutdown so
/// the task is bound to the engine's lifetime.
pub fn spawn_flush_task(registry: Arc<Registry>, work_db: Arc<WorkDb>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        // `Skip` keeps the task from firing many catch-up flushes
        // after a stall (e.g. machine sleep) — a single fresh flush
        // is what we want on resume.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick fires immediately; skip it so we don't
        // double-flush right after startup (seed-from-db already
        // ran).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(err) = flush_all(&registry, &work_db) {
                tracing::warn!(?err, "metrics flush failed; will retry on next tick");
            }
        }
    })
}

/// Snapshot every registered counter / gauge and upsert into
/// `state.db`. Stale rows (rehydrated rows whose name no longer
/// matches a registered handle) are skipped so we don't rewrite
/// them on every flush; the persisted row stays untouched.
pub fn flush_all(registry: &Registry, work_db: &WorkDb) -> Result<()> {
    let counter_snaps = registry.counter_snapshots();
    let gauge_snaps = registry.gauge_snapshots();
    let now = now_ms();

    let counters: Vec<MetricsCounterRow> = counter_snaps
        .into_iter()
        .filter(|s| !s.stale)
        .map(|s| MetricsCounterRow {
            name: s.name,
            value: s.value,
            // Use the in-memory `updated_at_ms` so the persisted
            // timestamp tracks the most recent increment, not the
            // wall-clock at flush time. Falls back to `now` if the
            // counter has never been touched since seed.
            updated_at_ms: if s.updated_at_ms == 0 { now } else { s.updated_at_ms },
            description: s.description,
        })
        .collect();

    let gauges: Vec<MetricsGaugeRow> = gauge_snaps
        .into_iter()
        .filter(|s| !s.stale)
        .map(|s| MetricsGaugeRow {
            name: s.name,
            value: s.value,
            observed_at_ms: if s.observed_at_ms == 0 { now } else { s.observed_at_ms },
            description: s.description,
        })
        .collect();

    work_db.metrics_flush(&counters, &gauges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::registry::CounterHandle;
    use crate::register_counter;
    use crate::register_gauge;
    use std::path::PathBuf;

    register_counter!(
        TEST_PERSIST_COUNTER,
        "test_persist.counter",
        "Counter used by persistence round-trip tests."
    );
    register_gauge!(
        TEST_PERSIST_GAUGE,
        "test_persist.gauge",
        "Gauge used by persistence round-trip tests."
    );

    fn open_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    #[test]
    fn round_trip_counter_value_across_simulated_restart() {
        let db = open_db();

        // First "engine boot": register, increment, flush.
        let registry_one = Registry::new();
        registry_one.register_counter(&TEST_PERSIST_COUNTER);
        TEST_PERSIST_COUNTER.inc_by(&registry_one, 5);
        flush_all(&registry_one, &db).expect("flush 1");
        drop(registry_one);

        // Second "engine boot": fresh registry, seed from db, value
        // must come back as 5.
        let registry_two = Registry::new();
        registry_two.register_counter(&TEST_PERSIST_COUNTER);
        seed_from_db(&registry_two, &db).expect("seed");
        assert_eq!(registry_two.counter_value("test_persist.counter"), Some(5));

        // Additional increments accumulate on top of the seeded
        // value.
        TEST_PERSIST_COUNTER.inc_by(&registry_two, 7);
        assert_eq!(registry_two.counter_value("test_persist.counter"), Some(12));

        flush_all(&registry_two, &db).expect("flush 2");
        let (counters, _) = db.metrics_load_all().expect("load");
        let row = counters
            .iter()
            .find(|r| r.name == "test_persist.counter")
            .expect("counter row persisted");
        assert_eq!(row.value, 12);
        assert_eq!(row.description, "Counter used by persistence round-trip tests.");
    }

    #[test]
    fn round_trip_gauge_value_across_simulated_restart() {
        let db = open_db();

        let registry_one = Registry::new();
        registry_one.register_gauge(&TEST_PERSIST_GAUGE);
        TEST_PERSIST_GAUGE.set(&registry_one, 999);
        flush_all(&registry_one, &db).expect("flush 1");

        let registry_two = Registry::new();
        registry_two.register_gauge(&TEST_PERSIST_GAUGE);
        seed_from_db(&registry_two, &db).expect("seed");
        assert_eq!(registry_two.gauge_value("test_persist.gauge"), Some(999));
    }

    #[test]
    fn unknown_persisted_row_is_kept_as_stale_not_dropped() {
        let db = open_db();

        // Simulate a previous engine version's counter that no
        // longer matches any registered handle.
        let registry_one = Registry::new();
        static OLD_HANDLE: CounterHandle = CounterHandle::new(
            "test_persist.removed_counter",
            "an old counter from a previous engine version",
        );
        registry_one.register_counter(&OLD_HANDLE);
        OLD_HANDLE.inc_by(&registry_one, 42);
        flush_all(&registry_one, &db).expect("flush 1");

        // New engine boot: no register_counter call for the old
        // name. The row must come back as stale.
        let registry_two = Registry::new();
        seed_from_db(&registry_two, &db).expect("seed");
        let snaps = registry_two.counter_snapshots();
        let stale = snaps
            .iter()
            .find(|s| s.name == "test_persist.removed_counter")
            .expect("stale row should be retained");
        assert!(stale.stale, "rehydrated unknown counter should be marked stale");
        assert_eq!(stale.value, 42);

        // And subsequent flushes must not drop it from the table —
        // the row stays.
        flush_all(&registry_two, &db).expect("flush 2");
        let (counters, _) = db.metrics_load_all().expect("load");
        assert!(
            counters
                .iter()
                .any(|r| r.name == "test_persist.removed_counter"),
            "stale row must survive subsequent flushes",
        );
    }

    #[test]
    fn stale_row_is_adopted_after_handle_is_added_back() {
        let db = open_db();

        // Persist a counter under a name first.
        let registry_one = Registry::new();
        registry_one.register_counter(&TEST_PERSIST_COUNTER);
        TEST_PERSIST_COUNTER.inc_by(&registry_one, 4);
        flush_all(&registry_one, &db).expect("flush 1");

        // Boot 2: registry is fresh, do NOT register first —
        // simulate the cold path where the rehydrate runs before
        // init_all. The row is stale at first…
        let registry_two = Registry::new();
        seed_from_db(&registry_two, &db).expect("seed");
        assert!(
            registry_two
                .counter_snapshots()
                .iter()
                .find(|s| s.name == "test_persist.counter")
                .map(|s| s.stale)
                .unwrap_or(false),
            "should be stale before registration"
        );

        // …then registration adopts it without losing the value.
        registry_two.register_counter(&TEST_PERSIST_COUNTER);
        let snap = registry_two
            .counter_snapshots()
            .into_iter()
            .find(|s| s.name == "test_persist.counter")
            .expect("entry");
        assert!(!snap.stale);
        assert_eq!(snap.value, 4);
    }

    #[test]
    fn flush_is_a_no_op_when_registry_is_empty() {
        let db = open_db();
        let registry = Registry::new();
        flush_all(&registry, &db).expect("flush no-op");
        let (counters, gauges) = db.metrics_load_all().expect("load");
        assert!(counters.is_empty());
        assert!(gauges.is_empty());
    }

    #[test]
    fn metrics_reset_one_zeros_counter_row_in_db() {
        let db = open_db();
        let registry = Registry::new();
        registry.register_counter(&TEST_PERSIST_COUNTER);
        TEST_PERSIST_COUNTER.inc_by(&registry, 20);
        flush_all(&registry, &db).expect("flush");

        db.metrics_reset_one("test_persist.counter", 9999).expect("reset one");
        let (counters, _) = db.metrics_load_all().expect("load");
        let row = counters.iter().find(|r| r.name == "test_persist.counter").unwrap();
        assert_eq!(row.value, 0);
        assert_eq!(row.updated_at_ms, 9999);
    }

    #[test]
    fn metrics_reset_one_returns_false_for_unknown_name() {
        let db = open_db();
        let (c, g) = db.metrics_reset_one("does.not.exist", 1234).expect("reset");
        assert!(!c);
        assert!(!g);
    }

    #[test]
    fn metrics_reset_all_zeros_every_row() {
        let db = open_db();
        let registry = Registry::new();
        registry.register_counter(&TEST_PERSIST_COUNTER);
        registry.register_gauge(&TEST_PERSIST_GAUGE);
        TEST_PERSIST_COUNTER.inc_by(&registry, 5);
        TEST_PERSIST_GAUGE.set(&registry, 77);
        flush_all(&registry, &db).expect("flush");

        let (counter_count, gauge_count) =
            db.metrics_reset_all(8888).expect("reset all");
        assert_eq!(counter_count, 1);
        assert_eq!(gauge_count, 1);

        let (counters, gauges) = db.metrics_load_all().expect("load");
        assert_eq!(counters[0].value, 0);
        assert_eq!(gauges[0].value, 0);
    }
}
