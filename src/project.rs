//! Project registry and resolution.
//!
//! A user-global manifest (`~/.config/sham/config.yaml`, or `$SHAM_CONFIG`)
//! lists the projects whose memory dirs are included in the store. It is the
//! only piece that is *user-global and durable* — everything else derives from
//! a project's own YAML. A project is simply a directory containing a
//! `.memory/` marker, which is how an arbitrary cwd resolves to a project.
//!
//! The manifest path honours a `$SHAM_CONFIG` override (mirroring the existing
//! `$SHAM_DB` convention for the cache), so both tests and alternate installs
//! can point it somewhere other than the user's real `~/.config`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use noyalib::compat::serde_yaml;

/// The `.memory/` marker directory name inside a project.
pub const MEMORY_DIR_NAME: &str = ".memory";
/// The memory file name inside a project's `.memory/` dir.
pub const MEMORY_FILE_NAME: &str = "memory.yaml";
/// Default name of the manifest file inside `~/.config/sham/`.
pub const MANIFEST_FILE_NAME: &str = "config.yaml";

/// The user-global manifest, mapping registered project names to their
/// `.memory` dir path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub projects: Vec<Project>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: default_version(),
            projects: Vec::new(),
        }
    }
}

/// One registered project entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
}

fn default_version() -> u32 {
    1
}

/// Absolute path to the manifest file (`$SHAM_CONFIG`, else `~/.config/sham/`).
pub fn manifest_path() -> Result<PathBuf> {
    if let Some(p) = env::var_os("SHAM_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::config_dir().context("could not derive the config dir")?;
    Ok(base.join("sham").join(MANIFEST_FILE_NAME))
}

/// Read the manifest. A missing file is treated as an empty config.
pub fn read_config() -> Result<Config> {
    let path = manifest_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("could not read manifest {}", path.display()))?;
    serde_yaml::from_str(&text)
        .with_context(|| format!("manifest {} did not parse", path.display()))
}

/// Write the manifest to its standard location (creating dirs as needed).
pub fn save_config(cfg: &Config) -> Result<()> {
    let path = manifest_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(
        &path,
        serde_yaml::to_string(cfg).expect("serialize manifest"),
    )
    .with_context(|| format!("could not write manifest {}", path.display()))
}

/// Register (or re-register) a project in the manifest, upserting by name.
pub fn register_project(name: &str, memory_dir: &Path) -> Result<()> {
    let mut cfg = read_config()?;
    let abs = memory_dir
        .canonicalize()
        .unwrap_or_else(|_| memory_dir.to_path_buf());
    match cfg.projects.iter_mut().find(|p| p.name == name) {
        Some(entry) => entry.path = abs,
        None => cfg.projects.push(Project {
            name: name.into(),
            path: abs,
        }),
    }
    save_config(&cfg)
}

/// Look up a registered project by name.
pub fn lookup_project(name: &str) -> Result<Option<Project>> {
    Ok(read_config()?.projects.into_iter().find(|p| p.name == name))
}

/// Walk up from `start` (inclusive) toward the filesystem root, returning the
/// first directory that contains a `.memory` marker. This is how a nested cwd
/// maps to the nearest project — the design's "project implied by context".
pub fn resolve_memory_dir(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let marker = dir.join(MEMORY_DIR_NAME);
        if marker.is_dir() {
            return Some(marker);
        }
        cur = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that set `SHAM_CONFIG` so they can't race each other.
    static CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point the manifest at a unique temp dir for the duration of `f`, then
    /// restore the previous `SHAM_CONFIG` value (or unset it).
    fn with_temp_config<R>(f: impl FnOnce(&Path) -> R) -> R {
        let _guard = CONFIG_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("sham-manifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(MANIFEST_FILE_NAME);

        let prev = env::var_os("SHAM_CONFIG");
        env::set_var("SHAM_CONFIG", &path);
        let result = f(&dir);
        match prev {
            Some(v) => env::set_var("SHAM_CONFIG", v),
            None => env::remove_var("SHAM_CONFIG"),
        }
        let _ = fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn resolve_finds_nearest_ancestor_marker() {
        let root = std::env::temp_dir().join(format!("sham-resolve-{}", std::process::id()));
        let nested = root.join("deep/nested/dir");
        let memory = root.join(MEMORY_DIR_NAME);
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&memory).unwrap();

        // A nested dir (no marker of its own) resolves up to the root marker.
        assert_eq!(resolve_memory_dir(&nested), Some(memory.clone()));
        // The dir *containing* the marker resolves to it too.
        assert_eq!(resolve_memory_dir(&root), Some(memory.clone()));
        // Above the marker, nothing resolves.
        assert_eq!(resolve_memory_dir(std::path::Path::new("/")), None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn register_upserts_by_name_and_lookup_round_trips() {
        with_temp_config(|dir| {
            let m1 = dir.join("repo-a").join(MEMORY_DIR_NAME);
            let m2 = dir.join("repo-b").join(MEMORY_DIR_NAME);
            fs::create_dir_all(&m1).unwrap();
            fs::create_dir_all(&m2).unwrap();

            // Register A, then re-register A pointing elsewhere: should replace,
            // not duplicate. Also register B.
            register_project("alpha", &m1).unwrap();
            register_project("alpha", &m2).unwrap();
            register_project("beta", &m1).unwrap();

            let cfg = read_config().unwrap();
            assert_eq!(
                cfg.projects.len(),
                2,
                "re-register by name upserts, no dupes"
            );
            assert_eq!(cfg.version, 1);

            // The re-registered alpha now points at m2 (canonicalized).
            let a = lookup_project("alpha").unwrap().unwrap();
            assert_eq!(a.path, m2.canonicalize().unwrap());

            assert!(lookup_project("nope").unwrap().is_none());
        });
    }
}
