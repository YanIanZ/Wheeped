//! The FILE backend's atomic persistence primitive.
//!
//! A snapshot is written to a temporary generation, verified by reading it back, and only then
//! published by replacing the `CURRENT` pointer. That replacement is the single commit point. A
//! crash at any earlier step leaves the previous generation authoritative, and leftovers are
//! removed the next time the store is opened by a role that accepts commits.
//!
//! On disk: `CURRENT` holds `"<N>\t<sha256 of N's manifest>"`; `generations/N/` holds the blobs and
//! `manifest`. A manifest is UTF-8 lines. Format 1 is blob lines only, one
//! `"<blob>\t<length>\t<sha256>"` per blob. Format 2 is written only for a chained generation:
//! `"@format\t2"`, then `"@base\t<M>\t<sha256 of M's manifest>"`, then the blob lines. A manifest
//! with an `@` line this build does not know is corrupt, never ignored.
//!
//! Durability order within a commit: a directory chain created for the first commit is made
//! durable first; every blob and the manifest are written, then forced in one pass, then the
//! temporary generation directory is forced; after verification the directory is renamed and
//! `generations/` forced; the pointer is written, forced, renamed and the root forced. Whatever a
//! snapshot references must be durable before a commit is called: this store forces nothing on the
//! caller's behalf.
//!
//! Retention: the newest retained committed generations are kept, plus every generation one of
//! them is chained to, transitively; everything else is deleted after a commit. That deletion runs
//! past the commit point, so when it fails the commit still succeeds and the next commit deletes
//! again.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::durability::{DISK, Durability};
use crate::error::{AwfError, Result};
use crate::hash::sha256_hex;
use crate::lock;
use crate::object_store::{publish_failure, remove_file_if_exists};
use crate::role::WorldRole;
use crate::text::{has_sha256_length, parse_number, split_java, trim_java};

/// The pointer file naming the committed generation.
pub const CURRENT: &str = "CURRENT";
/// The directory holding one directory per generation.
pub const GENERATIONS: &str = "generations";
const MANIFEST: &str = "manifest";
const TEMP_SUFFIX: &str = ".tmp";
/// Manifest header keys; a blob name can never start with `@`.
const FORMAT_KEY: &str = "@format";
const BASE_KEY: &str = "@base";
/// The manifest format this build writes for a chained generation; format 1 has no header.
const FORMAT: u64 = 2;
const MAX_BLOB_NAME: usize = 128;

/// Points in the commit sequence, for crash-injection tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Every blob and the manifest are written and forced.
    SnapshotWritten,
    /// The temporary generation was read back and matches.
    Verified,
    /// The generation directory has its final name; it is not authoritative yet.
    GenerationPublished,
    /// `CURRENT` names the new generation.
    Committed,
}

/// Called at each [`Stage`]; returning an error there simulates a crash at that stage.
pub type StageHook = dyn Fn(Stage) -> Result<()> + Send + Sync;

/// A committed generation's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    /// The generation's number, from 1.
    pub number: u64,
    /// The generation this one is chained to, 0 for none.
    pub base: u64,
    /// The SHA-256 of this generation's manifest: what a child chains to.
    pub manifest_sha: String,
    /// The base's manifest SHA-256 as recorded at commit time.
    pub base_manifest_sha: Option<String>,
    /// Blob name to bytes.
    pub blobs: BTreeMap<String, Vec<u8>>,
}

impl Generation {
    /// Whether this generation is chained to an earlier one.
    #[must_use]
    pub const fn has_base(&self) -> bool {
        self.base > 0
    }
}

/// A warning for cleanup that failed after a commit point: at most one line a minute per store,
/// with the failures counted in between. Logging only; the next commit retries the cleanup.
#[derive(Default)]
pub struct AfterCommitWarning {
    logged_at: Option<Instant>,
    failures: u64,
}

impl AfterCommitWarning {
    const INTERVAL: Duration = Duration::from_secs(60);

    /// Counts a failure and logs it unless a line was logged within the interval.
    pub fn failed(&mut self, message: &str, failure: &dyn Error) {
        self.failures += 1;
        let now = Instant::now();
        if self
            .logged_at
            .is_some_and(|logged_at| now.duration_since(logged_at) < Self::INTERVAL)
        {
            return;
        }
        tracing::warn!(
            "{message} ({} failure(s) since the last such warning): {failure}",
            self.failures
        );
        self.logged_at = Some(now);
        self.failures = 0;
    }
}

struct Pointer {
    generation: u64,
    manifest_sha: String,
}

/// A parsed manifest: the header and the blob lines.
struct Manifest {
    base: u64,
    base_manifest_sha: Option<String>,
    blobs: Vec<BlobLine>,
}

