//! A world's committed chunks on the FILE backend: a chunk index per generation in a
//! [`GenerationStore`], chunk bytes in a content-addressed [`ObjectStore`].
//!
//! Index format. A generation holds either a full index (blob `index`, one `"x z <object>"` line
//! per chunk, `-` for a deleted chunk) and is chained to nothing, or a delta (blob `delta`, chained
//! to the previous generation): one line per chunk whose entry changed, `"x z <object>"` for a new
//! object, `"x z -"` for a deletion that shadows the base, `"x z drop"` for a chunk that leaves the
//! index. The committed state of a generation is its chain replayed oldest to newest from the
//! nearest full index.
//!
//! Deltas are opt-in ([`Compaction::DELTAS`]). With them off every commit writes a full index in a
//! format 1 manifest, which every build of the Java store reads, and a writable open compacts a
//! chained store at once.
//!
//! Commit sequence, for every mode: write objects (each to a temporary file in its shard), read
//! back and verify the objects this commit wrote (checkpoint: every object the new index
//! references), sync the objects, then commit the index or delta as a new generation, which is the
//! single commit point, then drop objects no retained generation references. The steps after the
//! commit point never fail the commit: what they could not delete is logged and retried by the
//! next commit. No index is forced before every object it references has been.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::durability::{DISK, Durability};
use crate::error::{AwfError, Result};
use crate::generation::{AfterCommitWarning, Generation, GenerationStore, StageHook};
use crate::key::ChunkKey;
use crate::lock;
use crate::object_store::{ObjectName, ObjectStore};
use crate::role::{PersistenceMode, WorldRole};
use crate::source::ChunkSource;
use crate::store::{CommitResult, Store};
use crate::text::split_java;

const INDEX_BLOB: &str = "index";
const DELTA_BLOB: &str = "delta";
/// Index value for a chunk this world deleted. It shadows the base (a template) the way a write
/// does, so a deleted chunk does not reappear from underneath.
const TOMBSTONE: &str = "-";
/// Delta value for a chunk that leaves the index entirely; never in a full index.
const DROP: &str = "drop";

/// What the committed state says about one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// The chunk's bytes are in this object.
    Object(ObjectName),
    /// The chunk was deleted and shadows the base.
    Deleted,
}

/// One line of a delta: a new entry, or the chunk leaving the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Set(Entry),
    Drop,
}

type Index = BTreeMap<ChunkKey, Entry>;

/// Whether commits may write deltas at all, and when one writes a full index instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compaction {
    /// Whether a commit may write a delta against the previous generation.
    pub delta_generations: bool,
    /// The number of delta hops after which a commit writes a full index.
    pub max_chain_length: usize,
}

impl Compaction {
    /// Every commit writes a full index. The default: every build of the Java store reads it.
    pub const FULL_ONLY: Self = Self {
        delta_generations: false,
        max_chain_length: 32,
    };
    /// Commits write deltas, up to 32 hops.
    pub const DELTAS: Self = Self {
        delta_generations: true,
        max_chain_length: 32,
    };
}

/// A generation's replayed state and how far its chain reaches.
#[derive(Clone)]
struct Resolved {
    number: u64,
    index: Arc<Index>,
    hops: usize,
    entries: u64,
}

struct State {
    /// The committed generation the index is the state of; 0 before the first commit.
    generation: u64,
    /// Delta hops from the committed generation down to its full index, and the entries they hold.
    chain_length: usize,
    chain_entries: u64,
    /// Whether the last commit wrote a full index.
    last_commit_compacted: bool,
    /// Object names each retained generation's replayed state references, newest first. Kept in
    /// memory so a commit neither re-reads the retained indexes nor scans the object directory.
    retained_names: VecDeque<HashSet<ObjectName>>,
    /// Objects written by commits that failed before their generation committed, and objects a
    /// collection after a commit could not delete; the next commit's collection checks them.
    orphan_candidates: HashSet<ObjectName>,
    collect_warning: AfterCommitWarning,
    closed: bool,
}

