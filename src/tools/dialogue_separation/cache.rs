//! Caches separated stems, so separating a file again skips inference.
//!
//! Entries live in `~/.cache/video-tools/dialogue-separation/<key>/` (or under
//! `$XDG_CACHE_HOME`). The key covers the backend, the file's size and mtime, and a hash of its
//! first and last MiB. Hashing whole recordings would take minutes, and an edit that keeps the
//! size and mtime and touches neither end is not a realistic way for a video to change.
//!
//! A run writes into a `<key>.partial-*` directory and renames it into place when it finishes, so
//! an entry is either complete or absent. Partial directories left by a killed process are swept
//! on the next run.

use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result};

use super::{Backend, protocol::Stats};

/// Bump when the Python stage's output changes, so stale stems aren't reused.
const FORMAT: u32 = 1;

const SAMPLE_BYTES: u64 = 1 << 20;

pub const VOICE: &str = "voice.wav";
pub const AMBIENCE: &str = "ambience.wav";
const STATS: &str = "stats.json";

const PARTIAL: &str = ".partial-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key(String);

impl Key {
    pub fn of(path: &Path, backend: Backend) -> Result<Key> {
        let mut file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let meta = file.metadata().context("failed to read file metadata")?;
        let mtime = meta
            .modified()
            .context("failed to read mtime")?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let size = meta.len();

        let mut head = Vec::new();
        (&mut file).take(SAMPLE_BYTES).read_to_end(&mut head)?;
        let mut tail = Vec::new();
        file.seek(SeekFrom::Start(size.saturating_sub(SAMPLE_BYTES)))?;
        file.take(SAMPLE_BYTES).read_to_end(&mut tail)?;
        Ok(Key::from_parts(backend, size, mtime, &head, &tail))
    }

    fn from_parts(backend: Backend, size: u64, mtime_ns: u128, head: &[u8], tail: &[u8]) -> Key {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&FORMAT.to_le_bytes());
        hasher.update(&size.to_le_bytes());
        hasher.update(&mtime_ns.to_le_bytes());
        // Length prefixes keep the head/tail boundary unambiguous for files under 2 MiB.
        for sample in [head, tail] {
            hasher.update(&(sample.len() as u64).to_le_bytes());
            hasher.update(sample);
        }
        let hash = hasher.finalize().to_hex();
        Key(format!("{}-{}", backend.name(), &hash[..32]))
    }
}

pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn new() -> Result<Cache> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|home| home.join(".cache")))
            .context("no cache directory: neither XDG_CACHE_HOME nor HOME is set")?;
        Ok(Cache {
            root: base.join("video-tools").join("dialogue-separation"),
        })
    }

    pub fn get(&self, key: &Key) -> Option<Entry> {
        let dir = self.root.join(&key.0);
        let stats = fs::read(dir.join(STATS)).ok()?;
        Some(Entry {
            stats: serde_json::from_slice(&stats).ok()?,
            dir,
        })
    }

    /// Starts a new entry in a fresh partial directory.
    pub fn begin(&self, key: &Key) -> Result<Pending> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        self.sweep();

        // The counter keeps a cancelled run that's still cleaning up from colliding with its
        // replacement in the same process.
        static RUNS: AtomicU64 = AtomicU64::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = self
            .root
            .join(format!("{}{PARTIAL}{}-{run}", key.0, std::process::id()));
        fs::create_dir(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(Pending {
            dir,
            done: self.root.join(&key.0),
        })
    }

    /// Removes partial directories whose process has died.
    fn sweep(&self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
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
}

/// A complete cache entry.
#[derive(Debug)]
pub struct Entry {
    dir: PathBuf,
    pub stats: Stats,
}

