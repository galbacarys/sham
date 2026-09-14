//! `sham` — shared, hierarchical, cross-project agent memory.
//!
//! This file is wiring only: parse the CLI and dispatch to the layer modules.
//!   - `source` — the source-of-truth YAML (decision units + hashes/id)
//!   - `project` — user-global manifest + nearest-`.memory` resolution
//!   - `store`   — the disposable SQLite cache derived from `source`

mod project;
mod source;
mod store;

use std::env;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use rusqlite::Connection;
use ulid::Ulid;

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
    Export {
        #[arg(long)]
        project: String,
    },
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
    if let Err(err) = run(args) {
        eprintln!("sham: {:#}", err);
        std::process::exit(1);
    }
}

fn run(args: Cli) -> Result<()> {
    match args.command {
        Commands::AddProject { name } => cmd_add_project(&name),
        Commands::Add(args) => {
            let conn = store::open_db();
            cmd_add(&conn, args)
        }
        Commands::Get(args) => {
            let conn = store::open_db();
            cmd_get(&conn, args)
        }
        Commands::Search { query } => not_sliced(format!("search {query}")),
        Commands::Supersede { id, text } => not_sliced(format!("supersede {id} {text}")),
        Commands::Rm { id } => not_sliced(format!("rm {id}")),
        Commands::Reconcile => cmd_reconcile(),
        Commands::Build => cmd_build(),
        Commands::Export { project } => cmd_export(&project),
    }
}

fn not_sliced(what: String) -> Result<()> {
    bail!(
        "`{}` is not implemented in this slice (see docs/DESIGN.md roadmap)",
        what
    )
}

// ---- authoring -------------------------------------------------------------

fn cmd_add_project(name: &str) -> Result<()> {
    let cwd = env::current_dir().context("could not get the current directory")?;
    // Target the nearest existing .memory/, else scaffold one in the cwd.
    let memory_dir =
        project::resolve_memory_dir(&cwd).unwrap_or_else(|| cwd.join(project::MEMORY_DIR_NAME));
    let memory_file = memory_dir.join(project::MEMORY_FILE_NAME);

    if memory_file.exists() {
        let existing = source::read_file(&memory_file)?;
        if existing.project != name {
            bail!(
                "{} already belongs to project '{}'",
                memory_file.display(),
                existing.project
            );
        }
        // A pre-existing project must already satisfy the tree invariant.
        existing.check_invariants()?;
    } else {
        // Creates .memory/ and writes a valid, freshly-rooted memory file: the
        // project root node is the single parentless anchor everything parents to.
        let root_id = Ulid::generate().to_string();
        let mem = source::MemoryFile::new(name, &root_id);
        source::write_file(&memory_file, &mem)?;
    }

    project::register_project(name, &memory_dir)?;
    println!("added project '{}' at {}", name, memory_dir.display());
    Ok(())
}

fn cmd_add(conn: &Connection, args: AddArgs) -> Result<()> {
    let cwd = env::current_dir().context("could not get the current directory")?;
    let memory_dir = project::resolve_memory_dir(&cwd)
        .ok_or_else(|| anyhow!("no sham project here — run `sham add-project <name>` first"))?;
    let memory_file = memory_dir.join(project::MEMORY_FILE_NAME);
    let mut mem = source::read_file(&memory_file)?;

    let id = Ulid::generate().to_string();
    let (label, content, parent) = if let Some(pid) = args.node_id {
        // attach form: one positional = the rationale content, no label
        if args.args.len() != 1 {
            bail!("sham add: --node-id <id> takes exactly one <content>");
        }
        if !mem.nodes.iter().any(|n| n.id == pid) {
            bail!("sham add: parent node {} not found in this project", pid);
        }
        (String::new(), args.args[0].clone(), Some(pid))
    } else {
        if args.args.len() != 2 {
            bail!("sham add: give <label> <content> (or --node-id <id> <content> to attach)");
        }
        // A new top-level idea parents to the project's root node — no node is
        // parentless except the root itself.
        (
            args.args[0].clone(),
            args.args[1].clone(),
            Some(mem.root.clone()),
        )
    };

    let node = source::Node::new(&id, &label, &content, parent);
    source::append_node(&mut mem, node.clone());
    mem.check_invariants()?; // never emit an invalid tree
    source::write_file(&memory_file, &mem)?;

    // Eager ingest keeps the cache fresh so the next `get` needs no separate
    // reconcile: the add *is* the ingest (design section 7).
    store::insert_node(
        conn,
        &mem.project,
        &node.id,
        &node.label,
        &node.content,
        node.parent.as_deref(),
    );

    println!("{}", node.id);
    Ok(())
}

