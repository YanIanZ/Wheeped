//! Content-addressed chunk storage.
//!
//! Each object is a chunk's deflated bytes, named by the SHA-256 of its uncompressed bytes. That is
//! what makes an incremental commit both cheap and safe: a chunk that has not changed already has
//! an object with its name, so a commit writes only new objects and references the rest, while
//! every generation still describes the complete world.
//!
//! Writes are batched for durability. [`ObjectStore::put`] writes an object to a temporary file in
//! its shard and keeps it pending. [`ObjectStore::sync`] then forces every pending file in one
//! pass, moves each into place under its name, and forces each touched directory once. So an
//! object either exists whole under its name, forced before it was renamed, or is a temporary file
//! that the next writable open removes. A caller forces nothing that references an object before
//! `sync` has returned.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use indexmap::{IndexMap, IndexSet};

use crate::codec;
use crate::durability::Durability;
use crate::error::{AwfError, Result};
use crate::hash;
use crate::image::MAX_CHUNK_BYTES;
use crate::lock;
use crate::role::WorldRole;

const RAW_LENGTH_BYTES: usize = 4;
const TEMP_SUFFIX: &str = ".tmp";

/// An object's name: the SHA-256 of a chunk's uncompressed bytes.
///
/// On disk and in an index it is 64 lower-case hex characters. Nothing else is ever turned into a
/// path, so a damaged or hostile index cannot make a store read or delete a file outside its
/// object directory.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectName([u8; 32]);

impl ObjectName {
    /// The name of the object holding `raw`.
    #[must_use]
    pub fn of(raw: &[u8]) -> Self {
        Self(hash::sha256(raw))
    }

    /// Parses 64 lower-case hex characters; anything else is not an object name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let lower_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if text.len() != 64 || !text.bytes().all(lower_hex) {
            return None;
        }
        let mut name = [0u8; 32];
        hex::decode_to_slice(text, &mut name).ok()?;
        Some(Self(name))
    }

    /// The name as 64 lower-case hex characters.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for ObjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectName({self})")
    }
}

/// What [`ObjectStore::put`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// The object's name.
    pub name: ObjectName,
    /// Bytes written; 0 when the object already existed or is pending.
    pub bytes: u64,
}

#[derive(Default)]
struct Pending {
    /// Objects written since the last sync: name to its temporary file, in the order written.
    objects: IndexMap<ObjectName, PathBuf>,
    /// Shards with an object renamed into them whose force has not succeeded yet. Kept across a
    /// failed sync: an object already in place must still get its directory forced by the retry,
    /// or an index could become durable while the entry naming the object is not.
    dirty_shards: IndexSet<PathBuf>,
    /// Whether a shard was created since the root was last forced successfully.
    root_dirty: bool,
    /// Distinguishes this store's temporary files from each other.
    next_temp: u64,
}

/// The object directory of one world store.
pub struct ObjectStore {
    root: PathBuf,
    durability: Arc<dyn Durability>,
    /// Whether this store may create, write or delete anything.
    writable: bool,
    pending: Mutex<Pending>,
}

impl ObjectStore {
    /// Opens the object directory for a world of `role`.
    ///
    /// A role that accepts commits creates the directory and removes the temporary files a crash
    /// left. Any other role changes nothing on disk (it would otherwise delete the pending objects
    /// of a live writer of the same store) and refuses [`Self::put`], [`Self::delete`] and
    /// [`Self::retain_only`].
    pub fn open(root: &Path, durability: Arc<dyn Durability>, role: WorldRole) -> Result<Self> {
        let store = Self {
            root: root.to_path_buf(),
            durability,
            writable: role.accepts_commits(),
            pending: Mutex::new(Pending::default()),
        };
        if !store.writable {
            return Ok(store);
        }
        // A fresh store: the entries naming the root and any ancestor it needed are made durable
        // now, before any commit can depend on them.
        store.durability.create_directories_durably(root)?;
        for shard in fs::read_dir(root)? {
            let shard = shard?.path();
            if !shard.is_dir() {
                continue;
            }
            for file in fs::read_dir(&shard)? {
                let file = file?;
                if file.file_name().to_string_lossy().ends_with(TEMP_SUFFIX) {
                    remove_file_if_exists(&file.path())?;
                }
            }
        }
        Ok(store)
    }