impl Entry {
    pub fn stem(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// An entry being written. Dropping it without [`Pending::commit`] deletes it.
pub struct Pending {
    dir: PathBuf,
    done: PathBuf,
}

impl Pending {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Keeps only the stems, then moves the directory into place.
    pub fn commit(self, stats: &Stats) -> Result<Entry> {
        for entry in fs::read_dir(&self.dir)?.flatten() {
            let name = entry.file_name();
            if name != VOICE && name != AMBIENCE {
                fs::remove_file(entry.path())?;
            }
        }
        fs::write(self.dir.join(STATS), serde_json::to_vec(stats)?)?;
        // Another process may have finished the same file first. Its entry is as good as ours.
        if !self.done.exists() {
            fs::rename(&self.dir, &self.done)
                .with_context(|| format!("failed to move {} into place", self.dir.display()))?;
        }
        Ok(Entry {
            dir: self.done.clone(),
            stats: stats.clone(),
        })
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // After a commit this is gone, or holds a duplicate of a concurrent run's entry.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::OpenOptions, io::Write, time::SystemTime};

    use super::*;

    fn key(path: &Path) -> Key {
        Key::of(path, Backend::Sam).unwrap()
    }

    /// Overwrites one byte, then restores the mtime, like an edit that slips past it.
    fn poke(path: &Path, offset: u64) {
        let mtime = fs::metadata(path).unwrap().modified().unwrap();
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&[0xff]).unwrap();
        file.set_modified(mtime).unwrap();
    }

    fn file_of(dir: &Path, len: u64) -> PathBuf {
        let path = dir.join("video.mkv");
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn keys_are_prefixed_with_the_backend() {
        let key = Key::from_parts(Backend::Tiger, 1, 2, b"head", b"tail");

        assert_eq!(key.0.split_once('-').unwrap().0, "tiger");
        assert_eq!(key.0.len(), "tiger-".len() + 32);
    }

    #[test]
    fn keys_differ_by_backend() {
        assert_ne!(
            Key::from_parts(Backend::Sam, 1, 2, b"head", b"tail"),
            Key::from_parts(Backend::Tiger, 1, 2, b"head", b"tail")
        );
    }

    #[test]
    fn keys_change_with_the_first_mib() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_of(dir.path(), 3 << 20);
        let before = key(&path);

        poke(&path, 10);

        assert_ne!(key(&path), before);
    }

    #[test]
    fn keys_change_with_the_last_mib() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_of(dir.path(), 3 << 20);
        let before = key(&path);

        poke(&path, (3 << 20) - 1);

        assert_ne!(key(&path), before);
    }

    #[test]
    fn keys_ignore_the_middle_of_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_of(dir.path(), 3 << 20);
        let before = key(&path);

        poke(&path, 3 << 19);

        assert_eq!(key(&path), before);
    }

    #[test]
    fn keys_change_with_the_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_of(dir.path(), 100);
        let before = key(&path);

        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();

        assert_ne!(key(&path), before);
    }

    #[test]
    fn keys_cover_small_files_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_of(dir.path(), 100);
        let before = key(&path);

        poke(&path, 50);

        assert_ne!(key(&path), before);
    }

    fn stats() -> Stats {
        Stats {
            device: "cpu".into(),
            elapsed_s: 1.5,
            peak_vram_mb: None,
        }
    }

    #[test]
    fn commits_only_the_stems() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache {
            root: root.path().into(),
        };
        let key = Key::from_parts(Backend::Fake, 1, 2, b"", b"");
        let pending = cache.begin(&key).unwrap();
        for name in [VOICE, AMBIENCE, "input.wav"] {
            fs::write(pending.dir().join(name), name).unwrap();
        }

        let entry = pending.commit(&stats()).unwrap();

        assert_eq!(fs::read_to_string(entry.stem(VOICE)).unwrap(), VOICE);
        let mut names: Vec<_> = fs::read_dir(root.path().join(&key.0))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, [AMBIENCE, STATS, VOICE]);
        assert_eq!(cache.get(&key).unwrap().stats, stats());
    }

    #[test]
    fn misses_until_committed_and_removes_abandoned_entries() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache {
            root: root.path().into(),
        };
        let key = Key::from_parts(Backend::Fake, 1, 2, b"", b"");

        let pending = cache.begin(&key).unwrap();
        fs::write(pending.dir().join(VOICE), "partial").unwrap();
        assert!(cache.get(&key).is_none());
        drop(pending);

        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn sweeps_partial_entries_of_dead_processes() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache {
            root: root.path().into(),
        };
        // No process has this pid: Linux caps pids at 2^22.
        let dead = root.path().join("sam-abc.partial-99999999-0");
        fs::create_dir(&dead).unwrap();
        let key = Key::from_parts(Backend::Fake, 1, 2, b"", b"");

        let live = cache.begin(&key).unwrap();

        assert!(!dead.exists());
        assert!(live.dir().exists());
    }
}