/// Print a project's memory YAML to stdout (the canonical source form).
fn cmd_export(name: &str) -> Result<()> {
    let proj = project::lookup_project(name)?.ok_or_else(|| {
        anyhow!(
            "no project '{}' registered — run `sham add-project {}`",
            name,
            name
        )
    })?;
    let memory_file = proj.path.join(project::MEMORY_FILE_NAME);
    let mem = source::read_file(&memory_file)?;
    print!("{}", source::to_yaml(&mem));
    Ok(())
}

// ---- maintenance -----------------------------------------------------------

/// Shared core for `reconcile`/`build`: materialize every registered project's
/// memory YAML into the cache (upsert + GC), running inside the caller's
/// *single* transaction so a failure mid-run rolls back the whole pass. Returns
/// (project_count, node_count, pruned_count).
fn materialize_all(cursor: &Connection) -> Result<(usize, usize, usize)> {
    let cfg = project::read_config()?;
    let mut projects = 0usize;
    let mut nodes_total = 0usize;
    let mut pruned = 0usize;
    for entry in &cfg.projects {
        let memory_file = entry.path.join(project::MEMORY_FILE_NAME);
        let mem = source::read_file(&memory_file)?;
        mem.check_invariants()?; // never materialize an invalid tree
        let live: Vec<String> = mem.nodes.iter().map(|n| n.id.clone()).collect();
        for node in &mem.nodes {
            store::upsert_node(
                cursor,
                &mem.project,
                &node.id,
                &node.label,
                &node.content,
                node.parent.as_deref(),
            );
        }
        nodes_total += mem.nodes.len();
        pruned += store::prune_project(cursor, &mem.project, &live)?;
        projects += 1;
    }
    Ok((projects, nodes_total, pruned))
}

/// `sham reconcile` — incremental: upsert changed nodes + GC unreachable ids,
/// atomically across every registered project (design section 7).
fn cmd_reconcile() -> Result<()> {
    let mut conn = store::open_db();
    let tx = conn.transaction()?;
    let (projects, nodes, pruned) = materialize_all(&tx)?;
    tx.commit()?;
    println!(
        "reconcile: {} project(s), {} node(s) materialized, {} pruned",
        projects, nodes, pruned
    );
    Ok(())
}

/// `sham build` — explicit full rebuild: wipe the cache, then re-materialize
/// every project from source (escape hatch vs lazy first-use).
fn cmd_build() -> Result<()> {
    let mut conn = store::open_db();
    let tx = conn.transaction()?;
    store::delete_all(&tx);
    let (projects, nodes, _) = materialize_all(&tx)?;
    tx.commit()?;
    println!(
        "build: full rebuild from {} project(s) -> {} node(s) cached",
        projects, nodes
    );
    Ok(())
}

// ---- query (served from the cache) -----------------------------------------

struct Node {
    id: String,
    label: String,
    content: String,
}

fn cmd_get(conn: &Connection, args: GetArgs) -> Result<()> {
    let mut found = false;
    if let Some(nid) = args.node_id {
        let mut stmt = conn.prepare("SELECT id, label, content FROM nodes WHERE id = ?")?;
        let rows = stmt.query_map([nid], |row| {
            Ok(Node {
                id: row.get::<_, String>(0)?,
                label: row.get::<_, String>(1)?,
                content: row.get::<_, String>(2)?,
            })
        })?;
        for row in rows {
            print_node(&row?);
            found = true;
        }
    } else if let Some(label) = args.target {
        let mut stmt =
            conn.prepare("SELECT id, label, content FROM nodes WHERE label = ? ORDER BY id")?;
        let rows = stmt.query_map([label], |row| {
            Ok(Node {
                id: row.get::<_, String>(0)?,
                label: row.get::<_, String>(1)?,
                content: row.get::<_, String>(2)?,
            })
        })?;
        for row in rows {
            print_node(&row?);
            found = true;
        }
    } else {
        bail!("sham get: provide a <label> or --node-id <id>");
    }
    if !found {
        println!("(no nodes found)");
    }
    Ok(())
}

fn print_node(node: &Node) {
    println!("{} | {} | {}", node.id, node.label, node.content);
}
