//! Source-of-truth YAML layer.
//!
//! Authors write and read decision units here; the SQLite cache (`store`) and
//! any embedding index are *derived* from this layer and stay disposable. A
//! project's memory lives in a single `memory.yaml` inside its `.memory/` dir,
//! co-located and versioned with the project's code.
//!
//! Shape: a `project` header, a `root` header naming the id of the project's
//! top node, and a flat `nodes` list with hierarchy via `parent` edges. The
//! project root node is a real node: it is the **only** parentless node, and
//! every other node's `parent` chain terminates at it. This makes project
//! membership a structural fact of the graph (walk up to the root) rather than
//! an inference from file location, and makes GC a simple
//! "reachable from each project root" walk. Same flat model the DB uses, so a
//! later `reconcile` can rebuild the cache verbatim from these files.
//!
//! Identity (`id`) and freshness (`hash`) live in the YAML, not only in the DB,
//! which is what keeps the cache fully re-derivable.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use noyalib::compat::serde_yaml;

/// The canonical on-disk memory file: a project header, its root-node id, and
/// a flat node list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryFile {
    pub project: String,
    /// Id of this project's root node. The root node is the single parentless
    /// node; every other node's parent chain terminates at it.
    pub root: String,
    #[serde(default)]
    pub nodes: Vec<Node>,
}

/// One decision unit: an idea node (a `label`, `parent` set) or a rationale
/// leaf (`label` empty, `parent` set). The only node with `parent: null` is the
/// project root node (see `Node::root`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Stable ULID — identity. Survives moves, renames, re-parenting.
    pub id: String,
    /// SHA-256 of the node's own content (label + content) — freshness signal.
    pub hash: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub content: String,
    /// Parent id. `None` is reserved for the project root node only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

impl MemoryFile {
    /// A fresh, empty-until-populated project memory rooted at `root_id`.
    /// Creates the project root node immediately so the file is valid from the
    /// start (exactly one parentless node).
    pub fn new(project: impl Into<String>, root_id: impl Into<String>) -> Self {
        let project = project.into();
        let root = root_id.into();
        let nodes = vec![Node::root(&root, &project)];
        MemoryFile {
            project,
            root,
            nodes,
        }
    }

    /// Enforce the tree invariant the whole design leans on:
    ///   1. exactly one parentless node, and it is the project root (`root`),
    ///   2. every `parent` id resolves to a node present in this file.
    ///
    /// Call before writing so the CLI never emits an invalid tree; a future
    /// `reconcile` will also run this per file.
    pub fn check_invariants(&self) -> Result<()> {
        let parentless: Vec<&Node> = self.nodes.iter().filter(|n| n.parent.is_none()).collect();
        if parentless.len() != 1 {
            bail!(
                "expected exactly one parentless node (the project root {}), found {}",
                self.root,
                parentless.len()
            );
        }
        if parentless[0].id != self.root {
            bail!(
                "the parentless node {} is not the declared root {}",
                parentless[0].id,
                self.root
            );
        }
        let ids: std::collections::HashSet<&str> =
            self.nodes.iter().map(|n| n.id.as_str()).collect();
        for node in &self.nodes {
            if let Some(pid) = &node.parent {
                if !ids.contains(pid.as_str()) {
                    bail!(
                        "node {} has a dangling parent {} (not present in this file)",
                        node.id,
                        pid
                    );
                }
            }
        }
        Ok(())
    }
}

impl Node {
    /// A general node from authoring inputs, computing its content hash here so
    /// hash generation lives in exactly one place. Pass `parent = Some(parent)`
    /// for every real node; only the project root uses `Node::root`.
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        content: impl Into<String>,
        parent: Option<String>,
    ) -> Self {
        let id = id.into();
        let label = label.into();
        let content = content.into();
        let hash = content_hash(&label, &content);
        Node {
            id,
            hash,
            label,
            content,
            parent,
        }
    }

    /// The project root node: a container/anchor, not a decision unit. It is
    /// the single legitimate parentless node; everything else parents to it
    /// (directly or transitively).
    pub fn root(id: impl Into<String>, name: &str) -> Self {
        let label = name.to_string();
        let content = String::new();
        let hash = content_hash(&label, &content);
        Node {
            id: id.into(),
            hash,
            label,
            content,
            parent: None,
        }
    }
}