/// The chunks one commit changes.
struct Changes<'a> {
    changed: &'a BTreeMap<ChunkKey, Vec<u8>>,
    removed: &'a BTreeSet<ChunkKey>,
    deleted: &'a BTreeSet<ChunkKey>,
    mode: PersistenceMode,
    /// Write a full index whatever the policy says.
    force_full: bool,
}

/// What the part of a commit before the commit point produced.
struct Committed {
    generation: u64,
    full: bool,
    delta_entries: u64,
    objects_written: usize,
    bytes_written: u64,
}

/// A world's committed chunks on the FILE backend.
pub struct WorldStore {
    /// The store's directory, for log lines.
    root: PathBuf,
    generations: GenerationStore,
    objects: ObjectStore,
    role: WorldRole,
    retained_generations: usize,
    compaction: Compaction,
    /// The committed index. Readers take it without waiting for a commit in progress.
    index: RwLock<Arc<Index>>,
    state: Mutex<State>,
}

impl WorldStore {
    /// Opens (or creates) a world store that writes full indexes only.
    ///
    /// `retained_generations` is the number of committed generations kept for recovery, at least 1.
    pub fn open(root: &Path, role: WorldRole, retained_generations: usize) -> Result<Self> {
        Self::open_with(
            root,
            role,
            retained_generations,
            None,
            Arc::new(DISK),
            Compaction::FULL_ONLY,
        )
    }

    /// As [`Self::open`], with the compaction policy, a crash hook and the durability seam both
    /// stores force through.
    pub fn open_with(
        root: &Path,
        role: WorldRole,
        retained_generations: usize,
        hook: Option<Arc<StageHook>>,
        durability: Arc<dyn Durability>,
        compaction: Compaction,
    ) -> Result<Self> {
        if compaction.max_chain_length < 1 {
            return Err(AwfError::InvalidArgument(
                "maxChainLength must be at least 1".to_string(),
            ));
        }
        let generations = GenerationStore::open_with(
            &root.join("generations-root"),
            role,
            retained_generations,
            hook,
            durability.clone(),
        )?;
        let objects = ObjectStore::open(&root.join("objects"), durability, role)?;
        let mut resolved = HashMap::new();
        let current = generations.read()?;
        let has_current = current.is_some();
        // The current generation must replay; anything else fails the open.
        let state = match current {
            Some(current) => resolve(&generations, current, &mut resolved)?,
            None => Resolved {
                number: 0,
                index: Arc::new(Index::new()),
                hops: 0,
                entries: 0,
            },
        };
        let mut retained_names = VecDeque::new();
        for retained in generations.read_retained()? {
            match resolve(&generations, retained, &mut resolved) {
                Ok(replayed) => retained_names.push_back(names(&replayed.index)),
                // A retained generation whose chain no longer replays keeps nothing alive, like
                // one whose own blobs fail verification.
                Err(damaged) if damaged.is_io() => {}
                Err(refused) => return Err(refused),
            }
        }
        if retained_names.is_empty() && has_current {
            retained_names.push_back(names(&state.index));
        }
        let store = Self {
            root: root.to_path_buf(),
            generations,
            objects,
            role,
            retained_generations,
            compaction,
            index: RwLock::new(state.index.clone()),
            state: Mutex::new(State {
                generation: state.number,
                chain_length: state.hops,
                chain_entries: state.entries,
                last_commit_compacted: false,
                retained_names,
                orphan_candidates: HashSet::new(),
                collect_warning: AfterCommitWarning::default(),
                closed: false,
            }),
        };
        store.finish_open(&state)?;
        Ok(store)
    }

