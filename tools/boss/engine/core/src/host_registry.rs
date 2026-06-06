/// Host registry: tables, types, migration helpers, and `WorkDb` methods.
///
/// Phase 1 of the distributed-agent-execution design. Adds `hosts`,
/// `host_capabilities`, and `work_capability_requirements` tables plus
/// new columns on `work_executions`. No scheduler change; everything
/// still runs locally. Auto-discovers capabilities for the `local` host
/// on every engine startup via `uname` + `gh auth status`.
use std::collections::{BTreeSet, HashMap};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::work::WorkDb;

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Host {
    pub id: String,
    pub ssh_target: Option<String>,
    pub pool_size: i64,
    pub enabled: bool,
    pub last_seen_at: Option<String>,
    pub last_error_text: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct HostCapability {
    pub host_id: String,
    pub capability: String,
    pub source: String,
}

// ── Migration helpers (called from WorkDb::init) ──────────────────────────────

pub(crate) fn migrate_host_registry_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hosts (
             id             TEXT PRIMARY KEY,
             ssh_target     TEXT,
             pool_size      INTEGER NOT NULL DEFAULT 1,
             enabled        INTEGER NOT NULL DEFAULT 1,
             last_seen_at   TEXT,
             last_error_text TEXT,
             created_at     TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS host_capabilities (
             host_id    TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
             capability TEXT NOT NULL,
             source     TEXT NOT NULL,
             PRIMARY KEY (host_id, capability)
         );
         CREATE TABLE IF NOT EXISTS work_capability_requirements (
             subject_kind TEXT NOT NULL,
             subject_id   TEXT NOT NULL,
             capability   TEXT NOT NULL,
             PRIMARY KEY (subject_kind, subject_id, capability)
         );",
    )?;
    Ok(())
}

