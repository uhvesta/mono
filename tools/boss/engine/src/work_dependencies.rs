//! Storage and query helpers for `work_item_dependencies`.
//!
//! Edges are pure (`dependent_id`, `prerequisite_id`, `relation`)
//! triples; the table sits alongside `tasks` and `projects` and is
//! managed by `WorkDb` (see `work.rs`). This module provides the
//! lower-level SQL helpers; higher-level concerns (status mechanics,
//! dispatcher gating) live in the modules that call them.
//!
//! The functions here all take a `rusqlite::Connection` reference so
//! callers can compose them inside an in-flight transaction (cycle
//! check + insert, edge cleanup on prereq delete, etc.).

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use boss_protocol::WorkItemDependency;

/// The only edge type v1 understands. Storage permits other values
/// for forward compatibility (`relates-to`, `duplicates`, …) but the
/// engine refuses to *create* anything else and the dispatcher only
/// reads `blocks` rows.
pub const RELATION_BLOCKS: &str = "blocks";

/// Result of `insert_edge` — `Inserted` if a new row was added,
/// `AlreadyExists` if the call was an idempotent re-add (Q6: `add`
/// on an existing edge is a no-op success).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeInsertOutcome {
    Inserted,
    AlreadyExists,
}

/// Add a `(dependent_id, prerequisite_id, relation)` edge if it does
/// not already exist. Caller is responsible for:
///
/// - validating both ids resolve to live work items in the same
///   product (see `validate_edge_endpoints`),
/// - validating the new edge would not close a cycle (see
///   `would_create_cycle`).
///
/// This function only writes the row. It runs against any
/// `Connection` (engine-owned db, in-flight transaction, fresh
/// connection in a test) so the caller picks the isolation.
pub fn insert_edge(
    conn: &Connection,
    dependent_id: &str,
    prerequisite_id: &str,
    relation: &str,
    now_epoch: &str,
) -> Result<(WorkItemDependency, EdgeInsertOutcome)> {
    if dependent_id == prerequisite_id {
        bail!("dependency edge cannot point at itself: {dependent_id}");
    }
    let rows = conn.execute(
        "INSERT OR IGNORE INTO work_item_dependencies
            (dependent_id, prerequisite_id, relation, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![dependent_id, prerequisite_id, relation, now_epoch],
    )?;
    let outcome = if rows == 1 {
        EdgeInsertOutcome::Inserted
    } else {
        EdgeInsertOutcome::AlreadyExists
    };
    let edge = query_edge(conn, dependent_id, prerequisite_id, relation)?
        .with_context(|| format!("missing edge after insert: {dependent_id} → {prerequisite_id}"))?;
    Ok((edge, outcome))
}

/// Remove the named edge, if present. Returns `true` when a row was
/// actually deleted, `false` for a no-op delete (Q6: `rm` on a
/// missing edge is a success).
pub fn delete_edge(
    conn: &Connection,
    dependent_id: &str,
    prerequisite_id: &str,
    relation: &str,
) -> Result<bool> {
    let rows = conn.execute(
        "DELETE FROM work_item_dependencies
         WHERE dependent_id = ?1 AND prerequisite_id = ?2 AND relation = ?3",
        params![dependent_id, prerequisite_id, relation],
    )?;
    Ok(rows > 0)
}

/// All edges that name `work_item_id` as either endpoint. Useful
/// when a row is being deleted and the engine needs to cascade
/// edge cleanup (Q10: deleted prerequisite drops edges).
pub fn list_edges_touching(conn: &Connection, work_item_id: &str) -> Result<Vec<WorkItemDependency>> {
    let mut stmt = conn.prepare(
        "SELECT dependent_id, prerequisite_id, relation, created_at
         FROM work_item_dependencies
         WHERE dependent_id = ?1 OR prerequisite_id = ?1
         ORDER BY created_at ASC, dependent_id ASC, prerequisite_id ASC",
    )?;
    let rows = stmt.query_map([work_item_id], map_edge)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Edges whose `dependent_id` is `work_item_id` — i.e. the rows that
/// gate `work_item_id`. Only `blocks` rows are returned by default;
/// pass `None` to relation to get every relation type.
pub fn prerequisites_of(
    conn: &Connection,
    work_item_id: &str,
    relation: Option<&str>,
) -> Result<Vec<WorkItemDependency>> {
    edges_for(
        conn,
        "SELECT dependent_id, prerequisite_id, relation, created_at
         FROM work_item_dependencies
         WHERE dependent_id = ?1",
        work_item_id,
        relation,
        "prerequisite_id",
    )
}

/// Edges whose `prerequisite_id` is `work_item_id` — i.e. the rows
/// that depend on `work_item_id`.
pub fn dependents_of(
    conn: &Connection,
    work_item_id: &str,
    relation: Option<&str>,
) -> Result<Vec<WorkItemDependency>> {
    edges_for(
        conn,
        "SELECT dependent_id, prerequisite_id, relation, created_at
         FROM work_item_dependencies
         WHERE prerequisite_id = ?1",
        work_item_id,
        relation,
        "dependent_id",
    )
}

fn edges_for(
    conn: &Connection,
    base_sql: &str,
    work_item_id: &str,
    relation: Option<&str>,
    order_by_id_column: &str,
) -> Result<Vec<WorkItemDependency>> {
    let mut out = Vec::new();
    if let Some(rel) = relation {
        let sql = format!(
            "{base_sql} AND relation = ?2 ORDER BY created_at ASC, {order_by_id_column} ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![work_item_id, rel], map_edge)?;
        for row in rows {
            out.push(row?);
        }
    } else {
        let sql = format!("{base_sql} ORDER BY created_at ASC, {order_by_id_column} ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([work_item_id], map_edge)?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

/// Look up a single edge by its primary key.
pub fn query_edge(
    conn: &Connection,
    dependent_id: &str,
    prerequisite_id: &str,
    relation: &str,
) -> Result<Option<WorkItemDependency>> {
    conn.query_row(
        "SELECT dependent_id, prerequisite_id, relation, created_at
         FROM work_item_dependencies
         WHERE dependent_id = ?1 AND prerequisite_id = ?2 AND relation = ?3",
        params![dependent_id, prerequisite_id, relation],
        map_edge,
    )
    .optional()
    .map_err(Into::into)
}

/// True if inserting `(dependent_id → prerequisite_id)` would close a
/// cycle. Walks the existing edge graph forward from
/// `prerequisite_id`; if `dependent_id` is reachable, the new edge
/// would form a cycle. Designed to run inside the same transaction
/// as the upcoming insert so a concurrent writer sees the proposed
/// row before adding its own.
pub fn would_create_cycle(
    conn: &Connection,
    dependent_id: &str,
    prerequisite_id: &str,
) -> Result<bool> {
    if dependent_id == prerequisite_id {
        return Ok(true);
    }
    // The proposed edge says: `prerequisite_id → dependent_id` is
    // already implied (they share a future ordering). We walk forward
    // from `prerequisite_id` (i.e. its own prerequisites and their
    // prerequisites, recursively); if we ever reach `dependent_id`,
    // the new edge would close a loop.
    let exists: Option<i64> = conn
        .query_row(
            "WITH RECURSIVE forward(id) AS (
                SELECT prerequisite_id
                FROM work_item_dependencies
                WHERE dependent_id = ?1
              UNION
                SELECT d.prerequisite_id
                FROM work_item_dependencies d
                JOIN forward f ON d.dependent_id = f.id
            )
            SELECT 1 FROM forward WHERE id = ?2 LIMIT 1",
            params![prerequisite_id, dependent_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn map_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItemDependency> {
    Ok(WorkItemDependency {
        dependent_id: row.get(0)?,
        prerequisite_id: row.get(1)?,
        relation: row.get(2)?,
        created_at: row.get(3)?,
    })
}

/// Whether `status` counts as a "satisfied" prerequisite for the
/// dependency rule (Q4 / Q10). Tasks and chores satisfy on `done`;
/// projects also satisfy on `archived` (a wound-down project should
/// not perpetually gate downstream work). The function is on `id`
/// prefix because `tasks.kind` is not visible to callers that walk
/// the edge table.
pub fn status_satisfies(work_item_id: &str, status: &str) -> bool {
    if work_item_id.starts_with("proj_") {
        matches!(status, "done" | "archived")
    } else {
        status == "done"
    }
}

/// Look up the current status of a work item in either `tasks` or
/// `projects`. Returns `None` for unknown / soft-deleted ids; the
/// caller decides whether that's an error or a "treat as satisfied"
/// signal (a soft-deleted prereq has its edge dropped immediately,
/// so this code path should rarely see one).
pub fn lookup_work_item_status(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<String>> {
    if work_item_id.starts_with("proj_") {
        return conn
            .query_row(
                "SELECT status FROM projects WHERE id = ?1",
                params![work_item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into);
    }
    if work_item_id.starts_with("task_") {
        return conn
            .query_row(
                "SELECT status FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into);
    }
    Ok(None)
}

/// Return the prerequisite ids that currently *gate* `work_item_id`
/// — `blocks` edges whose prereq has not reached a satisfied
/// status. Used by both the dispatcher (to demote a gated dependent
/// to `waiting_dependency`) and the auto-block / unblock path.
pub fn gating_prereqs_for(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Vec<String>> {
    let edges = prerequisites_of(conn, work_item_id, Some(RELATION_BLOCKS))?;
    let mut gating = Vec::new();
    for edge in edges {
        let status = lookup_work_item_status(conn, &edge.prerequisite_id)?;
        match status {
            Some(s) if status_satisfies(&edge.prerequisite_id, &s) => {}
            _ => gating.push(edge.prerequisite_id),
        }
    }
    Ok(gating)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE work_item_dependencies (
                dependent_id     TEXT NOT NULL,
                prerequisite_id  TEXT NOT NULL,
                relation         TEXT NOT NULL DEFAULT 'blocks',
                created_at       TEXT NOT NULL,
                PRIMARY KEY (dependent_id, prerequisite_id, relation),
                CHECK (dependent_id <> prerequisite_id)
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn insert_then_idempotent_reinsert() {
        let conn = fresh_db();
        let (edge, outcome) =
            insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1000").unwrap();
        assert_eq!(outcome, EdgeInsertOutcome::Inserted);
        assert_eq!(edge.dependent_id, "task_a");
        assert_eq!(edge.prerequisite_id, "task_b");
        assert_eq!(edge.relation, RELATION_BLOCKS);

        let (_, outcome2) =
            insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "9999").unwrap();
        assert_eq!(outcome2, EdgeInsertOutcome::AlreadyExists);
    }

    #[test]
    fn delete_returns_false_for_missing_edge() {
        let conn = fresh_db();
        let removed = delete_edge(&conn, "task_a", "task_b", RELATION_BLOCKS).unwrap();
        assert!(!removed);
        let _ = insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1").unwrap();
        let removed = delete_edge(&conn, "task_a", "task_b", RELATION_BLOCKS).unwrap();
        assert!(removed);
    }

    #[test]
    fn cycle_detection_catches_two_step_loops() {
        let conn = fresh_db();
        insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1").unwrap();
        // Adding `task_b → task_a` would close the loop.
        assert!(would_create_cycle(&conn, "task_b", "task_a").unwrap());
        // But `task_b → task_c` is fine.
        assert!(!would_create_cycle(&conn, "task_b", "task_c").unwrap());
    }

    #[test]
    fn cycle_detection_catches_self_loop() {
        let conn = fresh_db();
        assert!(would_create_cycle(&conn, "task_a", "task_a").unwrap());
    }

    #[test]
    fn cycle_detection_walks_multi_hop() {
        let conn = fresh_db();
        insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1").unwrap();
        insert_edge(&conn, "task_b", "task_c", RELATION_BLOCKS, "2").unwrap();
        insert_edge(&conn, "task_c", "task_d", RELATION_BLOCKS, "3").unwrap();
        // a → b → c → d. Adding d → a closes a 4-cycle.
        assert!(would_create_cycle(&conn, "task_d", "task_a").unwrap());
        // Adding e → a is fine.
        assert!(!would_create_cycle(&conn, "task_e", "task_a").unwrap());
    }

    #[test]
    fn list_endpoints_match_direction() {
        let conn = fresh_db();
        insert_edge(&conn, "task_a", "task_b", RELATION_BLOCKS, "1").unwrap();
        insert_edge(&conn, "task_a", "task_c", RELATION_BLOCKS, "2").unwrap();
        insert_edge(&conn, "task_d", "task_a", RELATION_BLOCKS, "3").unwrap();

        let prereqs_of_a = prerequisites_of(&conn, "task_a", Some(RELATION_BLOCKS)).unwrap();
        assert_eq!(prereqs_of_a.len(), 2);
        assert_eq!(prereqs_of_a[0].prerequisite_id, "task_b");
        assert_eq!(prereqs_of_a[1].prerequisite_id, "task_c");

        let dependents_of_a = dependents_of(&conn, "task_a", Some(RELATION_BLOCKS)).unwrap();
        assert_eq!(dependents_of_a.len(), 1);
        assert_eq!(dependents_of_a[0].dependent_id, "task_d");
    }
}