    /// The parts of an open that only a writable store performs.
    fn finish_open(&self, state: &Resolved) -> Result<()> {
        let writable = self.role.accepts_commits();
        if state.hops > 0 {
            let note = if self.compaction.delta_generations {
                ""
            } else if writable {
                " (delta-generations is off: compacting now)"
            } else {
                " (delta-generations is off: a writable open compacts it)"
            };
            tracing::info!(
                "AWF store {} opened at generation {} with a delta chain of {} hop(s), {} entries replayed; a build before AWF-7 cannot open it until a compaction{note}",
                self.root.display(),
                state.number,
                state.hops,
                state.entries
            );
        }
        // Deltas off and a chain on disk: convert now, so that every store this server opens is
        // readable by a build without the chain parser, whether or not a commit ever touches it.
        if writable && !self.compaction.delta_generations && state.hops > 0 {
            let compacted = Store::compact(self)?;
            tracing::info!(
                "AWF store {} compacted at open to generation {} (delta-generations is off)",
                self.root.display(),
                compacted.map_or(state.number, |commit| commit.generation)
            );
        }
        // The one full scan: objects a crash left unreferenced. Only a writable store collects.
        if writable {
            let live: HashSet<ObjectName> = lock(&self.state)
                .retained_names
                .iter()
                .flatten()
                .copied()
                .collect();
            self.objects.retain_only(&live)?;
        }
        Ok(())
    }