struct BlobLine {
    name: String,
    length: u64,
    sha: String,
}

/// Atomic generation commits for one directory.
pub struct GenerationStore {
    root: PathBuf,
    role: WorldRole,
    retained_generations: usize,
    hook: Option<Arc<StageHook>>,
    durability: Arc<dyn Durability>,
    /// Serializes every operation, and holds the state of the failed-prune warning.
    monitor: Mutex<AfterCommitWarning>,
}

impl GenerationStore {
    /// Opens a store on the real disk.
    ///
    /// A role that accepts commits removes what an interrupted commit left behind. Any other role
    /// changes nothing on disk, so it cannot delete a generation a live writer of the same
    /// directory has published but not committed yet; such a reader reads only what `CURRENT`
    /// names.
    pub fn open(root: &Path, role: WorldRole, retained_generations: usize) -> Result<Self> {
        Self::open_with(root, role, retained_generations, None, Arc::new(DISK))
    }

    /// As [`Self::open`], with a crash hook and a durability seam.
    pub fn open_with(
        root: &Path,
        role: WorldRole,
        retained_generations: usize,
        hook: Option<Arc<StageHook>>,
        durability: Arc<dyn Durability>,
    ) -> Result<Self> {
        if retained_generations < 1 {
            return Err(AwfError::InvalidArgument(
                "retainedGenerations must be at least 1".to_string(),
            ));
        }
        let store = Self {
            root: root.to_path_buf(),
            role,
            retained_generations,
            hook,
            durability,
            monitor: Mutex::new(AfterCommitWarning::default()),
        };
        if role.accepts_commits() {
            store.recover()?;
        }
        Ok(store)
    }

    /// The committed generation number, or 0 when nothing has been committed.
    pub fn current_generation(&self) -> Result<u64> {
        let _monitor = lock(&self.monitor);
        Ok(self.read_pointer()?.map_or(0, |pointer| pointer.generation))
    }

    /// Commits a full snapshot as the next generation, chained to nothing.
    pub fn commit(&self, snapshot: &BTreeMap<String, Vec<u8>>) -> Result<u64> {
        self.commit_chained(snapshot, 0)
    }

    /// Commits a snapshot as the next generation, chained to `base`: a committed generation this
    /// one depends on, kept on disk as long as this one is retained, or 0 for none.
    pub fn commit_chained(&self, snapshot: &BTreeMap<String, Vec<u8>>, base: u64) -> Result<u64> {
        let mut monitor = lock(&self.monitor);
        if !self.role.accepts_commits() {
            return Err(AwfError::InvalidState(format!(
                "a {} world does not accept commits",
                self.role
            )));
        }
        for name in snapshot.keys() {
            if !usable_blob_name(name) {
                return Err(AwfError::InvalidArgument(format!(
                    "unusable blob name: {name}"
                )));
            }
        }
        let current = self.read_pointer()?.map_or(0, |pointer| pointer.generation);
        if base > current {
            return Err(AwfError::InvalidArgument(format!(
                "base generation {base} is not committed (current is {current})"
            )));
        }
        let next = current.max(self.highest_generation_dir()?) + 1;
        let generations = self.root.join(GENERATIONS);
        // The base's manifest hash is read before anything is written: a child never chains to a
        // base it could not read, and the link is checked again by `read_base`.
        let base_sha = if base == 0 {
            None
        } else {
            Some(sha256_hex(&self.manifest_of(base)?))
        };

        // A fresh store: the entries naming generations/, the root and any ancestor are made
        // durable before the generation that depends on them is.
        self.durability.create_directories_durably(&generations)?;
        let temp = generations.join(format!("{next}{TEMP_SUFFIX}"));
        delete_tree(&temp)?;
        fs::create_dir(&temp)?;

        // 1. Snapshot -> temporary generation: write every blob and the manifest, then make them
        //    durable in one pass: the files, then the directory that holds their entries.
        let mut manifest = String::new();
        if let Some(base_sha) = &base_sha {
            // Only a chained generation needs the header; an unchained one stays format 1 so that
            // a build without this parser can still read it.
            let _ = writeln!(manifest, "{FORMAT_KEY}\t{FORMAT}");
            let _ = writeln!(manifest, "{BASE_KEY}\t{base}\t{base_sha}");
        }
        for (name, bytes) in snapshot {
            write_file(&temp.join(name), bytes)?;
            let _ = writeln!(manifest, "{name}\t{}\t{}", bytes.len(), sha256_hex(bytes));
        }
        let manifest = manifest.into_bytes();
        write_file(&temp.join(MANIFEST), &manifest)?;
        for name in snapshot.keys() {
            self.durability.force_file(&temp.join(name))?;
        }
        self.durability.force_file(&temp.join(MANIFEST))?;
        self.durability.force_directory(&temp)?;
        self.reached(Stage::SnapshotWritten)?;

        // 2. Verify by reading back what the filesystem actually holds.
        load(next, &temp, &manifest)?;
        self.reached(Stage::Verified)?;

        // 3. Publish the generation directory under its final name. Still not authoritative.
        self.publish(&temp, &generations.join(next.to_string()))?;
        self.durability.force_directory(&generations)?;
        self.reached(Stage::GenerationPublished)?;

        // 4. Commit: replace the pointer. Before this the previous generation is current.
        let pointer_temp = self.root.join(format!("{CURRENT}{TEMP_SUFFIX}"));
        let pointer = format!("{next}\t{}\n", sha256_hex(&manifest));
        write_file(&pointer_temp, pointer.as_bytes())?;
        self.durability.force_file(&pointer_temp)?;
        self.publish(&pointer_temp, &self.root.join(CURRENT))?;
        self.durability.force_directory(&self.root)?;
        self.reached(Stage::Committed)?;

        // Past the commit point: the generation is current and durable whatever happens below. A
        // failed prune must not report the commit as failed, or the caller retries and commits yet
        // another generation.
        if let Err(prune_failed) = self.prune(next) {
            monitor.failed(
                &format!(
                    "AWF store {} committed generation {next}, but deleting the generations it no longer retains failed; the commit stands, old generations stay on disk until a later commit deletes them",
                    self.root.display()
                ),
                &prune_failed,
            );
        }
        Ok(next)
    }

