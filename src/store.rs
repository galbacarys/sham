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

use rusqlite::{params, Connection};

pub const SCHEMA_VERSION: i64 = 1;
// Stamped on every fresh init; must track SCHEMA_VERSION so the drift check
// below behaves (drop+recreate on any other version).
const STAMP_USER_VERSION: &str = "PRAGMA user_version = 1";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS nodes (
    id      TEXT PRIMARY KEY,   -- ULID: stable, sortable (timestamp-prefixed)
    label   TEXT NOT NULL,      -- short heading
    content TEXT NOT NULL DEFAULT '',
    parent  TEXT                -- optional parent id (tree layer)
);
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
        conn.execute_batch(STAMP_USER_VERSION)
            .expect("could not stamp schema version");
    } else {
        conn.execute_batch(SCHEMA).expect("could not init schema");
    }

    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .expect("could not set WAL mode");
    conn
}

/// Insert one node. A `None` parent becomes `NULL`, so this single function
/// serves both an idea node (no parent) and a rationale leaf (parent set).
pub fn insert_node(conn: &Connection, id: &str, label: &str, content: &str, parent: Option<&str>) {
    conn.execute(
        "INSERT INTO nodes (id, label, content, parent) VALUES (?, ?, ?, ?)",
        params![id, label, content, parent],
    )
    .expect("add: insert failed");
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

    #[test]
    fn insert_idea_and_leaf_round_trip_and_handle_schema_drift() {
        let path = std::env::temp_dir().join(format!("sham-db-{}.db", std::process::id()));
        let _ = fs::remove_file(&path);

        // Point db_path at our temp file via SHAM_DB for the whole test body
        // (only open_db reads it; we serialize on a lock to keep it race-free).
        #[allow(unused_braces)] // const-like static guard that lives for the test body
        let _guard = {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            LOCK.lock().unwrap()
        };
        std::env::set_var("SHAM_DB", &path);

        let conn = open_db();
        insert_node(&conn, "01A", "billing", "chose Stripe", None);
        insert_node(&conn, "01B", "", "idempotent webhooks", Some("01A"));
        assert_eq!(
            conn.query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );

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
}
