use super::*;

pub(crate) fn migrate_work_executions_v3(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "cube_workspace_id",
            "ALTER TABLE work_executions ADD COLUMN cube_workspace_id TEXT",
        ),
        (
            "priority",
            "ALTER TABLE work_executions ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "preferred_workspace_id",
            "ALTER TABLE work_executions ADD COLUMN preferred_workspace_id TEXT",
        ),
    ] {
        if !work_executions_has_column(conn, column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

pub(crate) fn migrate_work_executions_pre_start_retry(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "pre_start_failure_count",
            "ALTER TABLE work_executions ADD COLUMN pre_start_failure_count INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "dispatch_not_before",
            "ALTER TABLE work_executions ADD COLUMN dispatch_not_before TEXT",
        ),
    ] {
        if !work_executions_has_column(conn, column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

pub(crate) fn migrate_work_executions_pr_url(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_url")? {
        conn.execute("ALTER TABLE work_executions ADD COLUMN pr_url TEXT", [])?;
    }
    Ok(())
}

/// `pr_head_before`: the head SHA of the chore's bound PR captured
/// at the moment this execution started running. The Stop boundary's
/// SHA-delta gate uses it to decide whether a resume run actually
/// contributed to the bound PR before falling through to the
/// `PROBE_NO_PR` nudge — see the resume-bounce nudge-loop fix.
/// Idempotent.
pub(crate) fn migrate_work_executions_pr_head_before(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "pr_head_before")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN pr_head_before TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Add `tasks.parent_task_id` — the soft FK that ties a `revision` task
/// to the task whose PR it targets — and the accompanying index so the
/// coordinator can walk the chain efficiently. Mirrors the
/// `migrate_tasks_investigation_doc_columns` pattern: `table_has_column`
/// guard makes this idempotent across re-opens. No CHECK constraint; the
/// "kind = revision ⇒ parent_task_id IS NOT NULL" invariant is enforced
/// in `insert_revision_in_tx` (Phase 2). Existing non-revision rows default
/// to `NULL` with no backfill — that is the correct value for them.
pub(crate) fn migrate_tasks_parent_task_id_column(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "parent_task_id")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN parent_task_id TEXT", [])?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_tasks_parent_task_id
             ON tasks(parent_task_id);",
    )?;
    Ok(())
}