    /// Reads the committed generation, verifying every blob against the manifest and the manifest
    /// against `CURRENT`. `None` when nothing has been committed.
    pub fn read(&self) -> Result<Option<Generation>> {
        let _monitor = lock(&self.monitor);
        match self.read_pointer()? {
            Some(pointer) => self
                .read_verified(pointer.generation, Some(&pointer.manifest_sha))
                .map(Some),
            None => Ok(None),
        }
    }

    /// Reads a committed generation that is still on disk, verified against its own manifest (and
    /// against `CURRENT` when it is the current one). Use [`Self::read_base`] to follow a chain,
    /// which also checks the link.
    pub fn read_number(&self, number: u64) -> Result<Generation> {
        let _monitor = lock(&self.monitor);
        match self.read_pointer()? {
            Some(pointer) if (1..=pointer.generation).contains(&number) => {
                let expected = (number == pointer.generation).then_some(pointer.manifest_sha);
                self.read_verified(number, expected.as_deref())
            }
            _ => Err(AwfError::InvalidArgument(format!(
                "generation {number} is not committed"
            ))),
        }
    }

    /// Reads the generation `child` is chained to and checks that its manifest is the one the
    /// child recorded. `None` when the child has no base.
    pub fn read_base(&self, child: &Generation) -> Result<Option<Generation>> {
        if !child.has_base() {
            return Ok(None);
        }
        let _monitor = lock(&self.monitor);
        let base = self.read_verified(child.base, None)?;
        if child.base_manifest_sha.as_deref() != Some(base.manifest_sha.as_str()) {
            return Err(AwfError::CorruptGeneration(format!(
                "generation {} is chained to a manifest of generation {} that is not the one on disk",
                child.number, child.base
            )));
        }
        Ok(Some(base))
    }

    /// Every retained committed generation that still verifies, newest first: the newest retained
    /// ones on disk, not the chain-only ancestors kept for them. A retained generation that fails
    /// verification is skipped, not returned: callers use this to decide what must be kept, and
    /// damaged data keeps nothing alive.
    pub fn read_retained(&self) -> Result<Vec<Generation>> {
        let _monitor = lock(&self.monitor);
        let Some(pointer) = self.read_pointer()? else {
            return Ok(Vec::new());
        };
        let mut retained = Vec::new();
        let numbers = self.committed_numbers_newest_first(pointer.generation)?;
        for number in numbers.into_iter().take(self.retained_generations) {
            let expected = (number == pointer.generation).then_some(pointer.manifest_sha.as_str());
            match self.read_verified(number, expected) {
                Ok(generation) => retained.push(generation),
                Err(damaged) if damaged.is_io() => {}
                Err(refused) => return Err(refused),
            }
        }
        Ok(retained)
    }

    fn reached(&self, stage: Stage) -> Result<()> {
        self.hook.as_ref().map_or(Ok(()), |hook| hook(stage))
    }

    fn generation_dir(&self, number: u64) -> PathBuf {
        self.root.join(GENERATIONS).join(number.to_string())
    }

