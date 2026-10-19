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

/// A directory that's removed when dropped, unless [`Scratch::persist`] moved it.
#[derive(Debug)]
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    pub fn new(root: &Path, prefix: &str) -> Result<Scratch> {
        fs::create_dir_all(root).with_context(|| format!("failed to create {}", root.display()))?;
        sweep(root);

        // The counter keeps a cancelled run that's still cleaning up from colliding with its
        // replacement in the same process.
        static RUNS: AtomicU64 = AtomicU64::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("{prefix}{PARTIAL}{}-{run}", std::process::id()));
        fs::create_dir(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(Scratch { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Renames the directory to `target` in one step, unless `target` exists. Another run may
    /// have made `target` first, in which case this one is removed.
    pub fn persist(self, target: &Path) -> Result<()> {
        if !target.exists() {
            fs::rename(&self.dir, target)
                .with_context(|| format!("failed to move {} into place", self.dir.display()))?;
        }
        Ok(())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Gone already after `persist` moved it.
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
    fn persists_by_renaming() {
        let root = tempfile::tempdir().unwrap();
        let scratch = Scratch::new(root.path(), "run").unwrap();
        fs::write(scratch.dir().join("voice.wav"), "voice").unwrap();
        let target = root.path().join("done");

        scratch.persist(&target).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("voice.wav")).unwrap(),
            "voice"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn keeps_an_existing_target_and_drops_the_duplicate() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("done");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("voice.wav"), "first").unwrap();
        let scratch = Scratch::new(root.path(), "run").unwrap();
        fs::write(scratch.dir().join("voice.wav"), "second").unwrap();

        scratch.persist(&target).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("voice.wav")).unwrap(),
            "first"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
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
