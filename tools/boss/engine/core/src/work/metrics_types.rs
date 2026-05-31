/// One row pulled from `metrics_counter`. The framework rehydrates
/// these into the in-memory registry on engine start so monotonic
/// totals span restarts.
#[derive(Debug, Clone)]
pub struct MetricsCounterRow {
    pub name: String,
    pub value: u64,
    pub updated_at_ms: i64,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct MetricsGaugeRow {
    pub name: String,
    pub value: i64,
    pub observed_at_ms: i64,
    pub description: String,
}