    fn read_verified(
        &self,
        number: u64,
        expected_manifest_sha: Option<&str>,
    ) -> Result<Generation> {
        let manifest = self.manifest_of(number)?;
        if expected_manifest_sha.is_some_and(|expected| sha256_hex(&manifest) != expected) {
            return Err(AwfError::CorruptGeneration(format!(
                "generation {number} manifest does not match CURRENT"
            )));
        }
        load(number, &self.generation_dir(number), &manifest)
    }

    fn recover(&self) -> Result<()> {
        let generations = self.root.join(GENERATIONS);
        remove_file_if_exists(&self.root.join(format!("{CURRENT}{TEMP_SUFFIX}")))?;
        if !generations.is_dir() {
            return Ok(());
        }
        let current = self.read_pointer()?.map_or(0, |pointer| pointer.generation);
        for entry in fs::read_dir(&generations)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Unfinished writes, and generations published but never committed.
            let uncommitted = parse_number(&name).is_some_and(|number| number > current);
            if name.ends_with(TEMP_SUFFIX) || uncommitted {
                delete_tree(&entry.path())?;
            }
        }
        Ok(())
    }

    /// Deletes every committed generation that is neither one of the newest retained ones nor
    /// chained to, transitively, by one of them. A generation whose manifest cannot be read keeps
    /// nothing alive through it.
    fn prune(&self, current: u64) -> Result<()> {
        let committed = self.committed_numbers_newest_first(current)?;
        let mut keep: HashSet<u64> = HashSet::new();
        let mut chain: VecDeque<u64> = VecDeque::new();
        for number in committed.iter().take(self.retained_generations) {
            keep.insert(*number);
            chain.push_back(*number);
        }
        while let Some(number) = chain.pop_front() {
            let base = self.base_of(number);
            if base > 0 && keep.insert(base) {
                chain.push_back(base);
            }
        }
        for number in committed {
            if !keep.contains(&number) {
                delete_tree(&self.generation_dir(number))?;
            }
        }
        Ok(())
    }

    /// The base recorded in a generation's manifest, 0 when none or unreadable.
    fn base_of(&self, number: u64) -> u64 {
        self.manifest_of(number)
            .and_then(|manifest| parse(number, &self.generation_dir(number), &manifest))
            .map_or(0, |manifest| manifest.base)
    }

    fn manifest_of(&self, number: u64) -> Result<Vec<u8>> {
        match fs::read(self.generation_dir(number).join(MANIFEST)) {
            Ok(manifest) => Ok(manifest),
            Err(missing) if missing.kind() == ErrorKind::NotFound => Err(
                AwfError::CorruptGeneration(format!("generation {number} has no manifest")),
            ),
            Err(failed) => Err(failed.into()),
        }
    }

    fn committed_numbers_newest_first(&self, current: u64) -> Result<Vec<u64>> {
        let mut numbers = self.generation_numbers()?;
        numbers.retain(|number| *number <= current);
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        Ok(numbers)
    }

    fn highest_generation_dir(&self) -> Result<u64> {
        Ok(self.generation_numbers()?.into_iter().max().unwrap_or(0))
    }

    /// The number of every entry in `generations/` whose name is one.
    fn generation_numbers(&self) -> Result<Vec<u64>> {
        let entries = match fs::read_dir(self.root.join(GENERATIONS)) {
            Ok(entries) => entries,
            Err(none) if none.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(failed) => return Err(failed.into()),
        };
        let mut numbers = Vec::new();
        for entry in entries {
            if let Some(number) = parse_number(&entry?.file_name().to_string_lossy()) {
                numbers.push(number);
            }
        }
        Ok(numbers)
    }

    fn read_pointer(&self) -> Result<Option<Pointer>> {
        let bytes = match fs::read(self.root.join(CURRENT)) {
            Ok(bytes) => bytes,
            Err(none) if none.kind() == ErrorKind::NotFound => return Ok(None),
            Err(failed) => return Err(failed.into()),
        };
        let text = String::from_utf8(bytes)
            .map_err(|malformed| std::io::Error::new(ErrorKind::InvalidData, malformed))?;
        let text = trim_java(&text);
        let parts = split_java(text, '\t');
        match parts.as_slice() {
            [number, manifest_sha] if has_sha256_length(manifest_sha) => {
                parse_number(number).map(|generation| Pointer {
                    generation,
                    manifest_sha: (*manifest_sha).to_string(),
                })
            }
            _ => None,
        }
        .map(Some)
        .ok_or_else(|| AwfError::CorruptGeneration(format!("CURRENT is unreadable: {text}")))
    }

    /// Publishes by rename; see [`publish_failure`] for why a refusal fails the commit.
    fn publish(&self, from: &Path, to: &Path) -> Result<()> {
        self.durability
            .move_atomically(from, to)
            .map_err(|refused| publish_failure(to, refused))
    }
}

