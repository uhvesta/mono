/// Host registry: tables, types, migration helpers, and `WorkDb` methods.
///
/// Phase 1 of the distributed-agent-execution design. Adds `hosts`,
/// `host_capabilities`, and `work_capability_requirements` tables plus
/// new columns on `work_executions`. No scheduler change; everything
/// still runs locally. Auto-discovers capabilities for the `local` host
/// on every engine startup via `uname` + `gh auth status`.
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

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