    fn committed_index(&self) -> Arc<Index> {
        self.index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn publish_index(&self, index: Index) {
        *self
            .index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(index);
    }

    /// Delta hops from the committed generation to its full index; 0 right after a compaction.
    #[must_use]
    pub fn chain_length(&self) -> usize {
        lock(&self.state).chain_length
    }

    /// Whether the last commit wrote a full index (a compaction) rather than a delta.
    #[must_use]
    pub fn last_commit_compacted(&self) -> bool {
        lock(&self.state).last_commit_compacted
    }

    /// What the committed state says about one chunk.
    #[must_use]
    pub fn entry(&self, key: ChunkKey) -> Option<Entry> {
        self.committed_index().get(&key).copied()
    }

    fn commit_locked(&self, state: &mut State, changes: &Changes<'_>) -> Result<CommitResult> {
        if changes.mode == PersistenceMode::ReadOnly || !self.role.accepts_commits() {
            return Err(AwfError::InvalidState(format!(
                "a {} world in {} mode does not accept commits",
                self.role, changes.mode
            )));
        }
        if state.closed {
            return Err(AwfError::InvalidState(
                "this AWF store was closed; it accepts no more commits".to_string(),
            ));
        }
        let before = self.committed_index();
        let mut next = (*before).clone();
        for key in changes.removed {
            next.remove(key);
        }
        for key in changes.deleted {
            next.insert(*key, Entry::Deleted);
        }

        let mut to_verify = HashSet::new();
        let committed =
            match self.write_and_commit(state, changes, &before, &mut next, &mut to_verify) {
                Ok(committed) => committed,
                Err(failed) => {
                    // Whatever this commit wrote may now be referenced by nothing; the next successful
                    // commit checks it. A crash instead is covered by the scan when the store is opened.
                    state.orphan_candidates.extend(to_verify);
                    return Err(failed);
                }
            };
        let chunks = next.len();
        let referenced = names(&next);
        // Readers see the new index before anything the old one referenced is collected.
        self.publish_index(next);
        state.generation = committed.generation;
        state.last_commit_compacted = committed.full;
        if committed.full {
            state.chain_length = 0;
            state.chain_entries = 0;
        } else {
            state.chain_length += 1;
            state.chain_entries += committed.delta_entries;
        }
        let objects_removed = self.retain_and_collect(state, referenced);
        Ok(CommitResult {
            generation: committed.generation,
            chunks,
            objects_written: committed.objects_written,
            bytes_written: committed.bytes_written,
            objects_removed,
        })
    }

    /// Everything up to and including the commit point. `to_verify` collects the objects this
    /// commit wrote, also when it fails part-way.
    fn write_and_commit(
        &self,
        state: &State,
        changes: &Changes<'_>,
        before: &Index,
        next: &mut Index,
        to_verify: &mut HashSet<ObjectName>,
    ) -> Result<Committed> {
        let full_mode = changes.mode == PersistenceMode::Full;
        let mut objects_written = 0;
        let mut bytes_written = 0;
        for (key, bytes) in changes.changed {
            let written = self.objects.put(bytes, full_mode)?;
            next.insert(*key, Entry::Object(written.name));
            if written.bytes > 0 {
                objects_written += 1;
                bytes_written += written.bytes;
                to_verify.insert(written.name);
            }
        }
        if full_mode {
            // Full rewrites unchanged chunks as well, from their committed (verified) bytes.
            for (key, entry) in next.iter() {
                let Entry::Object(name) = entry else { continue };
                if changes.changed.contains_key(key) {
                    continue;
                }
                let written = self.objects.put(&self.objects.get(*name)?, true)?;
                objects_written += 1;
                bytes_written += written.bytes;
                to_verify.insert(written.name);
            }
        }
        if changes.mode == PersistenceMode::Checkpoint {
            to_verify.extend(names(next));
        }
        // Verify before the commit point: a generation must never reference an object that cannot
        // be read back.
        for name in to_verify.iter() {
            self.objects.get(*name)?;
        }
        // Every object the index will reference is made durable and named here, in one pass,
        // before anything that references it is written.
        self.objects.sync()?;

        // The delta: only the entries this commit changes, over the keys it touched.
        let delta = delta(before, next, changes);
        let full = changes.force_full
            || !self.compaction.delta_generations
            || state.generation == 0
            || full_mode
            || state.chain_length >= self.compaction.max_chain_length
            || state.chain_entries + delta.len() as u64 >= next.len() as u64;
        let generation = if full {
            let blobs = BTreeMap::from([(INDEX_BLOB.to_string(), encode_index(next))]);
            self.generations.commit(&blobs)?
        } else {
            let blobs = BTreeMap::from([(DELTA_BLOB.to_string(), encode_delta(&delta))]);
            self.generations.commit_chained(&blobs, state.generation)?
        };
        Ok(Committed {
            generation,
            full,
            delta_entries: delta.len() as u64,
            objects_written,
            bytes_written,
        })
    }

    /// After a commit: records the new generation's objects as retained and collects only what
    /// can have become garbage, the objects of generations that just fell out of retention and
    /// those of earlier failed commits, minus everything still retained. Returns how many objects
    /// it deleted.
    ///
    /// Runs past the commit point, so it never fails: an object it cannot delete stays a candidate
    /// for the next commit, and the failure is logged. Reporting the commit as failed would make
    /// the caller retry it, committing yet another generation.
    fn retain_and_collect(&self, state: &mut State, referenced: HashSet<ObjectName>) -> usize {
        state.retained_names.push_front(referenced);
        let mut candidates = std::mem::take(&mut state.orphan_candidates);
        while state.retained_names.len() > self.retained_generations {
            if let Some(dropped) = state.retained_names.pop_back() {
                candidates.extend(dropped);
            }
        }
        if candidates.is_empty() {
            return 0;
        }
        let live: HashSet<ObjectName> = state.retained_names.iter().flatten().copied().collect();
        let mut removed = 0;
        let mut first_failure = None;
        for name in candidates {
            if live.contains(&name) {
                continue;
            }
            match self.objects.delete(name) {
                Ok(true) => removed += 1,
                Ok(false) => {}
                Err(failed) => {
                    state.orphan_candidates.insert(name);
                    first_failure.get_or_insert(failed);
                }
            }
        }
        if let Some(failed) = first_failure {
            let message = format!(
                "AWF store {} committed generation {}, but {} object(s) no retained generation references could not be deleted; the commit stands and the next commit tries again",
                self.root.display(),
                state.generation,
                state.orphan_candidates.len()
            );
            state.collect_warning.failed(&message, &failed);
        }
        removed
    }

    /// The generations [`Self::rollback_to`] accepts, newest first: the retained committed
    /// generations whose chain still replays, the current one included.
    pub fn rollback_points(&self) -> Result<Vec<u64>> {
        let _state = lock(&self.state);
        self.replayable_retained()
    }

    fn replayable_retained(&self) -> Result<Vec<u64>> {
        let mut points = Vec::new();
        let mut resolved = HashMap::new();
        for retained in self.generations.read_retained()? {
            let number = retained.number;
            match resolve(&self.generations, retained, &mut resolved) {
                Ok(_) => points.push(number),
                // A generation that does not replay is no rollback point.
                Err(damaged) if damaged.is_io() => {}
                Err(refused) => return Err(refused),
            }
        }
        Ok(points)
    }

    /// Point-in-time rollback for operator recovery: commits, as a new generation on top, the
    /// state retained generation `target` committed. Nothing is rewritten: the generations after
    /// `target` stay on disk until retention drops them, so a rollback can itself be rolled back
    /// while they are retained. The new generation is always a full index, and every object it
    /// references is read back and verified first, so a damaged or missing object fails the
    /// rollback before its commit point and leaves the store as it was.
    ///
    /// Only for a store no running world is using: a loaded world keeps its own dirty chunks and
    /// would commit over the rollback. Returns `None` when `target` is already the current
    /// generation.
    pub fn rollback_to(&self, target: u64) -> Result<Option<CommitResult>> {
        let mut state = lock(&self.state);
        if !self.role.accepts_commits() {
            return Err(AwfError::InvalidState(format!(
                "a {} world does not accept commits",
                self.role
            )));
        }
        if state.closed {
            return Err(AwfError::InvalidState(
                "this AWF store was closed; it accepts no more commits".to_string(),
            ));
        }
        if target == state.generation && state.generation != 0 {
            return Ok(None);
        }
        let point = self
            .generations
            .read_retained()?
            .into_iter()
            .find(|retained| retained.number == target);
        let Some(point) = point else {
            return Err(AwfError::InvalidArgument(format!(
                "generation {target} is not a retained rollback point; retained: {:?}",
                self.replayable_retained()?
            )));
        };
        let index = resolve(&self.generations, point, &mut HashMap::new())?.index;
        for name in names(&index) {
            // Read back and verified before the commit point.
            self.objects.get(name)?;
        }
        let blobs = BTreeMap::from([(INDEX_BLOB.to_string(), encode_index(&index))]);
        let committed = self.generations.commit(&blobs)?;
        let chunks = index.len();
        self.publish_index((*index).clone());
        state.generation = committed;
        state.last_commit_compacted = true;
        state.chain_length = 0;
        state.chain_entries = 0;
        let objects_removed = self.retain_and_collect(&mut state, names(&index));
        tracing::warn!(
            "AWF store rolled back to the state of generation {target} as generation {committed} ({chunks} chunks)"
        );
        Ok(Some(CommitResult {
            generation: committed,
            chunks,
            objects_written: 0,
            bytes_written: 0,
            objects_removed,
        }))
    }
}

impl ChunkSource for WorldStore {
    fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>> {
        match self.entry(key) {
            Some(Entry::Object(name)) => self.objects.get(name).map(Some),
            Some(Entry::Deleted) | None => Ok(None),
        }
    }