/// Whether a commit accepts a blob of this name: the manifest's character rule, and nothing that
/// would collide with the store's own files or leave the generation directory.
fn usable_blob_name(name: &str) -> bool {
    blob_name_matches(name)
        && name != MANIFEST
        && !name.ends_with(TEMP_SUFFIX)
        && name != "."
        && name != ".."
}

/// Parses a manifest's header and blob lines without touching the blobs. Header lines start with
/// `@` and come first; a key this build does not know is corrupt, never ignored.
fn parse(number: u64, dir: &Path, manifest: &[u8]) -> Result<Manifest> {
    let corrupt = |what: &str, line: &str| {
        AwfError::CorruptGeneration(format!("{what} in {}: {line}", dir.display()))
    };
    let mut format = 1;
    let mut base = 0;
    let mut base_manifest_sha = None;
    let mut header = true;
    let mut blobs = Vec::new();
    let text = String::from_utf8_lossy(manifest);
    for line in split_java(&text, '\n') {
        if line.is_empty() {
            continue;
        }
        let parts = split_java(line, '\t');
        if line.starts_with('@') {
            if !header {
                return Err(corrupt("header line after a blob line", line));
            }
            match parts.as_slice() {
                [FORMAT_KEY, ..] => {
                    format = match parts.as_slice() {
                        [_, value] => parse_number(value),
                        _ => None,
                    }
                    .filter(|format| (1..=FORMAT).contains(format))
                    .ok_or_else(|| corrupt("manifest format not readable by this build", line))?;
                }
                [BASE_KEY, ..] => {
                    let chained = match parts.as_slice() {
                        [_, value, sha] if has_sha256_length(sha) => parse_number(value)
                            .filter(|base| *base >= 1 && *base < number)
                            .map(|base| (base, (*sha).to_string())),
                        _ => None,
                    };
                    let (chained_to, sha) =
                        chained.ok_or_else(|| corrupt("malformed base line", line))?;
                    base = chained_to;
                    base_manifest_sha = Some(sha);
                }
                _ => return Err(corrupt("unknown manifest header", line)),
            }
            continue;
        }
        header = false;
        let blob = match parts.as_slice() {
            [name, length, sha] if blob_name_matches(name) && has_sha256_length(sha) => {
                parse_number(length).map(|length| BlobLine {
                    name: (*name).to_string(),
                    length,
                    sha: (*sha).to_string(),
                })
            }
            _ => None,
        };
        blobs.push(blob.ok_or_else(|| corrupt("malformed manifest line", line))?);
    }
    if format == 1 && base != 0 {
        return Err(AwfError::CorruptGeneration(format!(
            "a format 1 manifest cannot have a base: {}",
            dir.display()
        )));
    }
    Ok(Manifest {
        base,
        base_manifest_sha,
        blobs,
    })
}

/// The character rule a manifest line's blob name must meet: what the Java reader accepts.
fn blob_name_matches(name: &str) -> bool {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    !name.is_empty() && name.len() <= MAX_BLOB_NAME && name.bytes().all(allowed)
}

/// Re-reads every blob in the manifest and checks size and hash.
fn load(number: u64, dir: &Path, manifest: &[u8]) -> Result<Generation> {
    let parsed = parse(number, dir, manifest)?;
    let mut blobs = BTreeMap::new();
    for blob in parsed.blobs {
        let bytes = match fs::read(dir.join(&blob.name)) {
            Ok(bytes) => bytes,
            Err(missing) if missing.kind() == ErrorKind::NotFound => {
                return Err(AwfError::CorruptGeneration(format!(
                    "missing blob {} in {}",
                    blob.name,
                    dir.display()
                )));
            }
            Err(failed) => return Err(failed.into()),
        };
        if bytes.len() as u64 != blob.length || sha256_hex(&bytes) != blob.sha {
            return Err(AwfError::CorruptGeneration(format!(
                "blob {} in {} fails verification",
                blob.name,
                dir.display()
            )));
        }
        blobs.insert(blob.name, bytes);
    }
    Ok(Generation {
        number,
        base: parsed.base,
        manifest_sha: sha256_hex(manifest),
        base_manifest_sha: parsed.base_manifest_sha,
        blobs,
    })
}

/// Writes a file completely; durability comes from the force pass that follows.
fn write_file(file: &Path, bytes: &[u8]) -> Result<()> {
    File::create(file)?.write_all(bytes)?;
    Ok(())
}

