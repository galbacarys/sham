use std::env;
use std::fs;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use rusqlite::{Connection, params};
use ulid::Ulid;

const SCHEMA_VERSION: i64 = 1;
// Stamped on every fresh init; must track SCHEMA_VERSION so the drift check
// below behaves (drop+recreate on any other version).
const STAMP_USER_VERSION: &str = "PRAGMA user_version = 1";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS nodes (
    id      TEXT PRIMARY KEY,   -- ULID: stable, sortable (timestamp-prefixed)
    label   TEXT NOT NULL,      -- short heading
    content TEXT NOT NULL DEFAULT '',
    parent  TEXT                -- optional parent id (tree layer, later)
);
CREATE INDEX IF NOT EXISTS idx_nodes_label ON nodes(label);
";

// ---- CLI shape (mirrors docs/DESIGN.md section 7) --------------------------

#[derive(Debug, Parser)]
#[command(
    name = "sham",
    about = "shared, hierarchical, cross-project agent memory",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Scaffold a new project's memory dir + name it
    #[command(arg_required_else_help = true)]
    AddProject { name: String },
    /// Add an idea node; prints its id
    Add(AddArgs),
    /// Fetch a node and its subtree, local + structural (no embedding)
    Get(GetArgs),
    /// Cross-project vector search (not in thin slice)
    Search { query: String },
    /// Retract a node in place: writes a supersedes edge
    Supersede { id: String, text: String },
    /// Remove a node (GC'd at reconcile)
    Rm { id: String },
    /// Incrementally re-embed changed units + GC unreachable ids
    Reconcile,
    /// Explicit full rebuild (escape hatch)
    Build,
    /// Dump a project's memory YAML to stdout
    #[command(arg_required_else_help = true)]
    Export { #[arg(long)] project: String },
}

#[derive(Debug, Args)]
struct AddArgs {
    /// either "<label> <content>" (new idea), or a single "<content>" when
    /// attaching --node-id (a rationale leaf, which has no label)
    #[arg(required = true, num_args = 1..=2)]
    args: Vec<String>,
    /// attach this node as a child (rationale) of an existing node
    #[arg(long)]
    node_id: Option<String>,
}

#[derive(Debug, Args)]
struct GetArgs {
    /// label to look up
    target: Option<String>,
    /// fetch by stable node id instead of label
    #[arg(long)]
    node_id: Option<String>,
}

// ---- entrypoint ------------------------------------------------------------

fn main() {
    let args = Cli::try_parse().unwrap_or_else(|error| error.exit());
    run(args);
}

fn run(args: Cli) {
    let conn = open_db();
    match args.command {
        Commands::AddProject { name } => not_sliced(format!("add-project {}", name)),
        Commands::Add(args) => cmd_add(&conn, args),
        Commands::Get(args) => cmd_get(&conn, args),
        Commands::Search { query } => not_sliced(format!("search {}", query)),
        Commands::Supersede { id, text } => not_sliced(format!("supersede {} {}", id, text)),
        Commands::Rm { id } => not_sliced(format!("rm {}", id)),
        Commands::Reconcile => not_sliced(format!("reconcile")),
        Commands::Build => not_sliced(format!("build")),
        Commands::Export { project } => not_sliced(format!("export --project {}", project)),
    }
}

fn not_sliced(what: String) {
    eprintln!(
        "sham: '{}' is not implemented in the thin slice (see docs/DESIGN.md roadmap)",
        what
    )
}

// ---- sqlite ----------------------------------------------------------------

fn open_db() -> Connection {
    let db_path = db_path();
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).expect("could not create sham cache dir");
    }
    let conn = Connection::open(db_path).expect("could not open sham.db");

    // The DB is a disposable build cache: detect schema drift via SQLite's
    // built-in user_version and, on mismatch, re-create the tables from scratch.
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| Ok(row.get::<_, i64>(0)?))
        .expect("could not read schema version");
    if version != SCHEMA_VERSION {
        conn.execute_batch("DROP TABLE IF EXISTS nodes;")
            .expect("schema drift: could not reset nodes");
        conn.execute_batch(SCHEMA).expect("could not init schema");
        conn.execute_batch(STAMP_USER_VERSION).expect("could not stamp schema version");
    } else {
        conn.execute_batch(SCHEMA).expect("could not init schema");
    }

    conn.execute_batch("PRAGMA journal_mode=WAL;").expect("could not set WAL mode");
    conn
}

struct Node {
    id: String,
    label: String,
    content: String,
}

fn cmd_add(conn: &Connection, args: AddArgs) {
    let id = Ulid::generate().to_string();
    if let Some(parent) = args.node_id {
        // attach form: one positional = the rationale content, no label
        let content = args.args[0].to_string();
        conn.execute(
            "INSERT INTO nodes (id, label, content, parent) VALUES (?, ?, ?, ?)",
            params!(id, "", content, parent),
        )
        .expect("add: insert failed");
        println!("{}", id);
        return;
    }
    if args.args.len() != 2 {
        eprintln!("sham add: give <label> <content> (or --node-id <id> <content> to attach)");
        return;
    }
    let label = args.args[0].to_string();
    let content = args.args[1].to_string();
    conn.execute(
        "INSERT INTO nodes (id, label, content) VALUES (?, ?, ?)",
        params!(id, label, content),
    )
    .expect("add: insert failed");
    println!("{}", id);
}

fn cmd_get(conn: &Connection, args: GetArgs) {
    let mut any_node = false;
    if let Some(nid) = args.node_id {
        let mut stmt = conn
            .prepare("SELECT id, label, content FROM nodes WHERE id = ?")
            .expect("get: prepare failed");
        let rows = stmt
            .query_map(params!(nid), |row| Ok(Node {
                id: row.get::<_, String>(0).unwrap(),
                label: row.get::<_, String>(1).unwrap(),
                content: row.get::<_, String>(2).unwrap(),
            }))
            .expect("get: query failed");
        for row in rows {
            let node = row.unwrap();
            println!("{} | {} | {}", node.id, node.label, node.content);
            any_node = true;
        }
    } else if let Some(label) = args.target {
        let mut stmt = conn
            .prepare("SELECT id, label, content FROM nodes WHERE label = ? ORDER BY id")
            .expect("get: prepare failed");
        let rows = stmt
            .query_map(params!(label), |row| Ok(Node {
                id: row.get::<_, String>(0).unwrap(),
                label: row.get::<_, String>(1).unwrap(),
                content: row.get::<_, String>(2).unwrap(),
            }))
            .expect("get: query failed");
        for row in rows {
            let node = row.unwrap();
            println!("{} | {} | {}", node.id, node.label, node.content);
            any_node = true;
        }
    } else {
        eprintln!("sham get: provide a <label> or --node-id <id>");
    }
    if !any_node {
        println!("(no nodes found)");
    }
}

// ---- env / paths -----------------------------------------------------------

fn db_path() -> PathBuf {
    if let Some(p) = env::var_os("SHAM_DB") {
        return PathBuf::from(p);
    }
    let mut pb = dirs::cache_dir().expect("could not derive the user cache dir");
    pb.push("sham");
    pb.push("sham.db");
    return pb;
}