    /// Chunks with committed bytes; deleted chunks are not included.
    fn keys(&self) -> BTreeSet<ChunkKey> {
        self.committed_index()
            .iter()
            .filter(|(_, entry)| matches!(entry, Entry::Object(_)))
            .map(|(key, _)| *key)
            .collect()
    }
}

impl Store for WorldStore {
    fn has(&self, key: ChunkKey) -> bool {
        self.committed_index().contains_key(&key)
    }

    fn deleted(&self) -> BTreeSet<ChunkKey> {
        self.committed_index()
            .iter()
            .filter(|(_, entry)| matches!(entry, Entry::Deleted))
            .map(|(key, _)| *key)
            .collect()
    }

    fn stores_bytes(&self, key: ChunkKey) -> bool {
        matches!(self.entry(key), Some(Entry::Object(_)))
    }

    fn generation(&self) -> Result<u64> {
        self.generations.current_generation()
    }

    fn commit(
        &self,
        changed: &BTreeMap<ChunkKey, Vec<u8>>,
        removed: &BTreeSet<ChunkKey>,
        deleted: &BTreeSet<ChunkKey>,
        mode: PersistenceMode,
    ) -> Result<CommitResult> {
        let mut state = lock(&self.state);
        self.commit_locked(
            &mut state,
            &Changes {
                changed,
                removed,
                deleted,
                mode,
                force_full: false,
            },
        )
    }