    fn path(&self, name: ObjectName) -> PathBuf {
        let hex = name.to_hex();
        self.root.join(&hex[..2]).join(hex)
    }

    fn require_writable(&self) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(AwfError::InvalidState(
                "this AWF object store was opened read-only".to_string(),
            ))
        }
    }

    /// Whether an object is in place under its name; a pending one is not yet.
    #[must_use]
    pub fn contains(&self, name: ObjectName) -> bool {
        self.path(name).is_file()
    }

    /// Writes a chunk as a pending object; [`Self::sync`] makes it durable and names it. It is
    /// readable through [`Self::get`] meanwhile.
    ///
    /// With `rewrite` the object is written even if one of this name exists (full mode).
    pub fn put(&self, raw: &[u8], rewrite: bool) -> Result<Written> {
        self.require_writable()?;
        let raw_length = i32::try_from(raw.len()).map_err(|_| {
            AwfError::InvalidArgument(format!("a chunk of {} bytes cannot be stored", raw.len()))
        })?;
        let name = ObjectName::of(raw);
        let unchanged = Written { name, bytes: 0 };
        let mut pending = lock(&self.pending);
        if let Some(temp) = pending.objects.get(&name) {
            if temp.is_file() {
                return Ok(unchanged);
            }
            // Its temporary file was removed under us (a temp cleaner, antivirus): written again.
            pending.objects.shift_remove(&name);
        }
        let target = self.path(name);
        if !rewrite && target.is_file() {
            return Ok(unchanged);
        }
        let shard = self.root.join(&name.to_hex()[..2]);
        if !shard.is_dir() {
            fs::create_dir_all(&shard)?;
            pending.root_dirty = true;
        }
        let deflated = codec::deflate(raw)?;
        let temp = write_temp(&shard, name, &mut pending.next_temp, raw_length, &deflated)?;
        pending.objects.insert(name, temp);
        Ok(Written {
            name,
            bytes: (RAW_LENGTH_BYTES + deflated.len()) as u64,
        })
    }

    /// Makes every pending object durable and puts it in place: one pass of file forces, then the
    /// moves, then one force per dirty directory (shards, then the root when a shard was created).
    /// Returns how many objects were synced.
    ///
    /// On failure everything not yet done stays pending or dirty, so a later sync finishes it,
    /// including the directory of an object the failed attempt had already renamed. A pending
    /// object whose temporary file has disappeared is dropped from the batch and the sync fails, so
    /// the commit fails once and its retry writes the object again.
    pub fn sync(&self) -> Result<usize> {
        let mut pending = lock(&self.pending);
        if pending.objects.is_empty() && pending.dirty_shards.is_empty() && !pending.root_dirty {
            return Ok(0);
        }
        self.force_pending(&mut pending)?;
        let synced = self.publish_pending(&mut pending)?;
        while let Some(shard) = pending.dirty_shards.first().cloned() {
            self.durability.force_directory(&shard)?;
            pending.dirty_shards.shift_remove(&shard);
        }
        if pending.root_dirty {
            self.durability.force_directory(&self.root)?;
            pending.root_dirty = false;
        }
        Ok(synced)
    }

    fn force_pending(&self, pending: &mut Pending) -> Result<()> {
        let mut vanished: Option<(PathBuf, io::Error)> = None;
        let mut failed: Option<io::Error> = None;
        pending.objects.retain(|_, temp| {
            if failed.is_some() {
                return true;
            }
            match self.durability.force_file(temp) {
                Ok(()) => true,
                Err(missing) if missing.kind() == ErrorKind::NotFound => {
                    if vanished.is_none() {
                        vanished = Some((temp.clone(), missing));
                    }
                    false
                }
                Err(other) => {
                    failed = Some(other);
                    true
                }
            }
        });
        if let Some(failed) = failed {
            return Err(failed.into());
        }
        match vanished {
            Some((temp, missing)) => Err(vanished_temp(&temp, missing)),
            None => Ok(()),
        }
    }

    fn publish_pending(&self, pending: &mut Pending) -> Result<usize> {
        let mut synced = 0;
        let mut failure = None;
        // Entries are removed in one pass at the end; `done` is how many leave the batch.
        let mut done = 0;
        for (name, temp) in &pending.objects {
            let target = self.path(*name);
            if let Some(shard) = target.parent() {
                pending.dirty_shards.insert(shard.to_path_buf());
            }
            match self.durability.move_atomically(temp, &target) {
                Ok(()) => {
                    synced += 1;
                    done += 1;
                }
                Err(missing) if missing.kind() == ErrorKind::NotFound => {
                    // Gone between its force and its rename: dropped, like a vanished force.
                    done += 1;
                    failure = Some(vanished_temp(temp, missing));
                    break;
                }
                Err(refused) => {
                    // The object stays pending and the commit fails.
                    failure = Some(publish_failure(&target, refused));
                    break;
                }
            }
        }
        pending.objects.drain(..done);
        failure.map_or(Ok(synced), Err)
    }

    /// Directories whose entries still need a force; non-zero only after a failed sync.
    #[must_use]
    pub fn dirty_directories(&self) -> usize {
        let pending = lock(&self.pending);
        pending.dirty_shards.len() + usize::from(pending.root_dirty)
    }

    /// Objects written and not yet synced.
    #[must_use]
    pub fn pending(&self) -> usize {
        lock(&self.pending).objects.len()
    }

    /// Reads and verifies an object, pending or in place.
    pub fn get(&self, name: ObjectName) -> Result<Vec<u8>> {
        let temp = lock(&self.pending).objects.get(&name).cloned();
        let pending_bytes = match temp.map(fs::read) {
            Some(Ok(bytes)) => Some(bytes),
            // A sync put it in place between the lookup and the read.
            Some(Err(moved)) if moved.kind() == ErrorKind::NotFound => None,
            Some(Err(failed)) => return Err(failed.into()),
            None => None,
        };
        let stored = match pending_bytes {
            Some(bytes) => bytes,
            None => match fs::read(self.path(name)) {
                Ok(bytes) => bytes,
                Err(missing) if missing.kind() == ErrorKind::NotFound => {
                    return Err(AwfError::Corrupt(format!("object {name} is missing")));
                }
                Err(failed) => return Err(failed.into()),
            },
        };
        let Some((length, body)) = stored.split_first_chunk::<RAW_LENGTH_BYTES>() else {
            return Err(AwfError::Corrupt(format!("object {name} is truncated")));
        };
        let declared = i32::from_be_bytes(*length);
        let raw_length = usize::try_from(declared)
            .ok()
            .filter(|length| *length <= MAX_CHUNK_BYTES)
            .ok_or_else(|| AwfError::Corrupt(format!("object {name} declares size {declared}")))?;
        let raw = codec::inflate(body, raw_length, &format_args!("object {name}"))?;
        if ObjectName::of(&raw) != name {
            return Err(AwfError::Corrupt(format!(
                "object {name} fails its SHA-256"
            )));
        }
        Ok(raw)
    }

    /// Deletes one object, pending or in place, if it exists; returns whether it did.
    pub fn delete(&self, name: ObjectName) -> Result<bool> {
        self.require_writable()?;
        let mut pending = lock(&self.pending);
        if let Some(temp) = pending.objects.shift_remove(&name) {
            remove_file_if_exists(&temp)?;
            return Ok(true);
        }
        Ok(remove_file_if_exists(&self.path(name))?)
    }

    /// Deletes every object in place that is not in `live`; returns how many were removed.
    /// Pending objects are left alone: they are this store's own unfinished batch.
    pub fn retain_only(&self, live: &HashSet<ObjectName>) -> Result<usize> {
        self.require_writable()?;
        let _pending = lock(&self.pending);
        let mut removed = 0;
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?.path();
            if !shard.is_dir() {
                continue;
            }
            for file in fs::read_dir(&shard)? {
                let file = file?;
                let file_name = file.file_name();
                let file_name = file_name.to_string_lossy();
                let is_live =
                    ObjectName::parse(&file_name).is_some_and(|name| live.contains(&name));
                if file_name.ends_with(TEMP_SUFFIX) || is_live {
                    continue;
                }
                if remove_file_if_exists(&file.path())? {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

/// Writes one object to a new temporary file in its shard and returns the file's path.
fn write_temp(
    shard: &Path,
    name: ObjectName,
    next_temp: &mut u64,
    raw_length: i32,
    deflated: &[u8],
) -> Result<PathBuf> {
    let (temp, mut file) = loop {
        let candidate = shard.join(format!(
            "{name}.{}.{next_temp}{TEMP_SUFFIX}",
            std::process::id()
        ));
        *next_temp += 1;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(taken) if taken.kind() == ErrorKind::AlreadyExists => {}
            Err(failed) => return Err(failed.into()),
        }
    };
    let written = file
        .write_all(&raw_length.to_be_bytes())
        .and_then(|()| file.write_all(deflated));
    if let Err(failed) = written {
        drop(file);
        remove_file_if_exists(&temp)?;
        return Err(failed.into());
    }
    Ok(temp)
}

fn vanished_temp(temp: &Path, missing: io::Error) -> AwfError {
    AwfError::IoContext {
        context: format!(
            "a pending AWF object's temporary file {} disappeared before it was made durable (removed outside the server?); it was dropped from the batch and the next commit attempt writes it again",
            temp.display()
        ),
        source: missing,
    }
}

/// The failure of a rename that publishes `target`.
///
/// A temporary file sits in its target's own directory, so a filesystem that reports a
/// cross-device rename here cannot rename atomically at all. An object or a generation must appear
/// whole under its name or not at all, and a copy-and-delete would not promise that.
pub fn publish_failure(target: &Path, refused: io::Error) -> AwfError {
    if refused.kind() == ErrorKind::CrossesDevices {
        AwfError::IoContext {
            context: format!(
                "AWF needs an atomic rename to publish {} and this filesystem refused one; the commit is refused rather than published by copying",
                target.display()
            ),
            source: refused,
        }
    } else {
        refused.into()
    }
}

/// Removes a file; returns whether it existed.
pub fn remove_file_if_exists(file: &Path) -> io::Result<bool> {
    match fs::remove_file(file) {
        Ok(()) => Ok(true),
        Err(missing) if missing.kind() == ErrorKind::NotFound => Ok(false),
        Err(failed) => Err(failed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durability::DISK;
    use crate::durability::testing::{Call, Recorder};

    fn open(root: &Path, role: WorldRole) -> ObjectStore {
        ObjectStore::open(root, Arc::new(DISK), role).unwrap()
    }

    #[test]
    fn an_object_name_is_lower_case_sha256_hex_and_nothing_else() {
        let name = ObjectName::of(b"abc");
        assert_eq!(
            name.to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(ObjectName::parse(&name.to_hex()), Some(name));
        assert_eq!(ObjectName::parse(&name.to_hex().to_uppercase()), None);
        assert_eq!(ObjectName::parse(&name.to_hex()[1..]), None);
        assert_eq!(
            ObjectName::parse("../../../../../../../../../../../../../../../../../../../../etc"),
            None
        );
        assert_eq!(ObjectName::parse("-"), None);
    }

    #[test]
    fn a_pending_object_is_readable_and_named_only_by_a_sync() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), WorldRole::Vanilla);
        let written = store.put(b"chunk bytes", false).unwrap();
        assert!(written.bytes > 0);
        assert_eq!(store.pending(), 1);
        assert!(
            !store.contains(written.name),
            "not in place before the sync"
        );
        assert_eq!(store.get(written.name).unwrap(), b"chunk bytes");

        assert_eq!(
            store.put(b"chunk bytes", false).unwrap().bytes,
            0,
            "already pending"
        );
        assert_eq!(store.sync().unwrap(), 1);
        assert!(store.contains(written.name));
        assert_eq!(store.pending(), 0);
        assert_eq!(store.get(written.name).unwrap(), b"chunk bytes");
        assert_eq!(
            store.put(b"chunk bytes", false).unwrap().bytes,
            0,
            "already in place"
        );
        assert!(
            store.put(b"chunk bytes", true).unwrap().bytes > 0,
            "a rewrite writes again"
        );
    }

    #[test]
    fn the_stored_form_is_the_raw_length_then_a_zlib_stream() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), WorldRole::Vanilla);
        let raw = b"the same bytes the Java store writes".repeat(20);
        let name = store.put(&raw, false).unwrap().name;
        store.sync().unwrap();
        let hex = name.to_hex();
        let stored = fs::read(temp.path().join(&hex[..2]).join(&hex)).unwrap();
        assert_eq!(stored[..4], (raw.len() as u32).to_be_bytes());
        assert_eq!(stored[4], 0x78, "zlib header");
        assert_eq!(
            codec::inflate(&stored[4..], raw.len(), &"object").unwrap(),
            raw
        );
    }

    #[test]
    fn a_sync_forces_files_then_moves_then_forces_each_directory_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("objects");
        let recorder = Arc::new(Recorder::new(&root));
        let store = ObjectStore::open(&root, recorder.clone(), WorldRole::Vanilla).unwrap();
        recorder.clear();
        let a = store.put(b"a", false).unwrap().name.to_hex();
        let b = store.put(b"b", false).unwrap().name.to_hex();
        assert_eq!(store.sync().unwrap(), 2);

        let calls = recorder.calls();
        let shard = |hex: &str| PathBuf::from(&hex[..2]);
        assert!(matches!(&calls[0], Call::File(path) if path.starts_with(shard(&a))));
        assert!(matches!(&calls[1], Call::File(path) if path.starts_with(shard(&b))));
        assert!(matches!(&calls[2], Call::Move(_, to) if *to == shard(&a).join(&a)));
        assert!(matches!(&calls[3], Call::Move(_, to) if *to == shard(&b).join(&b)));
        assert_eq!(
            calls[4..],
            [
                Call::Directory(shard(&a)),
                Call::Directory(shard(&b)),
                Call::Directory(PathBuf::new()),
            ]
        );
        assert_eq!(store.dirty_directories(), 0);
    }

    #[test]
    fn a_failed_directory_force_is_finished_by_the_next_sync() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("objects");
        let recorder = Arc::new(Recorder::new(&root));
        let store = ObjectStore::open(&root, recorder.clone(), WorldRole::Vanilla).unwrap();
        let name = store.put(b"a", false).unwrap().name;
        recorder.fail_when(|call| {
            matches!(call, Call::Directory(_)).then(|| io::Error::other("injected"))
        });
        assert!(store.sync().is_err());
        assert!(store.contains(name), "the rename had already happened");
        assert_eq!(store.pending(), 0);
        assert_eq!(
            store.dirty_directories(),
            2,
            "its shard and the root still need a force"
        );

        recorder.stop_failing();
        recorder.clear();
        assert_eq!(store.sync().unwrap(), 0);
        assert_eq!(recorder.calls().len(), 2);
        assert_eq!(store.dirty_directories(), 0);
    }

    #[test]
    fn a_vanished_temporary_file_fails_the_sync_once_and_is_written_again() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), WorldRole::Vanilla);
        let name = store.put(b"a", false).unwrap().name;
        let shard = temp.path().join(&name.to_hex()[..2]);
        for file in fs::read_dir(&shard).unwrap() {
            fs::remove_file(file.unwrap().path()).unwrap();
        }
        assert!(store.sync().is_err());
        assert_eq!(store.pending(), 0, "dropped from the batch");
        assert!(
            store.put(b"a", false).unwrap().bytes > 0,
            "the retry writes it again"
        );
        assert_eq!(store.sync().unwrap(), 1);
        assert_eq!(store.get(name).unwrap(), b"a");
    }

    #[test]
    fn a_damaged_object_is_reported_not_returned() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), WorldRole::Vanilla);
        let name = store.put(b"rot", false).unwrap().name;
        store.sync().unwrap();
        let hex = name.to_hex();
        let file = temp.path().join(&hex[..2]).join(&hex);

        fs::write(&file, [0, 0, 0, 3, 1, 2, 3]).unwrap();
        assert!(matches!(store.get(name), Err(AwfError::Corrupt(_))));
        fs::write(&file, [0, 0]).unwrap();
        assert!(
            matches!(store.get(name), Err(AwfError::Corrupt(message)) if message.ends_with("is truncated"))
        );
        fs::write(&file, [0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        assert!(
            matches!(store.get(name), Err(AwfError::Corrupt(message)) if message.ends_with("declares size -1"))
        );
        // Valid data of another chunk under this name.
        let mut other = 5i32.to_be_bytes().to_vec();
        other.extend(codec::deflate(b"other").unwrap());
        fs::write(&file, other).unwrap();
        assert!(
            matches!(store.get(name), Err(AwfError::Corrupt(message)) if message.ends_with("fails its SHA-256"))
        );
        fs::remove_file(&file).unwrap();
        assert!(
            matches!(store.get(name), Err(AwfError::Corrupt(message)) if message.ends_with("is missing"))
        );
    }

    #[test]
    fn retain_only_keeps_live_objects_and_this_stores_pending_batch() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path(), WorldRole::Vanilla);
        let live = store.put(b"live", false).unwrap().name;
        let dead = store.put(b"dead", false).unwrap().name;
        store.sync().unwrap();
        let pending = store.put(b"pending", false).unwrap().name;

        assert_eq!(store.retain_only(&HashSet::from([live])).unwrap(), 1);
        assert!(store.contains(live));
        assert!(!store.contains(dead));
        assert_eq!(store.get(pending).unwrap(), b"pending");
    }

    #[test]
    fn a_writable_open_removes_temporary_files_and_a_read_only_open_changes_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let writer = open(temp.path(), WorldRole::Vanilla);
        let name = writer.put(b"unfinished", false).unwrap().name;
        let shard = temp.path().join(&name.to_hex()[..2]);
        let files = |dir: &Path| fs::read_dir(dir).unwrap().count();
        assert_eq!(files(&shard), 1);

        let reader = open(temp.path(), WorldRole::ReadOnly);
        assert_eq!(
            files(&shard),
            1,
            "a reader leaves a live writer's batch alone"
        );
        assert!(matches!(
            reader.put(b"x", false),
            Err(AwfError::InvalidState(_))
        ));
        assert!(matches!(
            reader.delete(name),
            Err(AwfError::InvalidState(_))
        ));
        assert!(matches!(
            reader.retain_only(&HashSet::new()),
            Err(AwfError::InvalidState(_))
        ));

        open(temp.path(), WorldRole::Vanilla);
        assert_eq!(files(&shard), 0, "a crash's leftovers are removed");

        let missing = temp.path().join("never-created");
        open(&missing, WorldRole::Template);
        assert!(!missing.exists());
    }
}
