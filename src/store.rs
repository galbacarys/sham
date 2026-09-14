//! Disposable SQLite cache.
//!
//! The DB is a *derived* artifact (like a compiler cache): it is fully
//! re-derivable from the source YAML (`source` module) and non-precious.
//! Schema drift is handled by stamping `PRAGMA user_version` and dropping +
//! recreating on any mismatch. Early-slice error handling uses `.expect()`
//! because rusqlite/std::io error types don't unify into one `Result` without a
//! wrapper; that hardens once error paths actually matter.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use rusqlite::{params, params_from_iter, Connection};

/// Schema version of the derived cache. The `PRAGMA user_version` we stamp on
/// fresh init is derived from this single constant below, so the drift check
/// (drop+recreate on any mismatch) can never go stale by hand.
pub const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS nodes (
    id      TEXT PRIMARY KEY,   -- ULID: stable, sortable (timestamp-prefixed)
    project TEXT NOT NULL,      -- owning project (structural provenance)
    label   TEXT NOT NULL,      -- short heading
    content TEXT NOT NULL DEFAULT '',
    parent  TEXT                -- parent id; the project root node's is NULL
);
CREATE INDEX IF NOT EXISTS idx_nodes_project ON nodes(project);
CREATE INDEX IF NOT EXISTS idx_nodes_label ON nodes(label);
";

/// Open (and, on schema drift, reset) the cache DB at `$SHAM_DB` or the
/// default cache location.
pub fn open_db() -> Connection {
    let db_path = db_path();
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).expect("could not create sham cache dir");
    }
    let conn = Connection::open(db_path).expect("could not open sham.db");

    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .expect("could not read schema version");
    if version != SCHEMA_VERSION {
        conn.execute_batch("DROP TABLE IF EXISTS nodes;")
            .expect("schema drift: could not reset nodes");
        conn.execute_batch(SCHEMA).expect("could not init schema");
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .expect("could not stamp schema version");
    } else {
        conn.execute_batch(SCHEMA).expect("could not init schema");
    }

    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .expect("could not set WAL mode");
    conn
}

/// Insert one node, recording its owning project (provenance). A `None` parent
/// is the project root node; every other node carries a parent id.
pub fn insert_node(
    conn: &Connection,
    project: &str,
    id: &str,
    label: &str,
    content: &str,
    parent: Option<&str>,
) {
    conn.execute(
        "INSERT INTO nodes (id, project, label, content, parent) VALUES (?, ?, ?, ?, ?)",
        params![id, project, label, content, parent],
    )
    .expect("add: insert failed");
}

/// Insert or replace a node, keyed by its stable id. Idempotent across runs,
/// which is what makes `reconcile`/`build` re-materialization the same call
/// whether a row is new or an in-place edit.
pub fn upsert_node(
    conn: &Connection,
    project: &str,
    id: &str,
    label: &str,
    content: &str,
    parent: Option<&str>,
) {
    conn.execute(
        "INSERT OR REPLACE INTO nodes (id, project, label, content, parent) VALUES (?, ?, ?, ?, ?)",
        params![id, project, label, content, parent],
    )
    .expect("reconcile: upsert failed");
}

/// Garbage-collect a project's cache rows: delete any row whose id is no
/// longer reachable from the project's source YAML. This is the design's
/// "GC = reachability over node IDs (pure set-difference)" — the store must
/// end up mirroring exactly the source node set (design section 4). Runs
/// inside the caller's transaction; returns how many rows were pruned.
pub fn prune_project(conn: &Connection, project: &str, live_ids: &Vec<String>) -> Result<usize> {
    if live_ids.is_empty() {
        return Ok(0);
    }
    // Dynamic `IN` list: placeholder count is a source node-id length, never
    // caller-controlled text (same var-arity idiom rusqlite documents for
    // `IN` clauses of unknown length).
    let placeholders = repeat_vars(live_ids.len());
    let mut params: Vec<String> = vec![project.into()];
    for id in live_ids {
        params.push(id.clone());
    }
    conn.execute(
        &format!(
            "DELETE FROM nodes WHERE project = ? AND id NOT IN ({})",
            placeholders,
        ),
        params_from_iter(params),
    )
    .with_context(|| format!("reconcile: GC failed for project '{}'", project))
}

/// Drop every row in the cache — the full-rebuild wipe backing `sham build`.
/// Content only; schema and `user_version` are left untouched.
pub fn delete_all(conn: &Connection) {
    conn.execute_batch("DELETE FROM nodes;")
        .expect("build: cache wipe failed");
}

/// Comma-joined `?` placeholders for a var-arity `IN` list. Never called with
/// caller-controlled input — `count` is a node-id list length.
fn repeat_vars(count: usize) -> String {
    (0..count)
        .map(|i| if i == 0 { "?" } else { ",?" })
        .collect()
}

fn db_path() -> PathBuf {
    if let Some(p) = env::var_os("SHAM_DB") {
        return PathBuf::from(p);
    }
    let mut pb = dirs::cache_dir().expect("could not derive the user cache dir");
    pb.push("sham");
    pb.push("sham.db");
    pb
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that set `SHAM_DB` so they can't race each other.
    static DB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn insert_idea_and_leaf_round_trip_and_handle_schema_drift() {
        let _guard = DB_LOCK.lock().unwrap();
        let path = std::env::temp_dir().join(format!("sham-db-{}.db", std::process::id()));
        let _ = fs::remove_file(&path);

        std::env::set_var("SHAM_DB", &path);

        let conn = open_db();
        insert_node(&conn, "acme", "01A", "billing", "chose Stripe", None);
        insert_node(&conn, "acme", "01B", "", "idempotent webhooks", Some("01A"));
        assert_eq!(
            conn.query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        // Provenance is recorded per node.
        let proj: String = conn
            .query_row("SELECT project FROM nodes WHERE id = '01B'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(proj, "acme");

        // Simulate a schema bump: corrupt user_version, reopen resets cleanly.
        conn.execute_batch("PRAGMA user_version = 999;").unwrap();
        drop(conn);
        let conn2 = open_db();
        let n: i64 = conn2
            .query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "schema drift resets the table");

        std::env::remove_var("SHAM_DB");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn reconcile_primitives_upsert_gc_and_full_wipe() {
        let _guard = DB_LOCK.lock().unwrap();
        let path = std::env::temp_dir().join(format!("sham-rc-{}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        std::env::set_var("SHAM_DB", &path);

        let conn = open_db();
        // Seed two projects, as if `add` had populated them earlier.
        insert_node(&conn, "acme", "01A", "billing", "chose Stripe", None);
        insert_node(&conn, "acme", "01B", "", "idempotent webhooks", Some("01A"));
        insert_node(&conn, "globex", "09A", "billing", "chose Adyen", None);

        // Reconcile "acme": upsert its current live nodes, then GC rows whose
        // id is no longer reachable in the source (01B was removed from YAML).
        upsert_node(&conn, "acme", "01A", "billing", "chose Stripe", None);
        let live: Vec<String> = vec!["01A".into()];
        let pruned = prune_project(&conn, "acme", &live).unwrap();
        assert_eq!(pruned, 1, "unreachable 01B is pruned");

        // GC is per-project: globex's 09A survives, acme's 01A survives.
        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(remaining, 2, "01A (acme) + 09A (globex) remain");

        // `build` wipes the whole cache before re-materializing.
        delete_all(&conn);
        let after_wipe: i64 = conn
            .query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(after_wipe, 0, "build wipes the cache first");

        std::env::remove_var("SHAM_DB");
        let _ = fs::remove_file(&path);
    }
}