fn delete_tree(path: &Path) -> Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(missing) if missing.kind() == ErrorKind::NotFound => return Ok(()),
        Err(failed) => Err(failed),
    };
    match removed {
        Err(missing) if missing.kind() == ErrorKind::NotFound => Ok(()),
        other => Ok(other?),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;

    use super::*;
    use crate::durability::testing::{Call, Recorder};

    fn snapshot(value: &str) -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            ("level.dat".to_string(), value.as_bytes().to_vec()),
            (
                "region-0-0".to_string(),
                format!("chunks:{value}").into_bytes(),
            ),
        ])
    }

    fn level(generation: &Generation) -> String {
        String::from_utf8(generation.blobs["level.dat"].clone()).unwrap()
    }

    fn open(root: &Path, retained: usize) -> GenerationStore {
        GenerationStore::open(root, WorldRole::Vanilla, retained).unwrap()
    }

    fn crashing_at(root: &Path, crash_at: Stage) -> GenerationStore {
        let hook: Arc<StageHook> = Arc::new(move |stage| {
            if stage == crash_at {
                return Err(io::Error::other(format!("simulated crash at {stage:?}")).into());
            }
            Ok(())
        });
        GenerationStore::open_with(root, WorldRole::Vanilla, 2, Some(hook), Arc::new(DISK)).unwrap()
    }

    fn generation_dirs(root: &Path) -> BTreeSet<String> {
        fs::read_dir(root.join(GENERATIONS))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn an_empty_store_has_no_generation() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 2);
        assert_eq!(store.current_generation().unwrap(), 0);
        assert_eq!(store.read().unwrap(), None);
    }

    #[test]
    fn a_commit_is_read_back_verified() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 2);
        assert_eq!(store.commit(&snapshot("one")).unwrap(), 1);
        assert_eq!(store.commit(&snapshot("two")).unwrap(), 2);
        let read = open(temp.path(), 2).read().unwrap().unwrap();
        assert_eq!(read.number, 2);
        assert_eq!(level(&read), "two");
        assert_eq!(read.blobs["region-0-0"], b"chunks:two");
    }

    #[test]
    fn the_files_on_disk_are_the_format_the_java_store_writes() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 2);
        store.commit(&snapshot("one")).unwrap();
        let manifest = fs::read_to_string(temp.path().join("generations/1/manifest")).unwrap();
        assert_eq!(
            manifest,
            format!(
                "level.dat\t3\t{}\nregion-0-0\t10\t{}\n",
                sha256_hex(b"one"),
                sha256_hex(b"chunks:one")
            )
        );
        assert_eq!(
            fs::read_to_string(temp.path().join(CURRENT)).unwrap(),
            format!("1\t{}\n", sha256_hex(manifest.as_bytes()))
        );
        assert_eq!(
            fs::read(temp.path().join("generations/1/level.dat")).unwrap(),
            b"one"
        );

        // A chained generation carries the format 2 header and the hash of its base's manifest.
        store
            .commit_chained(&BTreeMap::from([("delta".to_string(), b"d".to_vec())]), 1)
            .unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("generations/2/manifest")).unwrap(),
            format!(
                "@format\t2\n@base\t1\t{}\ndelta\t1\t{}\n",
                sha256_hex(manifest.as_bytes()),
                sha256_hex(b"d")
            )
        );
    }

    #[test]
    fn a_crash_at_any_stage_before_the_commit_leaves_the_previous_generation() {
        for crash_at in [
            Stage::SnapshotWritten,
            Stage::Verified,
            Stage::GenerationPublished,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let world = temp.path();
            open(world, 2).commit(&snapshot("stable")).unwrap();
            let crashing = crashing_at(world, crash_at);
            assert!(crashing.commit(&snapshot("lost")).is_err());

            // Restart: reopening recovers, and the old generation is still the truth.
            let reopened = open(world, 2);
            assert_eq!(reopened.current_generation().unwrap(), 1, "{crash_at:?}");
            assert_eq!(
                level(&reopened.read().unwrap().unwrap()),
                "stable",
                "{crash_at:?}"
            );
            assert_eq!(
                generation_dirs(world),
                BTreeSet::from(["1".to_string()]),
                "leftovers removed after {crash_at:?}"
            );

            // And the next commit proceeds normally.
            assert_eq!(reopened.commit(&snapshot("next")).unwrap(), 2);
            assert_eq!(level(&reopened.read().unwrap().unwrap()), "next");
        }
    }

    #[test]
    fn a_crash_after_the_commit_point_keeps_the_new_generation() {
        let temp = tempfile::tempdir().unwrap();
        open(temp.path(), 2).commit(&snapshot("old")).unwrap();
        let crashing = crashing_at(temp.path(), Stage::Committed);
        assert!(crashing.commit(&snapshot("new")).is_err());
        assert_eq!(level(&open(temp.path(), 2).read().unwrap().unwrap()), "new");
    }

    #[test]
    fn a_reader_does_not_remove_what_a_live_writer_has_not_committed_yet() {
        let temp = tempfile::tempdir().unwrap();
        open(temp.path(), 2).commit(&snapshot("stable")).unwrap();
        assert!(
            crashing_at(temp.path(), Stage::GenerationPublished)
                .commit(&snapshot("in flight"))
                .is_err()
        );
        let reader = GenerationStore::open(temp.path(), WorldRole::ReadOnly, 2).unwrap();
        assert_eq!(
            generation_dirs(temp.path()),
            BTreeSet::from(["1".to_string(), "2".to_string()])
        );
        assert_eq!(level(&reader.read().unwrap().unwrap()), "stable");
    }

    #[test]
    fn a_damaged_blob_is_reported_not_returned() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 2);
        store.commit(&snapshot("intact")).unwrap();
        fs::write(temp.path().join("generations/1/level.dat"), "tampere").unwrap();
        assert!(matches!(store.read(), Err(AwfError::CorruptGeneration(_))));
    }

    #[test]
    fn a_manifest_that_does_not_match_current_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 2);
        store.commit(&snapshot("intact")).unwrap();
        let manifest = temp.path().join("generations/1/manifest");
        let text = fs::read_to_string(&manifest).unwrap();
        fs::write(&manifest, format!("{text}\n")).unwrap();
        assert!(
            matches!(store.read(), Err(AwfError::CorruptGeneration(message)) if message.contains("does not match CURRENT"))
        );
    }

    #[test]
    fn only_the_retained_number_of_generations_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 2);
        for i in 0..5 {
            store.commit(&snapshot(&format!("v{i}"))).unwrap();
        }
        assert_eq!(
            generation_dirs(temp.path()),
            BTreeSet::from(["4".to_string(), "5".to_string()])
        );
        let retained: Vec<u64> = store
            .read_retained()
            .unwrap()
            .iter()
            .map(|generation| generation.number)
            .collect();
        assert_eq!(retained, [5, 4]);
    }

    #[test]
    fn a_base_is_kept_as_long_as_a_retained_generation_is_chained_to_it() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 1);
        let delta =
            |value: &str| BTreeMap::from([("delta".to_string(), value.as_bytes().to_vec())]);
        store.commit(&snapshot("full")).unwrap();
        store.commit_chained(&delta("a"), 1).unwrap();
        store.commit_chained(&delta("b"), 2).unwrap();
        assert_eq!(
            generation_dirs(temp.path()),
            BTreeSet::from(["1".to_string(), "2".to_string(), "3".to_string()]),
            "the whole chain under the one retained generation stays"
        );
        let retained = store.read_retained().unwrap();
        assert_eq!(
            retained.len(),
            1,
            "chain-only ancestors are not retained generations"
        );
        let top = &retained[0];
        let middle = store.read_base(top).unwrap().unwrap();
        let bottom = store.read_base(&middle).unwrap().unwrap();
        assert_eq!((top.number, middle.number, bottom.number), (3, 2, 1));
        assert_eq!(store.read_base(&bottom).unwrap(), None);

        // A full generation ends the chain, and retention drops the old one.
        store.commit(&snapshot("compacted")).unwrap();
        assert_eq!(
            generation_dirs(temp.path()),
            BTreeSet::from(["4".to_string()])
        );
    }

    #[test]
    fn a_base_that_is_not_the_manifest_chained_to_is_corrupt() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 2);
        store.commit(&snapshot("full")).unwrap();
        store
            .commit_chained(&BTreeMap::from([("delta".to_string(), b"d".to_vec())]), 1)
            .unwrap();
        let child = store.read().unwrap().unwrap();
        // The base is replaced by a different, internally consistent generation.
        fs::write(temp.path().join("generations/1/level.dat"), "other").unwrap();
        fs::write(
            temp.path().join("generations/1/manifest"),
            format!(
                "level.dat\t5\t{}\nregion-0-0\t11\t{}\n",
                sha256_hex(b"other"),
                sha256_hex(b"chunks:full")
            ),
        )
        .unwrap();
        assert!(
            matches!(store.read_base(&child), Err(AwfError::CorruptGeneration(message)) if message.contains("not the one on disk"))
        );
    }

    #[test]
    fn a_base_must_be_committed() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 2);
        store.commit(&snapshot("full")).unwrap();
        assert!(matches!(
            store.commit_chained(&snapshot("x"), 2),
            Err(AwfError::InvalidArgument(_))
        ));
        assert_eq!(store.current_generation().unwrap(), 1);
    }

    #[test]
    fn roles_that_do_not_accept_commits_refuse() {
        let temp = tempfile::tempdir().unwrap();
        for role in WorldRole::ALL {
            let store = GenerationStore::open(&temp.path().join(role.name()), role, 1).unwrap();
            if role.accepts_commits() {
                assert_eq!(store.commit(&snapshot("ok")).unwrap(), 1);
            } else {
                assert!(
                    matches!(
                        store.commit(&snapshot("no")),
                        Err(AwfError::InvalidState(_))
                    ),
                    "{role}"
                );
            }
        }
    }

    #[test]
    fn unsafe_blob_names_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 1);
        for name in ["../escape", "manifest", "a/b", "", "x.tmp", ".", ".."] {
            let snapshot = BTreeMap::from([(name.to_string(), Vec::new())]);
            assert!(
                matches!(store.commit(&snapshot), Err(AwfError::InvalidArgument(_))),
                "{name}"
            );
        }
        assert_eq!(store.current_generation().unwrap(), 0);
    }

    #[test]
    fn a_manifest_this_build_does_not_understand_is_corrupt_not_ignored() {
        let dir = Path::new("generations/7");
        let sha = sha256_hex(b"");
        let refused = |manifest: String| {
            matches!(
                parse(7, dir, manifest.as_bytes()),
                Err(AwfError::CorruptGeneration(_))
            )
        };
        assert!(refused(format!("@format\t3\nindex\t0\t{sha}\n")));
        assert!(refused(format!("@future\tx\nindex\t0\t{sha}\n")));
        assert!(
            refused(format!("index\t0\t{sha}\n@format\t2\n")),
            "header after a blob line"
        );
        assert!(
            refused(format!("@format\t2\n@base\t7\t{sha}\nindex\t0\t{sha}\n")),
            "a base is older"
        );
        assert!(
            refused(format!("@base\t1\t{sha}\nindex\t0\t{sha}\n")),
            "format 1 has no base"
        );
        assert!(refused("index\t0\tshort\n".to_string()));
        assert!(refused(format!("in/dex\t0\t{sha}\n")));
        assert!(refused(format!("index\t0\t{sha}\textra\n")));

        let parsed = parse(
            7,
            dir,
            format!("@format\t2\n@base\t6\t{sha}\n\nindex\t0\t{sha}\t\n").as_bytes(),
        )
        .unwrap();
        assert_eq!(parsed.base, 6);
        assert_eq!(
            parsed.blobs.len(),
            1,
            "an empty line and a trailing tab are tolerated, as in Java"
        );
    }

    #[test]
    fn a_damaged_pointer_is_corrupt() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), 1);
        store.commit(&snapshot("one")).unwrap();
        for damaged in ["", "1", "x\tabc", "1\tshort", "1\t2\t3"] {
            fs::write(temp.path().join(CURRENT), damaged).unwrap();
            assert!(
                matches!(
                    store.current_generation(),
                    Err(AwfError::CorruptGeneration(_))
                ),
                "{damaged:?}"
            );
        }
    }

    #[test]
    fn a_commit_forces_files_before_their_directory_and_the_pointer_last() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("store");
        fs::create_dir(&root).unwrap();
        let recorder = Arc::new(Recorder::new(&root));
        let store =
            GenerationStore::open_with(&root, WorldRole::Vanilla, 2, None, recorder.clone())
                .unwrap();
        store.commit(&snapshot("one")).unwrap();
        let path = PathBuf::from;
        assert_eq!(
            recorder.calls(),
            [
                // The fresh `generations/` directory's entry in the root.
                Call::Directory(path("")),
                Call::File(path("generations/1.tmp/level.dat")),
                Call::File(path("generations/1.tmp/region-0-0")),
                Call::File(path("generations/1.tmp/manifest")),
                Call::Directory(path("generations/1.tmp")),
                Call::Move(path("generations/1.tmp"), path("generations/1")),
                Call::Directory(path("generations")),
                Call::File(path("CURRENT.tmp")),
                Call::Move(path("CURRENT.tmp"), path("CURRENT")),
                Call::Directory(path("")),
            ]
        );
    }

    #[test]
    fn a_failed_force_fails_the_commit_and_leaves_the_previous_generation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("store");
        fs::create_dir(&root).unwrap();
        let recorder = Arc::new(Recorder::new(&root));
        let store =
            GenerationStore::open_with(&root, WorldRole::Vanilla, 2, None, recorder.clone())
                .unwrap();
        store.commit(&snapshot("stable")).unwrap();
        recorder.fail_when(|call| {
            matches!(call, Call::Directory(dir) if dir == Path::new("generations"))
                .then(|| io::Error::other("injected"))
        });
        assert!(store.commit(&snapshot("lost")).is_err());
        recorder.stop_failing();
        assert_eq!(level(&store.read().unwrap().unwrap()), "stable");
        assert_eq!(
            store.commit(&snapshot("next")).unwrap(),
            3,
            "the unpublished number is not reused"
        );
    }
}