/// Backfill revision `name` to first-line-of-description for existing rows.
///
/// The original `insert_revision_in_tx` stored the full description in both
/// `name` and `description` (see revision-tasks.md implementation). This
/// caused the macOS kanban card to display the entire multi-paragraph
/// description verbatim. The corrected insert now uses `revision_name_from_description`
/// (first non-empty line, ≤120 chars) as `name`; this migration aligns
/// pre-fix rows that still carry `name = description`.
///
/// SQLite's INSTR + SUBSTR extract the first `\n`-terminated segment.
/// Rows where `name` already differs from `description` (e.g. manually
/// patched) are left as-is. Idempotent.
pub(crate) fn migrate_revision_names_to_first_line(conn: &Connection) -> Result<()> {
    // Pull all revision task IDs + descriptions where name = description.
    // We do the first-line extraction in Rust (not raw SQL) because
    // SQLite's string functions cannot reliably handle Unicode ellipsis
    // or word-boundary truncation.
    struct Row {
        id: String,
        description: String,
    }
    let mut stmt = conn.prepare(
        "SELECT id, description FROM tasks
         WHERE kind = 'revision' AND name = description AND deleted_at IS NULL",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            Ok(Row {
                id: row.get(0)?,
                description: row.get(1)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    for row in rows {
        let name = revision_name_from_description(&row.description);
        if name != row.description {
            conn.execute(
                "UPDATE tasks SET name = ?1 WHERE id = ?2",
                rusqlite::params![name, row.id],
            )?;
        }
    }
    Ok(())
}

/// Add `work_executions.prefer_is_soft` — a boolean signal (stored as
/// INTEGER 0/1 per SQLite convention) that tells the coordinator's
/// `lease_workspace_with_fallback` to treat `preferred_workspace_id` as a
/// warmth hint rather than a hard requirement. Set `true` (1) for
/// `revision_implementation` executions; defaults to `false` (0) for all
/// existing rows, preserving the hard-prefer semantics used by orphan-resume.
/// See design § OQ5 and `revision-tasks.md`.
pub(crate) fn migrate_work_executions_prefer_is_soft(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "prefer_is_soft")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN prefer_is_soft INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// `transient_failure_count`: how many times the engine has auto-resumed
/// this work item's execution chain because a worker stalled or died on
/// a transient Claude API error. Carried forward onto each fresh resume
/// execution by [`WorkDb::request_resume_execution`] so the bounded-retry
/// policy in [`crate::transient_recovery`] can cap retries and back off.
/// Idempotent; existing rows default to 0.
pub(crate) fn migrate_work_executions_transient_failure_count(conn: &Connection) -> Result<()> {
    if !work_executions_has_column(conn, "transient_failure_count")? {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN transient_failure_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

/// Canonicalize all timestamp columns to Unix epoch seconds (decimal
/// string). Older rows in some databases hold ISO 8601 strings (e.g.
/// `2026-05-07T18:55:45.000Z`) from a pre-canonical write path; this
/// rewrites them in-place so consumers — `boss chore list --json`,
/// the macOS app's Done-lane bucketing, and any future SQL ordering —
/// see one shape. Idempotent: rows already in epoch form are skipped
/// by the LIKE filter.
pub(crate) fn migrate_timestamps_to_epoch(conn: &Connection) -> Result<()> {
    const TIMESTAMP_COLUMNS: &[(&str, &str)] = &[
        ("products", "created_at"),
        ("products", "updated_at"),
        ("projects", "created_at"),
        ("projects", "updated_at"),
        ("tasks", "created_at"),
        ("tasks", "updated_at"),
        ("tasks", "deleted_at"),
        ("work_executions", "created_at"),
        ("work_executions", "started_at"),
        ("work_executions", "finished_at"),
        ("work_runs", "created_at"),
        ("work_runs", "started_at"),
        ("work_runs", "finished_at"),
        ("work_attention_items", "created_at"),
        ("work_attention_items", "resolved_at"),
        ("pane_summaries", "created_at"),
    ];
    for (table, column) in TIMESTAMP_COLUMNS {
        // SQLite LIKE: `_` matches any single character, so this picks
        // up `YYYY-MM-DD`-prefixed values without parsing every row.
        let select_sql = format!(
            "SELECT rowid, {column} FROM {table} \
             WHERE {column} LIKE '____-__-__T%' OR {column} LIKE '____-__-__ %'"
        );
        let mut stmt = conn.prepare(&select_sql)?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (rowid, value) in rows {
            if let Some(epoch) = parse_iso8601_to_epoch(&value) {
                let update_sql = format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2");
                conn.execute(&update_sql, params![epoch.to_string(), rowid])?;
            }
        }
    }
    Ok(())
}

/// Parse an ISO 8601 / RFC 3339 UTC timestamp like
/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` into Unix epoch seconds. Returns
/// `None` for any other shape (already-canonical numeric strings,
/// non-UTC offsets, malformed values) so the caller can leave them
/// alone.
pub(crate) fn parse_iso8601_to_epoch(value: &str) -> Option<i64> {
    let s = value.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    if !s.ends_with('Z') {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64)
}

/// Days from the Unix epoch (1970-01-01) for a (year, month, day)
/// triple. Howard Hinnant's `days_from_civil`; see
/// https://howardhinnant.github.io/date_algorithms.html.
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let m = month as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

pub(crate) fn work_executions_has_column(conn: &Connection, column: &str) -> Result<bool> {
    table_has_column(conn, "work_executions", column)
}

pub(crate) fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}
