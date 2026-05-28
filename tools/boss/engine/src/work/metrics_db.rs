use super::*;

impl WorkDb {
    /// Load every persisted counter and gauge row for the
    /// metrics-framework startup rehydrate. Order is unspecified
    /// (the caller is `metrics::seed_from_db`, which doesn't care).
    pub fn metrics_load_all(&self) -> Result<(Vec<MetricsCounterRow>, Vec<MetricsGaugeRow>)> {
        let conn = self.connect()?;
        let mut counter_stmt =
            conn.prepare("SELECT name, value, updated_at_ms, description FROM metrics_counter")?;
        let counters: Vec<MetricsCounterRow> = counter_stmt
            .query_map([], |row| {
                let value_i64: i64 = row.get(1)?;
                Ok(MetricsCounterRow {
                    name: row.get(0)?,
                    // Counters round-trip as raw bits so monotonic
                    // u64 values above i64::MAX (theoretical only —
                    // see design §"Bounded memory and disk cost")
                    // survive the encode/decode.
                    value: value_i64 as u64,
                    updated_at_ms: row.get(2)?,
                    description: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut gauge_stmt =
            conn.prepare("SELECT name, value, observed_at_ms, description FROM metrics_gauge")?;
        let gauges: Vec<MetricsGaugeRow> = gauge_stmt
            .query_map([], |row| {
                Ok(MetricsGaugeRow {
                    name: row.get(0)?,
                    value: row.get(1)?,
                    observed_at_ms: row.get(2)?,
                    description: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok((counters, gauges))
    }

    /// UPSERT every counter and gauge snapshot in a single
    /// transaction. The flush task calls this every 30s; the
    /// graceful-shutdown path calls it once more before the engine
    /// exits. Rehydrated "stale" rows (whose name no longer matches
    /// any registered handle) are skipped — the existing row stays
    /// in the table untouched so historical answers remain
    /// queryable (design §"Risks / open questions" item 3).
    pub fn metrics_flush(
        &self,
        counters: &[MetricsCounterRow],
        gauges: &[MetricsGaugeRow],
    ) -> Result<()> {
        if counters.is_empty() && gauges.is_empty() {
            return Ok(());
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        for c in counters {
            tx.execute(
                "INSERT INTO metrics_counter (name, value, updated_at_ms, description)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET
                     value = excluded.value,
                     updated_at_ms = excluded.updated_at_ms,
                     description = excluded.description",
                params![c.name, c.value as i64, c.updated_at_ms, c.description],
            )?;
        }
        for g in gauges {
            tx.execute(
                "INSERT INTO metrics_gauge (name, value, observed_at_ms, description)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET
                     value = excluded.value,
                     observed_at_ms = excluded.observed_at_ms,
                     description = excluded.description",
                params![g.name, g.value, g.observed_at_ms, g.description],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Zero one metric (counter or gauge) in `state.db`. Called from
    /// the `MetricsReset` RPC handler after the in-memory atomic is
    /// already cleared. Returns `(counter_cleared, gauge_cleared)` so
    /// the caller can tell the operator which kind was found.
    pub fn metrics_reset_one(&self, name: &str, now_ms: i64) -> Result<(bool, bool)> {
        let conn = self.connect()?;
        let counter_rows = conn.execute(
            "UPDATE metrics_counter SET value = 0, updated_at_ms = ?2 WHERE name = ?1",
            params![name, now_ms],
        )?;
        let gauge_rows = conn.execute(
            "UPDATE metrics_gauge SET value = 0, observed_at_ms = ?2 WHERE name = ?1",
            params![name, now_ms],
        )?;
        Ok((counter_rows > 0, gauge_rows > 0))
    }

    /// Zero every counter and gauge row in `state.db`. Called from
    /// the `MetricsReset { name: None }` path (reset --all). Returns
    /// `(counters_cleared, gauges_cleared)`.
    pub fn metrics_reset_all(&self, now_ms: i64) -> Result<(usize, usize)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let counter_rows = tx.execute(
            "UPDATE metrics_counter SET value = 0, updated_at_ms = ?1",
            params![now_ms],
        )?;
        let gauge_rows = tx.execute(
            "UPDATE metrics_gauge SET value = 0, observed_at_ms = ?1",
            params![now_ms],
        )?;
        tx.commit()?;
        Ok((counter_rows, gauge_rows))
    }
}