    /// Refuses every later commit; reads still work. It waits for a commit that is running.
    fn close(&self) -> Result<()> {
        lock(&self.state).closed = true;
        Ok(())
    }

    /// Whether the committed generation is a full index (or there is none).
    fn full_index(&self) -> bool {
        self.chain_length() == 0
    }

    /// Commits the current state as a full index, ending the chain. An empty store has nothing to
    /// compact and returns `None`; so does one whose generation is full already.
    fn compact(&self) -> Result<Option<CommitResult>> {
        let mut state = lock(&self.state);
        if state.generation == 0 || state.chain_length == 0 {
            return Ok(None);
        }
        let nothing = BTreeSet::new();
        self.commit_locked(
            &mut state,
            &Changes {
                changed: &BTreeMap::new(),
                removed: &nothing,
                deleted: &nothing,
                mode: PersistenceMode::Incremental,
                force_full: true,
            },
        )
        .map(Some)
    }
}

/// Replays a generation's chain: walks down through [`GenerationStore::read_base`] (which checks
/// each hash link) to a full index or an already resolved generation, then applies the deltas
/// oldest to newest. Every generation on the way is memoised in `resolved`.
fn resolve(
    generations: &GenerationStore,
    top: Generation,
    resolved: &mut HashMap<u64, Resolved>,
) -> Result<Resolved> {
    let mut chain = Vec::new();
    let mut base: Option<Resolved> = None;
    let mut cursor = Some(top);
    while let Some(generation) = cursor {
        if let Some(known) = resolved.get(&generation.number) {
            base = Some(known.clone());
            break;
        }
        // A delta without a base, or an index with one, is corrupt before anything is replayed.
        kind(&generation)?;
        cursor = generations.read_base(&generation)?;
        chain.push(generation);
    }
    for generation in chain.into_iter().rev() {
        let next = if kind(&generation)? == INDEX_BLOB {
            Resolved {
                number: generation.number,
                index: Arc::new(decode_index(&generation)?),
                hops: 0,
                entries: 0,
            }
        } else {
            let Some(below) = &base else {
                return Err(AwfError::CorruptGeneration(format!(
                    "generation {} holds a delta but is chained to nothing",
                    generation.number
                )));
            };
            let mut index = (*below.index).clone();
            let applied = apply_delta(&mut index, &generation)?;
            Resolved {
                number: generation.number,
                index: Arc::new(index),
                hops: below.hops + 1,
                entries: below.entries + applied,
            }
        };
        resolved.insert(generation.number, next.clone());
        base = Some(next);
    }
    base.ok_or_else(|| AwfError::CorruptGeneration("an empty generation chain".to_string()))
}

/// Which blob a generation carries, after checking it is chained consistently.
fn kind(generation: &Generation) -> Result<&'static str> {
    let index = generation.blobs.contains_key(INDEX_BLOB);
    let delta = generation.blobs.contains_key(DELTA_BLOB);
    let number = generation.number;
    if index == delta {
        let what = if index {
            "both an index and a delta"
        } else {
            "no index"
        };
        return Err(AwfError::CorruptGeneration(format!(
            "generation {number} has {what}"
        )));
    }
    if index && generation.has_base() {
        return Err(AwfError::CorruptGeneration(format!(
            "generation {number} holds a full index but is chained to {}",
            generation.base
        )));
    }
    if delta && !generation.has_base() {
        return Err(AwfError::CorruptGeneration(format!(
            "generation {number} holds a delta but is chained to nothing"
        )));
    }
    Ok(if index { INDEX_BLOB } else { DELTA_BLOB })
}

/// The objects an index references.
fn names(index: &Index) -> HashSet<ObjectName> {
    index
        .values()
        .filter_map(|entry| match entry {
            Entry::Object(name) => Some(*name),
            Entry::Deleted => None,
        })
        .collect()
}