/// SHA-256 of a node's own content, lowercase hex. This is the freshness signal
/// for the node itself. (The design's *embedding-unit* hash — a node plus the
/// identity+hash of its immediate children — is computed at reconcile, where
/// embeddings exist; it is deliberately out of scope here.)
pub fn content_hash(label: &str, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(label.as_bytes());
    h.update(b"\n");
    h.update(content.as_bytes());
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

/// Serialize a memory file to its canonical YAML string.
pub fn to_yaml(mem: &MemoryFile) -> String {
    serde_yaml::to_string(mem).expect("serializing a MemoryFile to YAML cannot fail")
}

/// Parse a memory file from a YAML string.
pub fn from_yaml(s: &str) -> Result<MemoryFile> {
    serde_yaml::from_str(s).context("memory YAML did not parse")
}

/// Read a project's memory file from disk.
pub fn read_file(path: &Path) -> Result<MemoryFile> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("could not read memory file {}", path.display()))?;
    from_yaml(&text)
}

/// Write a memory file to disk (creating parent dirs as needed).
pub fn write_file(path: &Path, mem: &MemoryFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(path, to_yaml(mem))
        .with_context(|| format!("could not write memory file {}", path.display()))?;
    Ok(())
}

/// Append a node to an in-memory memory file.
pub fn append_node(mem: &mut MemoryFile, node: Node) {
    mem.nodes.push(node);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A builder-friendly valid tree for tests: project root + one root idea.
    fn tree() -> MemoryFile {
        let mut mem = MemoryFile::new("acme", "ROOT");
        append_node(
            &mut mem,
            Node::new("01A", "billing", "chose Stripe", Some("ROOT".into())),
        );
        append_node(
            &mut mem,
            Node::new("01B", "", "idempotent webhooks", Some("01A".into())),
        );
        mem.check_invariants().expect("test tree must be valid");
        mem
    }

    #[test]
    fn content_hash_is_stable_and_sensitive_to_every_field() {
        let a = content_hash("billing", "chose Stripe");
        assert_eq!(a, content_hash("billing", "chose Stripe"));
        assert_ne!(a, content_hash("billing", "chose PayPal"));
        assert_ne!(a, content_hash("billingz", "chose Stripe"));
        // Empty label contributes, so two leaves with different text differ.
        assert_ne!(content_hash("", "x"), content_hash("", "y"));
        // Empty content vs a label that would produce the same raw concat differ.
        assert_ne!(content_hash("", "ab"), content_hash("a", "b"));
    }

    #[test]
    fn node_new_sets_hash_from_label_and_content() {
        let n = Node::new("id1", "billing", "chose Stripe", None);
        assert_eq!(n.hash, content_hash("billing", "chose Stripe"));
        let leaf = Node::new("id2", "", "justification", Some("id1".into()));
        assert_eq!(leaf.label, "");
        assert_eq!(leaf.parent.as_deref(), Some("id1"));
    }

    #[test]
    fn project_root_node_is_the_parentless_anchor() {
        let mem = MemoryFile::new("acme", "ROOT");
        assert_eq!(mem.project, "acme");
        assert_eq!(mem.root, "ROOT");
        // Exactly the root node, parentless, label = project name.
        assert_eq!(mem.nodes.len(), 1);
        let root = &mem.nodes[0];
        assert_eq!(root.id, "ROOT");
        assert_eq!(root.label, "acme");
        assert_eq!(root.content, "");
        assert_eq!(root.parent, None);
        assert_eq!(root.hash, content_hash("acme", ""));
        mem.check_invariants().expect("fresh file is valid");
    }

    #[test]
    fn every_real_node_parents_transitively_to_the_root() {
        let mem = tree();
        mem.check_invariants().unwrap();
        // Walk 01A's ancestor chain: 01A -> ROOT.
        let a = mem.nodes.iter().find(|n| n.id == "01A").unwrap();
        assert_eq!(a.parent.as_deref(), Some("ROOT"));
        // And a top-level idea parents to the project root, not to null.
        let mut leaf_idea = MemoryFile::new("acme", "ROOT");
        append_node(
            &mut leaf_idea,
            Node::new("09Z", "standalone", "idea", Some("ROOT".into())),
        );
        let parentless = leaf_idea
            .nodes
            .iter()
            .filter(|n| n.parent.is_none())
            .count();
        assert_eq!(parentless, 1, "only the root may be parentless");
    }

    #[test]
    fn memory_file_round_trips_through_yaml() {
        let mem = tree();
        let yaml = to_yaml(&mem);
        let back = from_yaml(&yaml).expect("round-trip parse");
        assert_eq!(back.project, "acme");
        assert_eq!(back.root, "ROOT");
        assert_eq!(back, mem);
        back.check_invariants()
            .expect("round-tripped file still valid");
    }

    #[test]
    fn serialized_root_is_parentless_and_others_are_not() {
        let mem = tree();
        let yaml = to_yaml(&mem);
        // Exactly one node carries no `parent:` line — the root. Every other
        // node (01A, 01B) serializes one.
        let parent_lines = yaml
            .lines()
            .filter(|l| l.trim_start().starts_with("parent:"))
            .count();
        assert_eq!(parent_lines, 2, "only the project root is parentless");
    }

    #[test]
    fn invariants_reject_dangling_parent() {
        let mut mem = MemoryFile::new("acme", "ROOT");
        // Parent points at a node that does not exist in the file.
        append_node(
            &mut mem,
            Node::new("01A", "idea", "content", Some("GHOST".into())),
        );
        assert!(mem.check_invariants().is_err());
    }

    #[test]
    fn invariants_reject_more_than_one_parentless_node() {
        let mut mem = MemoryFile::new("acme", "ROOT");
        // A second parentless node alongside the root.
        append_node(&mut mem, Node::new("01A", "idea", "content", None));
        assert!(mem.check_invariants().is_err());
    }

    #[test]
    fn invariants_reject_declared_root_not_matching_the_parentless_node() {
        // `root` header points at GHOST, but the actual parentless node is ROOT.
        let mem = MemoryFile {
            project: "acme".into(),
            root: "GHOST".into(),
            nodes: vec![Node::new("ROOT", "acme", "", None)],
        };
        assert!(mem.check_invariants().is_err());
    }

    #[test]
    fn parses_a_hand_authoring_style_document() {
        // Someone wrote/edited the file by hand; labels with special chars.
        let doc = "\
project: demo
root: \"ROOT\"
nodes:
  - id: \"ROOT\"
    hash: deadbeef
    label: demo
    content: ''
  - id: \"01H\"
    hash: c0ffee
    label: \"site reliability\"
    content: prefer boring infra
    parent: \"ROOT\"
  - id: \"01I\"
    hash: c0ffee2
    label: ''
    content: \"ops on-call rotation\"
    parent: \"01H\"
";
        let mem = from_yaml(doc).unwrap();
        assert_eq!(mem.project, "demo");
        assert_eq!(mem.root, "ROOT");
        assert_eq!(mem.nodes.len(), 3);
        assert_eq!(mem.nodes[0].parent, None);
        assert_eq!(mem.nodes[1].parent.as_deref(), Some("ROOT"));
        assert_eq!(mem.nodes[2].parent.as_deref(), Some("01H"));
        mem.check_invariants().expect("hand-written valid tree");
        // Reserializing keeps id/hash verbatim.
        let yaml = to_yaml(&mem);
        assert!(yaml.contains("deadbeef"));
        assert!(yaml.contains("c0ffee"));
    }

    #[test]
    fn write_then_read_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("sham-yaml-test-{}", std::process::id()));
        let path = dir.join("memory.yaml");
        let mem = tree();
        write_file(&path, &mem).expect("write");
        let back = read_file(&path).expect("read");
        assert_eq!(back, mem);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