pub(crate) fn migrate_work_executions_host_columns(conn: &Connection) -> Result<()> {
    let cols = pragma_columns(conn, "work_executions")?;
    if !cols.contains(&"pinned_host_id".to_owned()) {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN pinned_host_id TEXT",
            [],
        )?;
    }
    if !cols.contains(&"host_id".to_owned()) {
        conn.execute(
            "ALTER TABLE work_executions ADD COLUMN host_id TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Add the Phase 3 host attribution columns to `work_runs`. Per the
/// design's "Storage Additions": `host_id` defaults to `'local'` so
/// the existing local-only deployment is unaffected by the migration;
/// `cube_workspace_id` and `remote_pid` are NULL for legacy rows and
/// populated for new runs (the cube workspace id pair `(host_id,
/// cube_workspace_id)` is the durable identity per Q8 of the design;
/// `remote_pid` is the addressing key for Phase 4 signal delivery).
pub(crate) fn migrate_work_runs_host_columns(conn: &Connection) -> Result<()> {
    let cols = pragma_columns(conn, "work_runs")?;
    if !cols.contains(&"host_id".to_owned()) {
        conn.execute(
            "ALTER TABLE work_runs ADD COLUMN host_id TEXT NOT NULL DEFAULT 'local'",
            [],
        )?;
    }
    if !cols.contains(&"cube_workspace_id".to_owned()) {
        conn.execute(
            "ALTER TABLE work_runs ADD COLUMN cube_workspace_id TEXT",
            [],
        )?;
    }
    if !cols.contains(&"remote_pid".to_owned()) {
        conn.execute(
            "ALTER TABLE work_runs ADD COLUMN remote_pid INTEGER",
            [],
        )?;
    }
    Ok(())
}

/// Ensure the `local` host row exists. Idempotent — the `INSERT OR IGNORE`
/// is a no-op on subsequent engine starts.
pub(crate) fn ensure_local_host(conn: &Connection) -> Result<()> {
    let now = now_epoch_string();
    conn.execute(
        "INSERT OR IGNORE INTO hosts (id, ssh_target, pool_size, enabled, created_at)
         VALUES ('local', NULL, 1, 1, ?1)",
        params![now],
    )?;
    Ok(())
}

/// Replace all `auto`-sourced capabilities for the `local` host with the
/// result of the local probe. User-tagged rows (`source = 'user'`) are
/// left untouched. Called every engine startup so auth drift and
/// OS/arch changes surface immediately.
pub(crate) fn refresh_local_host_auto_capabilities(conn: &Connection) -> Result<()> {
    let caps = discover_local_capabilities();

    // Delete old auto caps for local, then bulk-insert fresh ones.
    conn.execute(
        "DELETE FROM host_capabilities WHERE host_id = 'local' AND source = 'auto'",
        [],
    )?;
    for (capability, source) in &caps {
        conn.execute(
            "INSERT OR REPLACE INTO host_capabilities (host_id, capability, source)
             VALUES ('local', ?1, ?2)",
            params![capability, source],
        )?;
    }
    tracing::debug!(
        count = caps.len(),
        "host_registry: refreshed local host auto capabilities",
    );
    Ok(())
}

// ── Local capability discovery ────────────────────────────────────────────────

/// Probe the local host and return `(capability, source)` pairs. Every
/// returned row has `source = "auto"`. Failures for individual probes
/// are logged and skipped; the remainder still land.
fn discover_local_capabilities() -> Vec<(String, String)> {
    let mut caps: Vec<(String, String)> = Vec::new();

    // OS family
    match run_one(&["uname", "-s"]) {
        Some(raw) => {
            let tag = match raw.to_lowercase().as_str() {
                "darwin" => "os=macos".to_owned(),
                other => format!("os={other}"),
            };
            caps.push((tag, "auto".to_owned()));
        }
        None => tracing::warn!("host_registry: uname -s failed; os= capability not set"),
    }

    // CPU architecture
    match run_one(&["uname", "-m"]) {
        Some(raw) => {
            let arch = match raw.to_lowercase().as_str() {
                "aarch64" => "arm64".to_owned(),
                other => other.to_owned(),
            };
            caps.push((format!("arch={arch}"), "auto".to_owned()));
        }
        None => tracing::warn!("host_registry: uname -m failed; arch= capability not set"),
    }

    // gh auth state (per design open-question: catches credential drift
    // hours earlier than waiting for a `gh pr create` failure in a worker)
    let gh_authed = std::process::Command::new("gh")
        .args(["auth", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    caps.push((format!("gh-authed={gh_authed}"), "auto".to_owned()));

    caps
}

/// Run a command, capture stdout, trim whitespace. Returns `None` on any error.
fn run_one(argv: &[&str]) -> Option<String> {
    let (cmd, args) = argv.split_first()?;
    let out = std::process::Command::new(cmd)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if text.is_empty() { None } else { Some(text) }
}

// ── WorkDb impl: host CRUD ────────────────────────────────────────────────────

impl WorkDb {
    pub fn list_hosts(&self) -> Result<Vec<Host>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, ssh_target, pool_size, enabled, last_seen_at, last_error_text, created_at
             FROM hosts
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_host)?;
        collect_rows(rows)
    }

    pub fn get_host(&self, id: &str) -> Result<Option<Host>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, ssh_target, pool_size, enabled, last_seen_at, last_error_text, created_at
             FROM hosts WHERE id = ?1",
            params![id],
            map_host,
        )
        .optional()
        .context("get_host query")
    }

    /// Insert a new non-local host. Returns an error if the id is `"local"` or
    /// already exists. User-provided `tags` are stored with `source = 'user'`.
    pub fn add_host(
        &self,
        id: &str,
        ssh_target: &str,
        pool_size: i64,
        tags: &[String],
    ) -> Result<Host> {
        if id == "local" {
            bail!("'local' is a reserved host id; use `bossctl hosts list` to see it");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        if tx.query_row(
            "SELECT COUNT(*) FROM hosts WHERE id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        )? > 0
        {
            bail!("host '{}' already exists; use `bossctl hosts show {}` to inspect it", id, id);
        }

        let now = now_epoch_string();
        tx.execute(
            "INSERT INTO hosts (id, ssh_target, pool_size, enabled, created_at)
             VALUES (?1, ?2, ?3, 1, ?4)",
            params![id, ssh_target, pool_size, now],
        )?;

        for tag in tags {
            tx.execute(
                "INSERT OR REPLACE INTO host_capabilities (host_id, capability, source)
                 VALUES (?1, ?2, 'user')",
                params![id, tag],
            )?;
        }

        let host = tx
            .query_row(
                "SELECT id, ssh_target, pool_size, enabled, last_seen_at, last_error_text, created_at
                 FROM hosts WHERE id = ?1",
                params![id],
                map_host,
            )
            .context("add_host: re-query after insert")?;

        tx.commit()?;
        Ok(host)
    }

    pub fn set_host_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let conn = self.connect()?;
        let n = conn.execute(
            "UPDATE hosts SET enabled = ?1 WHERE id = ?2",
            params![enabled as i64, id],
        )?;
        if n == 0 {
            bail!("host '{}' not found", id);
        }
        Ok(())
    }

    /// Record a human-readable error description on the host row.
    /// Called when an eager wrapper push fails at registration time so the
    /// UI can surface the cause without the user needing to check logs.
    pub fn set_host_last_error(&self, id: &str, text: Option<&str>) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE hosts SET last_error_text = ?1 WHERE id = ?2",
            params![text, id],
        )?;
        Ok(())
    }

    /// Delete a host. Fails if live executions reference it (by convention;
    /// Phase 1 has no dispatch so the check is always trivially true).
    pub fn remove_host(&self, id: &str) -> Result<()> {
        if id == "local" {
            bail!("the 'local' host cannot be removed");
        }
        let conn = self.connect()?;
        let n = conn.execute("DELETE FROM hosts WHERE id = ?1", params![id])?;
        if n == 0 {
            bail!("host '{}' not found", id);
        }
        Ok(())
    }

    pub fn list_host_capabilities(&self, host_id: &str) -> Result<Vec<HostCapability>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT host_id, capability, source
             FROM host_capabilities
             WHERE host_id = ?1
             ORDER BY source ASC, capability ASC",
        )?;
        let rows = stmt.query_map(params![host_id], |row| {
            Ok(HostCapability {
                host_id: row.get(0)?,
                capability: row.get(1)?,
                source: row.get(2)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Add a user-tagged capability to a host. Overwrites the row if
    /// a capability with the same key already exists (regardless of prior source).
    pub fn add_user_host_capability(&self, host_id: &str, capability: &str) -> Result<()> {
        let conn = self.connect()?;
        if conn.query_row(
            "SELECT COUNT(*) FROM hosts WHERE id = ?1",
            params![host_id],
            |r| r.get::<_, i64>(0),
        )? == 0
        {
            bail!("host '{}' not found", host_id);
        }
        conn.execute(
            "INSERT OR REPLACE INTO host_capabilities (host_id, capability, source)
             VALUES (?1, ?2, 'user')",
            params![host_id, capability],
        )?;
        Ok(())
    }

    /// Remove a capability from a host. Only removes user-tagged capabilities;
    /// auto-discovered capabilities are managed by the engine heartbeat.
    /// Fails if the capability is not present.
    pub fn remove_user_host_capability(&self, host_id: &str, capability: &str) -> Result<()> {
        let conn = self.connect()?;
        let n = conn.execute(
            "DELETE FROM host_capabilities
             WHERE host_id = ?1 AND capability = ?2 AND source = 'user'",
            params![host_id, capability],
        )?;
        if n == 0 {
            // Check if it exists but is auto to give a better error.
            let auto_exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM host_capabilities
                 WHERE host_id = ?1 AND capability = ?2",
                params![host_id, capability],
                |r| r.get(0),
            )?;
            if auto_exists > 0 {
                bail!(
                    "capability '{}' on host '{}' is auto-discovered and cannot be removed manually; \
                     it will be refreshed on the next engine startup",
                    capability,
                    host_id,
                );
            }
            bail!("capability '{}' not found on host '{}'", capability, host_id);
        }
        Ok(())
    }

    // ── Host-scheduling reads (consumed by the coordinator dispatch loop) ──────

    /// Read an execution's `pinned_host_id` (the design's "pin escape
    /// hatch"). `None` when unpinned or on rows that predate the column.
    pub fn execution_pinned_host(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT pinned_host_id FROM work_executions WHERE id = ?1",
            params![execution_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .context("execution_pinned_host query")
        .map(Option::flatten)
    }

    /// Pin an execution to a specific host (or clear the pin with
    /// `None`). When set, [`crate::host_scheduling::select_host`] bypasses
    /// the capability filter and routes the execution only to that host.
    pub fn set_execution_pinned_host(
        &self,
        execution_id: &str,
        host_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.connect()?;
        let affected = conn.execute(
            "UPDATE work_executions SET pinned_host_id = ?2 WHERE id = ?1",
            params![execution_id, host_id],
        )?;
        if affected == 0 {
            bail!("unknown execution: {execution_id}");
        }
        Ok(())
    }

    /// The host a `work_runs` row was attributed to. Used by tests and
    /// (later) the host-badge surface to confirm where a run executed.
    pub fn run_host(&self, run_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT host_id FROM work_runs WHERE id = ?1",
            params![run_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .context("run_host query")
    }

    /// Count live worker runs grouped by host. Feeds the free-slots gate
    /// in [`crate::host_scheduling::select_host`]. Only `active` runs are
    /// counted — terminal runs have already freed their slot. The local
    /// host is *not* gated by this count (the worker pool bounds local
    /// concurrency), so the coordinator overrides its slot to free.
    pub fn active_runs_per_host(&self) -> Result<HashMap<String, i64>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT host_id, COUNT(*) FROM work_runs WHERE status = 'active' GROUP BY host_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (host_id, count) = row?;
            out.insert(host_id, count);
        }
        Ok(out)
    }

    /// Union of required capabilities recorded against any of the given
    /// subject ids (a chore plus its product / project). Subject ids are
    /// globally unique across kinds, so the lookup matches on id alone.
    /// Empty when nothing has been tagged — the common case today, which
    /// leaves every enabled host capability-eligible.
    pub fn required_capabilities_for_subject_ids(
        &self,
        subject_ids: &[&str],
    ) -> Result<BTreeSet<String>> {
        if subject_ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let conn = self.connect()?;
        let placeholders = vec!["?"; subject_ids.len()].join(", ");
        let sql = format!(
            "SELECT DISTINCT capability FROM work_capability_requirements
             WHERE subject_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(subject_ids.iter()), |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = BTreeSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }
}

// ── Row mappers ───────────────────────────────────────────────────────────────

fn map_host(row: &rusqlite::Row<'_>) -> rusqlite::Result<Host> {
    Ok(Host {
        id: row.get(0)?,
        ssh_target: row.get(1)?,
        pool_size: row.get(2)?,
        enabled: row.get::<_, i64>(3)? != 0,
        last_seen_at: row.get(4)?,
        last_error_text: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn collect_rows<T>(
    rows: impl Iterator<Item = rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    rows.collect::<rusqlite::Result<Vec<T>>>()
        .context("host_registry row collection")
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn now_epoch_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn pragma_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(cols)
}

// ── Tests ───────────────────────────────────────────────────────────────────
//
// Behavioral coverage for the `WorkDb` host-registry methods. Everything
// runs against a fresh `:memory:` database, which `WorkDb::open` seeds with
// the migrations plus `ensure_local_host` / `refresh_local_host_auto_capabilities`.
// Assertions go through the public methods (returned values, `Host` /
// `HostCapability` fields, error outcomes, post-state read back). Raw
// connections are used only to plant fixture rows in tables that have no
// public insert path on `WorkDb` (`work_executions`, `work_runs`,
// `work_capability_requirements`) and to control `created_at` for the
// ordering test.
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn open_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).expect("open in-memory work db")
    }

    /// Plant a minimal `work_executions` row (no public insert method is a
    /// good fit for these focused host-registry tests).
    fn insert_execution(db: &WorkDb, id: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_executions
                 (id, work_item_id, kind, status, repo_remote_url, created_at)
             VALUES (?1, 'wi-1', 'chore_implementation', 'queued',
                     'https://github.com/test/repo', '100')",
            params![id],
        )
        .unwrap();
    }

    /// Plant a `work_runs` row attributed to `host_id` with the given status.
    fn insert_run(db: &WorkDb, id: &str, execution_id: &str, host_id: &str, status: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_runs
                 (id, execution_id, agent_id, status, created_at, host_id)
             VALUES (?1, ?2, 'agent-1', ?4, '100', ?3)",
            params![id, execution_id, host_id, status],
        )
        .unwrap();
    }

    /// Plant a `work_capability_requirements` row keyed on `subject_id`.
    fn insert_requirement(db: &WorkDb, subject_id: &str, capability: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_capability_requirements (subject_kind, subject_id, capability)
             VALUES ('chore', ?1, ?2)",
            params![subject_id, capability],
        )
        .unwrap();
    }

    /// Plant a capability row directly with an explicit `source` (used to set
    /// up `auto`-sourced rows, which have no public insert path).
    fn insert_capability(db: &WorkDb, host_id: &str, capability: &str, source: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO host_capabilities (host_id, capability, source)
             VALUES (?1, ?2, ?3)",
            params![host_id, capability, source],
        )
        .unwrap();
    }

    // ── add_host ──────────────────────────────────────────────────────────

    #[test]
    fn add_host_rejects_reserved_local_id() {
        let db = open_db();
        let err = db.add_host("local", "user@x", 2, &[]).unwrap_err();
        assert!(err.to_string().contains("reserved"), "got: {err}");
    }

    #[test]
    fn add_host_rejects_duplicate_id() {
        let db = open_db();
        db.add_host("zakalwe", "user@z", 2, &[]).unwrap();
        let err = db.add_host("zakalwe", "user@z", 2, &[]).unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[test]
    fn add_host_persists_fields_and_user_tags() {
        let db = open_db();
        let host = db
            .add_host(
                "zakalwe",
                "user@zakalwe:22",
                4,
                &["os=macos".to_owned(), "xcode=15".to_owned()],
            )
            .unwrap();

        // Returned Host reflects the inserted row.
        assert_eq!(host.id, "zakalwe");
        assert_eq!(host.ssh_target.as_deref(), Some("user@zakalwe:22"));
        assert_eq!(host.pool_size, 4);
        assert!(host.enabled);

        // Read back through the public getter.
        let fetched = db.get_host("zakalwe").unwrap().expect("host present");
        assert_eq!(fetched.ssh_target.as_deref(), Some("user@zakalwe:22"));
        assert_eq!(fetched.pool_size, 4);

        // Each provided tag is stored with source = 'user'.
        let caps = db.list_host_capabilities("zakalwe").unwrap();
        assert_eq!(caps.len(), 2);
        assert!(caps.iter().all(|c| c.source == "user"));
        let names: BTreeSet<&str> = caps.iter().map(|c| c.capability.as_str()).collect();
        assert!(names.contains("os=macos"));
        assert!(names.contains("xcode=15"));
    }

    // ── remove_host ───────────────────────────────────────────────────────

    #[test]
    fn remove_host_rejects_local() {
        let db = open_db();
        let err = db.remove_host("local").unwrap_err();
        assert!(err.to_string().contains("cannot be removed"), "got: {err}");
    }

    #[test]
    fn remove_host_errors_on_missing() {
        let db = open_db();
        let err = db.remove_host("ghost").unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn remove_host_deletes_host_and_cascades_capabilities() {
        let db = open_db();
        db.add_host("zakalwe", "user@z", 2, &["os=macos".to_owned()])
            .unwrap();
        assert!(db.get_host("zakalwe").unwrap().is_some());
        assert_eq!(db.list_host_capabilities("zakalwe").unwrap().len(), 1);

        db.remove_host("zakalwe").unwrap();

        assert!(db.get_host("zakalwe").unwrap().is_none());
        // ON DELETE CASCADE drops the capability rows too.
        assert!(db.list_host_capabilities("zakalwe").unwrap().is_empty());
    }

    // ── set_host_enabled ──────────────────────────────────────────────────

    #[test]
    fn set_host_enabled_errors_on_unknown() {
        let db = open_db();
        let err = db.set_host_enabled("ghost", false).unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn set_host_enabled_toggles_state() {
        let db = open_db();
        db.add_host("zakalwe", "user@z", 2, &[]).unwrap();
        assert!(db.get_host("zakalwe").unwrap().unwrap().enabled);

        db.set_host_enabled("zakalwe", false).unwrap();
        assert!(!db.get_host("zakalwe").unwrap().unwrap().enabled);

        db.set_host_enabled("zakalwe", true).unwrap();
        assert!(db.get_host("zakalwe").unwrap().unwrap().enabled);
    }

    // ── get_host / list_hosts ───────────────────────────────────────────────

    #[test]
    fn get_host_returns_none_for_missing() {
        let db = open_db();
        assert!(db.get_host("nope").unwrap().is_none());
    }

    #[test]
    fn local_host_present_after_open() {
        let db = open_db();
        let local = db
            .get_host("local")
            .unwrap()
            .expect("local host seeded on open");
        assert_eq!(local.id, "local");
        assert!(local.enabled);
        assert!(db.list_hosts().unwrap().iter().any(|h| h.id == "local"));
    }

    #[test]
    fn list_hosts_orders_by_created_at_then_id() {
        let db = open_db();
        db.add_host("bravo", "user@b", 1, &[]).unwrap();
        db.add_host("alpha", "user@a", 1, &[]).unwrap();
        db.add_host("charlie", "user@c", 1, &[]).unwrap();

        // Force deterministic created_at values: wall-clock granularity is
        // too coarse to control ordering otherwise. `alpha` and `charlie`
        // share a created_at to exercise the secondary id ASC tiebreak.
        {
            let conn = db.connect().unwrap();
            conn.execute("UPDATE hosts SET created_at = '10' WHERE id = 'local'", [])
                .unwrap();
            conn.execute("UPDATE hosts SET created_at = '20' WHERE id = 'alpha'", [])
                .unwrap();
            conn.execute("UPDATE hosts SET created_at = '20' WHERE id = 'charlie'", [])
                .unwrap();
            conn.execute("UPDATE hosts SET created_at = '30' WHERE id = 'bravo'", [])
                .unwrap();
        }

        let ids: Vec<String> = db.list_hosts().unwrap().into_iter().map(|h| h.id).collect();
        assert_eq!(ids, vec!["local", "alpha", "charlie", "bravo"]);
    }

    // ── add_user_host_capability ────────────────────────────────────────────

    #[test]
    fn add_user_capability_errors_when_host_missing() {
        let db = open_db();
        let err = db
            .add_user_host_capability("ghost", "os=macos")
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn add_user_capability_replaces_auto_source_with_user() {
        let db = open_db();
        db.add_host("zakalwe", "user@z", 2, &[]).unwrap();
        insert_capability(&db, "zakalwe", "os=macos", "auto");

        db.add_user_host_capability("zakalwe", "os=macos").unwrap();

        // INSERT OR REPLACE overwrites the single row and flips its source.
        let matching: Vec<_> = db
            .list_host_capabilities("zakalwe")
            .unwrap()
            .into_iter()
            .filter(|c| c.capability == "os=macos")
            .collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].source, "user");
    }

    // ── remove_user_host_capability ─────────────────────────────────────────

    #[test]
    fn remove_user_capability_removes_user_sourced() {
        let db = open_db();
        db.add_host("zakalwe", "user@z", 2, &["os=macos".to_owned()])
            .unwrap();

        db.remove_user_host_capability("zakalwe", "os=macos")
            .unwrap();

        assert!(
            db.list_host_capabilities("zakalwe")
                .unwrap()
                .iter()
                .all(|c| c.capability != "os=macos")
        );
    }

    #[test]
    fn remove_user_capability_rejects_auto_sourced() {
        let db = open_db();
        db.add_host("zakalwe", "user@z", 2, &[]).unwrap();
        insert_capability(&db, "zakalwe", "arch=arm64", "auto");

        let err = db
            .remove_user_host_capability("zakalwe", "arch=arm64")
            .unwrap_err();
        assert!(err.to_string().contains("auto-discovered"), "got: {err}");

        // The auto row is left in place.
        assert!(
            db.list_host_capabilities("zakalwe")
                .unwrap()
                .iter()
                .any(|c| c.capability == "arch=arm64")
        );
    }

    #[test]
    fn remove_user_capability_not_found_when_absent() {
        let db = open_db();
        db.add_host("zakalwe", "user@z", 2, &[]).unwrap();

        let err = db
            .remove_user_host_capability("zakalwe", "nope=1")
            .unwrap_err();
        let msg = err.to_string();
        // Distinct from the auto-discovered branch.
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(!msg.contains("auto-discovered"), "got: {msg}");
    }

    // ── list_host_capabilities ──────────────────────────────────────────────

    #[test]
    fn list_host_capabilities_orders_by_source_then_capability() {
        let db = open_db();
        db.add_host("zakalwe", "user@z", 2, &[]).unwrap();
        insert_capability(&db, "zakalwe", "zzz", "user");
        insert_capability(&db, "zakalwe", "os=linux", "auto");
        insert_capability(&db, "zakalwe", "arch=x86", "auto");
        insert_capability(&db, "zakalwe", "aaa", "user");

        let ordered: Vec<(String, String)> = db
            .list_host_capabilities("zakalwe")
            .unwrap()
            .into_iter()
            .map(|c| (c.source, c.capability))
            .collect();
        assert_eq!(
            ordered,
            vec![
                ("auto".to_owned(), "arch=x86".to_owned()),
                ("auto".to_owned(), "os=linux".to_owned()),
                ("user".to_owned(), "aaa".to_owned()),
                ("user".to_owned(), "zzz".to_owned()),
            ]
        );
    }

    // ── set_execution_pinned_host / execution_pinned_host ───────────────────

    #[test]
    fn pinned_host_round_trips_and_clears() {
        let db = open_db();
        insert_execution(&db, "exec-1");

        // Unpinned to start.
        assert!(db.execution_pinned_host("exec-1").unwrap().is_none());

        db.set_execution_pinned_host("exec-1", Some("zakalwe"))
            .unwrap();
        assert_eq!(
            db.execution_pinned_host("exec-1").unwrap().as_deref(),
            Some("zakalwe")
        );

        // Clearing with None resets to None.
        db.set_execution_pinned_host("exec-1", None).unwrap();
        assert!(db.execution_pinned_host("exec-1").unwrap().is_none());
    }

    #[test]
    fn set_execution_pinned_host_errors_on_unknown_execution() {
        let db = open_db();
        let err = db
            .set_execution_pinned_host("nope", Some("zakalwe"))
            .unwrap_err();
        assert!(err.to_string().contains("unknown execution"), "got: {err}");
    }

    // ── run_host / active_runs_per_host ──────────────────────────────────────

    #[test]
    fn active_runs_per_host_counts_only_active_grouped_by_host() {
        let db = open_db();
        insert_execution(&db, "exec-1");
        insert_run(&db, "run-1", "exec-1", "zakalwe", "active");
        insert_run(&db, "run-2", "exec-1", "zakalwe", "active");
        insert_run(&db, "run-3", "exec-1", "local", "active");
        // Terminal runs must not be counted.
        insert_run(&db, "run-4", "exec-1", "zakalwe", "finished");
        insert_run(&db, "run-5", "exec-1", "local", "failed");

        let counts = db.active_runs_per_host().unwrap();
        assert_eq!(counts.get("zakalwe").copied(), Some(2));
        assert_eq!(counts.get("local").copied(), Some(1));
    }

    #[test]
    fn run_host_returns_attributed_host() {
        let db = open_db();
        insert_execution(&db, "exec-1");
        insert_run(&db, "run-1", "exec-1", "zakalwe", "active");

        assert_eq!(db.run_host("run-1").unwrap().as_deref(), Some("zakalwe"));
        assert!(db.run_host("missing").unwrap().is_none());
    }

    // ── required_capabilities_for_subject_ids ────────────────────────────────

    #[test]
    fn required_capabilities_empty_for_empty_input() {
        let db = open_db();
        assert!(
            db.required_capabilities_for_subject_ids(&[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn required_capabilities_distinct_union_across_subjects() {
        let db = open_db();
        insert_requirement(&db, "chore-1", "os=macos");
        insert_requirement(&db, "chore-1", "xcode=15");
        // Duplicate capability under a different subject id collapses to one.
        insert_requirement(&db, "product-1", "os=macos");
        insert_requirement(&db, "product-1", "gpu=true");

        let caps = db
            .required_capabilities_for_subject_ids(&["chore-1", "product-1"])
            .unwrap();
        let expected: BTreeSet<String> = ["os=macos", "xcode=15", "gpu=true"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(caps, expected);
    }

    // ── ensure_local_host / refresh_local_host_auto_capabilities ─────────────

    #[test]
    fn ensure_local_host_is_idempotent() {
        let db = open_db();
        let conn = db.connect().unwrap();
        // `local` already exists from open(); re-running must not error or dup.
        ensure_local_host(&conn).unwrap();
        ensure_local_host(&conn).unwrap();

        let locals = db
            .list_hosts()
            .unwrap()
            .into_iter()
            .filter(|h| h.id == "local")
            .count();
        assert_eq!(locals, 1);
    }

    #[test]
    fn refresh_local_auto_caps_replaces_auto_leaves_user() {
        let db = open_db();
        // A user cap and a stale auto cap on the local host.
        db.add_user_host_capability("local", "team=infra").unwrap();
        insert_capability(&db, "local", "stale=auto", "auto");

        let conn = db.connect().unwrap();
        refresh_local_host_auto_capabilities(&conn).unwrap();

        let caps = db.list_host_capabilities("local").unwrap();
        // The user-sourced row survives the refresh.
        assert!(
            caps.iter()
                .any(|c| c.capability == "team=infra" && c.source == "user")
        );
        // The stale auto row is replaced by the fresh probe set.
        assert!(!caps.iter().any(|c| c.capability == "stale=auto"));
    }
}