/// The entries that differ between two indexes over the touched keys: a new value, a tombstone, or
/// a drop for a key that left. Keys that end up unchanged (a chunk rewritten with identical bytes)
/// are not in the delta.
fn delta(before: &Index, after: &Index, changes: &Changes<'_>) -> BTreeMap<ChunkKey, Change> {
    let mut delta = BTreeMap::new();
    for key in changes.removed {
        if before.contains_key(key) && !after.contains_key(key) {
            delta.insert(*key, Change::Drop);
        }
    }
    for key in changes.changed.keys().chain(changes.deleted) {
        if let Some(entry) = after.get(key)
            && before.get(key) != Some(entry)
        {
            delta.insert(*key, Change::Set(*entry));
        }
    }
    delta
}

fn encode_entry(out: &mut String, key: ChunkKey, value: &str) {
    let _ = writeln!(out, "{} {} {value}", key.x, key.z);
}

fn entry_text(entry: Entry) -> String {
    match entry {
        Entry::Object(name) => name.to_hex(),
        Entry::Deleted => TOMBSTONE.to_string(),
    }
}

fn encode_index(index: &Index) -> Vec<u8> {
    let mut out = String::with_capacity(index.len() * 80);
    for (key, entry) in index {
        encode_entry(&mut out, *key, &entry_text(*entry));
    }
    out.into_bytes()
}

/// Same line form as the index; `drop` is the only extra value.
fn encode_delta(delta: &BTreeMap<ChunkKey, Change>) -> Vec<u8> {
    let mut out = String::with_capacity(delta.len() * 80);
    for (key, change) in delta {
        match change {
            Change::Set(entry) => encode_entry(&mut out, *key, &entry_text(*entry)),
            Change::Drop => encode_entry(&mut out, *key, DROP),
        }
    }
    out.into_bytes()
}

fn decode_index(generation: &Generation) -> Result<Index> {
    let Some(bytes) = generation.blobs.get(INDEX_BLOB) else {
        return Err(AwfError::CorruptGeneration(format!(
            "generation {} has no index",
            generation.number
        )));
    };
    let mut index = Index::new();
    for (key, change) in decode_entries(bytes, false)? {
        if let Change::Set(entry) = change {
            index.insert(key, entry);
        }
    }
    Ok(index)
}

/// Applies a generation's delta to `index`; returns how many entries it held.
fn apply_delta(index: &mut Index, generation: &Generation) -> Result<u64> {
    let Some(bytes) = generation.blobs.get(DELTA_BLOB) else {
        return Err(AwfError::CorruptGeneration(format!(
            "generation {} has no delta",
            generation.number
        )));
    };
    let delta = decode_entries(bytes, true)?;
    for (key, change) in &delta {
        match change {
            Change::Set(entry) => index.insert(*key, *entry),
            Change::Drop => index.remove(key),
        };
    }
    Ok(delta.len() as u64)
}

/// Parses index or delta lines. The value must be an object name, a tombstone, or in a delta a
/// drop: anything else, such as a path, makes the generation corrupt, so a store copied from
/// elsewhere can never name a file outside its object directory.
fn decode_entries(bytes: &[u8], is_delta: bool) -> Result<BTreeMap<ChunkKey, Change>> {
    let mut entries = BTreeMap::new();
    let text = String::from_utf8_lossy(bytes);
    for line in split_java(&text, '\n') {
        if line.is_empty() {
            continue;
        }
        let parts = split_java(line, ' ');
        let parsed = match parts.as_slice() {
            [x, z, value] => {
                let change = match *value {
                    TOMBSTONE => Some(Change::Set(Entry::Deleted)),
                    DROP if is_delta => Some(Change::Drop),
                    name => ObjectName::parse(name).map(|name| Change::Set(Entry::Object(name))),
                };
                match (x.parse::<i32>(), z.parse::<i32>(), change) {
                    (Ok(x), Ok(z), Some(change)) => Some((ChunkKey::new(x, z), change)),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((key, change)) = parsed else {
            let what = if is_delta { "delta" } else { "index" };
            return Err(AwfError::CorruptGeneration(format!(
                "malformed {what} line: {line}"
            )));
        };
        entries.insert(key, change);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests;
