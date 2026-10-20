//! Working directories under vt's cache, for files a run needs only while it runs.
//!
//! Each lives in `<root>/<prefix>.partial-<pid>-<n>` and is removed when dropped. Directories
//! left by a killed process are swept by the next run.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};

use crate::quote::quote;

const PARTIAL: &str = ".partial-";

/// `~/.cache/video-tools/<tool>`, or under `$XDG_CACHE_HOME`.
pub fn cache_dir(tool: &str) -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".cache")))
        .context("no cache directory: neither XDG_CACHE_HOME nor HOME is set")?;
    Ok(base.join("video-tools").join(tool))
}

/// A directory that's removed when dropped.
#[derive(Debug)]
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    pub fn new(root: &Path, prefix: &str) -> Result<Scratch> {
        fs::create_dir_all(root).with_context(|| format!("failed to create {}", quote(root)))?;
        sweep(root);

        // The counter keeps a cancelled run that's still cleaning up from colliding with its
        // replacement in the same process.
        static RUNS: AtomicU64 = AtomicU64::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("{prefix}{PARTIAL}{}-{run}", std::process::id()));
        fs::create_dir(&dir).with_context(|| format!("failed to create {}", quote(&dir)))?;
        Ok(Scratch { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Removes partial directories whose process has died.
fn sweep(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((_, owner)) = name.to_str().and_then(|n| n.split_once(PARTIAL)) else {
            continue;
        };
        let pid = owner.split('-').next().unwrap_or_default();
        if !Path::new("/proc").join(pid).exists() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_the_directory_when_dropped() {
        let root = tempfile::tempdir().unwrap();
        let scratch = Scratch::new(root.path(), "run").unwrap();
        fs::write(scratch.dir().join("input.wav"), "partial").unwrap();

        drop(scratch);

        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn sweeps_directories_of_dead_processes() {
        let root = tempfile::tempdir().unwrap();
        // No process has this pid: Linux caps pids at 2^22.
        let dead = root.path().join("run.partial-99999999-0");
        fs::create_dir(&dead).unwrap();

        let live = Scratch::new(root.path(), "run").unwrap();

        assert!(!dead.exists());
        assert!(live.dir().exists());
    }
}
