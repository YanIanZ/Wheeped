//! The durability primitives the FILE backend is built on.
//!
//! Forcing a file makes its bytes durable but says nothing about the directory entry that names
//! it: a create or a rename is durable only once the directory holding it is forced. So a commit
//! forces files, renames them, and then forces their directory, and a failed force is a failed
//! commit.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// File force, directory force and atomic rename, as a seam.
///
/// [`Disk`] is the real one. Tests substitute a recording or failing one to prove the order in
/// which a commit makes things durable.
pub trait Durability: Send + Sync {
    /// Makes a file's content and metadata durable.
    fn force_file(&self, file: &Path) -> io::Result<()>;

    /// Makes a directory's entries durable: the creates and renames inside it.
    fn force_directory(&self, dir: &Path) -> io::Result<()>;

    /// Publishes `from` under `to` by one atomic rename. The temporary file always sits next to
    /// its target, so a refusal means the filesystem cannot rename atomically at all, and the
    /// caller fails the commit rather than copying.
    fn move_atomically(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    /// Creates a directory and its missing ancestors and makes the new entries durable: the parent
    /// of every directory created is forced, outermost first. Returns the directories created.
    fn create_directories_durably(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let absolute = std::path::absolute(dir)?;
        let mut missing: Vec<PathBuf> = absolute
            .ancestors()
            .take_while(|path| !path.is_dir())
            .map(Path::to_path_buf)
            .collect();
        if missing.is_empty() {
            return Ok(missing);
        }
        missing.reverse();
        fs::create_dir_all(dir)?;
        for child in &missing {
            if let Some(parent) = child.parent() {
                self.force_directory(parent)?;
            }
        }
        Ok(missing)
    }
}

/// The on-disk implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Disk;

/// The shared on-disk implementation.
pub static DISK: Disk = Disk;

static DEGRADED: AtomicU64 = AtomicU64::new(0);
static DEGRADED_REASON: OnceLock<String> = OnceLock::new();
static LOGGED: AtomicBool = AtomicBool::new(false);

/// Directory forces this process could not perform because the platform cannot open a directory.
#[must_use]
pub fn degraded_directory_forces() -> u64 {
    DEGRADED.load(Ordering::Relaxed)
}

/// Why directory forces are degraded, or `None` while none has been.
#[must_use]
pub fn degraded_reason() -> Option<&'static str> {
    DEGRADED_REASON.get().map(String::as_str)
}

impl Durability for Disk {
    fn force_file(&self, file: &Path) -> io::Result<()> {
        // Opened for writing: Windows refuses to flush a read-only handle.
        OpenOptions::new().write(true).open(file)?.sync_all()
    }

    fn force_directory(&self, dir: &Path) -> io::Result<()> {
        match File::open(dir) {
            Ok(handle) => handle.sync_all(),
            // Windows cannot open a directory this way. The rename's own durability is all there
            // is, and the gap is counted so that it is visible. Everywhere else a directory that
            // cannot be opened is a real failure and fails the commit.
            Err(refused) if cfg!(windows) => {
                degraded(dir, &refused);
                Ok(())
            }
            Err(refused) => Err(refused),
        }
    }
}

fn degraded(dir: &Path, why: &io::Error) {
    DEGRADED.fetch_add(1, Ordering::Relaxed);
    let reason = DEGRADED_REASON.get_or_init(|| why.to_string());
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "AWF cannot fsync directories on this platform ({reason}); directory entries such as {} rely on the rename alone. Commits are not fully durable against power loss here.",
            dir.display()
        );
    }
}

#[cfg(test)]
pub mod testing {
    //! A durability seam that records what was forced and can fail on demand.

    use std::sync::Mutex;

    use super::{DISK, Durability, Path, PathBuf, io};
    use crate::lock;

    /// One durability call, with the path relative to the recorder's root.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        File(PathBuf),
        Directory(PathBuf),
        Move(PathBuf, PathBuf),
    }

    type Failure = Box<dyn Fn(&Call) -> Option<io::Error> + Send + Sync>;

    /// Performs every call on disk, records it, and fails the ones a test asks it to.
    pub struct Recorder {
        root: PathBuf,
        calls: Mutex<Vec<Call>>,
        failure: Mutex<Option<Failure>>,
    }

    impl Recorder {
        pub fn new(root: &Path) -> Self {
            Self {
                root: root.to_path_buf(),
                calls: Mutex::new(Vec::new()),
                failure: Mutex::new(None),
            }
        }

        /// Fails every later call for which `failure` returns an error, before it reaches the disk.
        pub fn fail_when(
            &self,
            failure: impl Fn(&Call) -> Option<io::Error> + Send + Sync + 'static,
        ) {
            *lock(&self.failure) = Some(Box::new(failure));
        }

        pub fn stop_failing(&self) {
            *lock(&self.failure) = None;
        }

        pub fn calls(&self) -> Vec<Call> {
            lock(&self.calls).clone()
        }

        pub fn clear(&self) {
            lock(&self.calls).clear();
        }

        fn relative(&self, path: &Path) -> PathBuf {
            path.strip_prefix(&self.root).unwrap_or(path).to_path_buf()
        }

        fn record(&self, call: Call) -> io::Result<()> {
            let refused = lock(&self.failure)
                .as_ref()
                .and_then(|failure| failure(&call));
            if let Some(refused) = refused {
                return Err(refused);
            }
            lock(&self.calls).push(call);
            Ok(())
        }
    }

    impl Durability for Recorder {
        fn force_file(&self, file: &Path) -> io::Result<()> {
            self.record(Call::File(self.relative(file)))?;
            DISK.force_file(file)
        }

        fn force_directory(&self, dir: &Path) -> io::Result<()> {
            self.record(Call::Directory(self.relative(dir)))?;
            DISK.force_directory(dir)
        }

        fn move_atomically(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.record(Call::Move(self.relative(from), self.relative(to)))?;
            DISK.move_atomically(from, to)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Call, Recorder};
    use super::*;

    #[test]
    fn new_directories_have_their_parents_forced_outermost_first() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let recorder = Recorder::new(&root);
        let created = recorder
            .create_directories_durably(&root.join("a/b/c"))
            .unwrap();
        assert_eq!(
            created,
            [root.join("a"), root.join("a/b"), root.join("a/b/c")]
        );
        assert_eq!(
            recorder.calls(),
            [
                Call::Directory(PathBuf::new()),
                Call::Directory(PathBuf::from("a")),
                Call::Directory(PathBuf::from("a/b")),
            ]
        );
        recorder.clear();
        assert!(
            recorder
                .create_directories_durably(&root.join("a/b/c"))
                .unwrap()
                .is_empty()
        );
        assert!(
            recorder.calls().is_empty(),
            "an existing directory forces nothing"
        );
    }
}
