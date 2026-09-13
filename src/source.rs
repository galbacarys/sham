//! Source-of-truth YAML layer.
//!
//! Authors write and read decision units here; the SQLite cache (`store`) and
//! any embedding index are *derived* from this layer and stay disposable. A
//! project's memory lives in a single `memory.yaml` inside its `.memory/` dir,
//! co-located and versioned with the project's code.
//!
//! The on-disk shape is deliberately flat (a `project` header + a flat list of
//! `nodes`), with the tree layer expressed via `parent` edges — the exact same
//! model the DB uses, so a later `reconcile` can rebuild the cache verbatim
//! from these files. Identity (`id`) and freshness (`hash`) live in the YAML,
//! not only in the DB, which is what keeps the cache fully re-derivable.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use noyalib::compat::serde_yaml;

/// The canonical on-disk memory file: a project header plus a flat node list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryFile {
    pub project: String,
    #[serde(default)]
    pub nodes: Vec<Node>,
}

/// One decision unit: either an idea node (has a `label`, optional `parent`)
/// or a rationale leaf (`label` empty, `parent` set) attached under an idea.
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
    /// Parent id when this node is a rationale leaf / child in the tree layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

impl MemoryFile {
    pub fn new(project: impl Into<String>) -> Self {
        MemoryFile {
            project: project.into(),
            nodes: Vec::new(),
        }
    }
}

impl Node {
    /// Build a node from authoring inputs, computing its content hash here so
    /// hash generation lives in exactly one place.
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
}

/// SHA-256 of a node's own content, lowercase hex. This is the freshness
/// signal for the node itself. (The design's *embedding-unit* hash — a node
/// plus the identity+hash of its immediate children — is computed at reconcile,
/// where embeddings exist; it is deliberately out of scope here.)
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
    fn memory_file_round_trips_through_yaml() {
        let mut mem = MemoryFile::new("myproj");
        append_node(&mut mem, Node::new("01A", "billing", "chose Stripe", None));
        append_node(
            &mut mem,
            Node::new("01B", "", "idempotent webhooks", Some("01A".into())),
        );

        let yaml = to_yaml(&mem);
        let back = from_yaml(&yaml).expect("round-trip parse");

        assert_eq!(back.project, "myproj");
        assert_eq!(back, mem);
    }

    #[test]
    fn parent_is_omitted_when_absent_but_preserved_when_set() {
        let mut mem = MemoryFile::new("p");
        append_node(&mut mem, Node::new("01A", "root", "content", None));
        append_node(&mut mem, Node::new("01B", "", "leaf", Some("01A".into())));

        let yaml = to_yaml(&mem);
        // Everything before the leaf id ("01B") is the header + root block: it
        // must not carry a `parent:` line. The leaf block must carry one,
        // pointing back at the root id. (Ids serialize quoted, e.g. "01A".)
        let root_block = yaml.split("01B").next().unwrap();
        assert!(!root_block.lines().any(|l| l.trim().starts_with("parent:")));
        let leaf_block = yaml.split("01B").nth(1).unwrap();
        assert!(leaf_block.lines().any(|l| l.trim().starts_with("parent:")));
        assert!(leaf_block.contains("01A"));

        // And parsing restores the Option exactly.
        assert_eq!(from_yaml(&yaml).unwrap(), mem);
    }

    #[test]
    fn empty_memory_file_serializes_and_reparses() {
        let mem = MemoryFile::new("empty");
        let yaml = to_yaml(&mem);
        let back = from_yaml(&yaml).unwrap();
        assert_eq!(back, mem);
        assert!(back.nodes.is_empty());
    }

    #[test]
    fn parses_a_hand_authoring_style_document() {
        // Someone wrote/edited the file by hand; labels with special chars.
        let doc = "\
project: demo
nodes:
  - id: 01H
    hash: deadbeef
    label: \"site reliability\"
    content: prefer boring infra
  - id: 01I
    hash: c0ffee
    label: ''
    content: \"ops on-call rotation\"
    parent: 01H
";
        let mem = from_yaml(doc).unwrap();
        assert_eq!(mem.project, "demo");
        assert_eq!(mem.nodes.len(), 2);
        assert_eq!(mem.nodes[0].label, "site reliability");
        assert_eq!(mem.nodes[0].parent, None);
        assert_eq!(mem.nodes[1].parent.as_deref(), Some("01H"));
        // Reserializing keeps it lossless enough (id/hash preserved verbatim).
        assert!(to_yaml(&mem).contains("deadbeef"));
        assert!(to_yaml(&mem).contains("c0ffee"));
    }

    #[test]
    fn write_then_read_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("sham-yaml-test-{}", std::process::id()));
        let path = dir.join("memory.yaml");
        let mut mem = MemoryFile::new("roundtrip");
        append_node(&mut mem, Node::new("01X", "k", "v", None));

        write_file(&path, &mem).expect("write");
        let back = read_file(&path).expect("read");
        assert_eq!(back, mem);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
