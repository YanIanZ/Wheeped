//! One world at runtime: the chunks it owns, layered copy-on-write over a base and committed
//! through a [`Store`].
//!
//! Ownership. [`World::write`] is called by whoever owns the chunk, with bytes that are already
//! serialized. They are copied, so the caller may reuse its buffer at once. Stored bytes are never
//! changed afterwards (a later write replaces them), which lets a commit carry them without
//! keeping writers out. Queueing a save never blocks its caller.
//!
//! Copy-on-write. Reads fall through to the base (an image, a store, or one layered over the
//! other) until the first write, which makes the chunk world-owned. The base is never written, so
//! any number of instances can share one template and stay isolated.
//!
//! Memory bound. With a resident limit, chunks beyond it that are clean (committed to the store
//! and not rewritten since) are dropped after a save, least recently used first. A dropped chunk
//! is read back from the store. Dirty chunks are never dropped, so the limit can be exceeded by
//! unsaved work; that is the price of never losing a write.
//!
//! Deletion. [`World::delete`] records that the world no longer has a chunk. The deletion shadows
//! the base exactly as a write does, and is committed as a tombstone, so the chunk does not
//! reappear from the base after a save or a restart.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::commit_lock::CommitLock;
use crate::error::{AwfError, Result};
use crate::key::ChunkKey;
use crate::lock;
use crate::metrics::{Metrics, MetricsSnapshot};
use crate::role::{PersistenceMode, WorldRole};
use crate::shutdown::ShutdownBudget;
use crate::source::ChunkSource;
use crate::store::{CommitResult, Store};

/// Saves of one world that may be queued or running at once. The bound keeps a stalled backend
/// from piling them up.
pub const MAX_PENDING_SAVES: usize = 16;

/// A store a world shares with whoever else reads it.
pub type SharedStore = Arc<dyn Store + Send + Sync>;

/// A chunk source a world shares with whoever else reads it.
pub type SharedSource = Arc<dyn ChunkSource + Send + Sync>;

/// Where a world reads the chunks it does not own.
#[derive(Clone)]
pub enum WorldBase {
    /// A source that is not a store: an image, or one source layered over another.
    Source(SharedSource),
    /// A committed store, a template's for example. It can answer for one chunk without listing
    /// all of them, and it can say whether a read would fetch bytes.
    Store(SharedStore),
}

impl WorldBase {
    fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Source(source) => source.read(key),
            Self::Store(store) => store.read(key),
        }
    }

    fn keys(&self) -> BTreeSet<ChunkKey> {
        match self {
            Self::Source(source) => source.keys(),
            Self::Store(store) => store.keys(),
        }
    }
}

/// What a world holds in memory for one chunk.
#[derive(Clone)]
enum Held {
    /// The chunk's bytes. They are never changed; a later write replaces them.
    Bytes(Arc<[u8]>),
    /// The world deleted the chunk.
    Deleted,
}

/// One chunk a world holds in memory.
struct Entry {
    held: Held,
    /// The write that dirtied the chunk, unique per write; 0 once it is committed.
    version: u64,
    /// When the chunk became dirty; `None` while it is clean.
    dirty_since: Option<Instant>,
    /// The last read or write of the chunk on the world's access counter, for dropping the least
    /// recently used first.
    accessed: u64,
}

impl Entry {
    const fn dirty(&self) -> bool {
        self.version != 0
    }
}

/// The chunks a world holds in memory. One lock covers all of it, so a chunk's bytes, its dirty
/// state and its place in `dirty` always change together.
#[derive(Default)]
struct Chunks {
    owned: HashMap<ChunkKey, Entry>,
    /// The keys of the dirty entries in `owned`, so that counting, sampling and snapshotting them
    /// never walks the clean ones.
    dirty: BTreeSet<ChunkKey>,
    versions: u64,
    access_clock: u64,
}

#[derive(Default)]
struct PendingSaves {
    /// Each pending save's ticket and when it was queued, oldest first.
    queue: VecDeque<(u64, Instant)>,
    tickets: u64,
}

/// The dirty chunks one commit carries.
#[derive(Default)]
struct Taken {
    /// Each chunk taken with the write that was taken, to tell it from a later one.
    versions: Vec<(ChunkKey, u64)>,
    changed: BTreeMap<ChunkKey, Vec<u8>>,
    deleted: BTreeSet<ChunkKey>,
}

/// One Aurora World Fabric world at runtime.
pub struct World {
    name: String,
    role: WorldRole,
    base: Option<WorldBase>,
    store: Option<SharedStore>,
    /// World-owned chunks kept in memory after a save; 0 means unlimited.
    resident_limit: usize,
    metrics: Metrics,
    chunks: Mutex<Chunks>,
    pending_saves: Mutex<PendingSaves>,
    evicted: AtomicU64,
    /// Held from the snapshot of dirty chunks until the store has the commit, so commits reach
    /// the store in snapshot order. Without it a periodic commit that snapshotted a chunk could
    /// lose the race for the store to a flush that snapshotted the chunk's newer bytes, land last,
    /// and leave the store with the older bytes while the chunk was no longer dirty.
    commit_lock: CommitLock,
    uncertain_chunks: AtomicUsize,
    /// Set by [`World::fence`] and the two ways of closing the store. Read under the commit lock
    /// before a commit takes its snapshot, so a commit either started before the fence (and
    /// completes) or commits nothing.
    fenced: AtomicBool,
}

/// A save that holds one of its world's pending slots until it has run.
///
/// Running it is the storage lane's job. Dropping it without running frees the slot and counts as
/// a failed save.
#[must_use = "a queued save commits nothing until it is run"]
pub struct QueuedSave {
    world: Arc<World>,
    mode: PersistenceMode,
    max_attempts: usize,
    ticket: u64,
    ran: bool,
}

impl QueuedSave {
    /// Commits the world's dirty chunks on the calling thread.
    ///
    /// The dirty chunks are taken now, not when the save was queued. A chunk rewritten while the
    /// commit is in flight stays dirty for the next save. A failed disk or backend call is retried
    /// up to the attempts the save was queued with; after that the save fails and the chunks stay
    /// dirty. `Ok(None)` means there was nothing to commit and the store was not asked.
    pub fn run(mut self) -> Result<Option<CommitResult>> {
        self.ran = true;
        self.world.commit(self.mode, self.max_attempts)
    }
}

impl Drop for QueuedSave {
    fn drop(&mut self) {
        let mut pending = lock(&self.world.pending_saves);
        if let Some(at) = pending
            .queue
            .iter()
            .position(|(ticket, _)| *ticket == self.ticket)
        {
            pending.queue.remove(at);
        }
        drop(pending);
        if !self.ran {
            self.world.metrics.failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl World {
    /// A world with no resident limit.
    ///
    /// `base` is where chunks the world does not own are read from, `None` for an empty world.
    /// `store` is where commits go, `None` for a world that is never persisted.
    pub fn new(
        name: impl Into<String>,
        role: WorldRole,
        base: Option<WorldBase>,
        store: Option<SharedStore>,
    ) -> Result<Self> {
        Self::with_resident_limit(name, role, base, store, 0)
    }

    /// As [`Self::new`], keeping at most `resident_limit` world-owned chunks in memory after a
    /// save; 0 means unlimited.
    pub fn with_resident_limit(
        name: impl Into<String>,
        role: WorldRole,
        base: Option<WorldBase>,
        store: Option<SharedStore>,
        resident_limit: usize,
    ) -> Result<Self> {
        if store.is_none() && role.accepts_commits() {
            return Err(AwfError::InvalidArgument(format!(
                "a {role} world needs a store"
            )));
        }
        Ok(Self {
            name: name.into(),
            role,
            base,
            store,
            resident_limit,
            metrics: Metrics::default(),
            chunks: Mutex::default(),
            pending_saves: Mutex::default(),
            evicted: AtomicU64::new(0),
            commit_lock: CommitLock::default(),
            uncertain_chunks: AtomicUsize::new(0),
            fenced: AtomicBool::new(false),
        })
    }

    /// The world's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The world's role.
    #[must_use]
    pub const fn role(&self) -> WorldRole {
        self.role
    }

    /// The chunk's bytes: the world's own if it wrote the chunk, else the base's. `None` when the
    /// world deleted the chunk or nothing has it.
    pub fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>> {
        if let Some(mine) = self.touch(key) {
            return Ok(match mine {
                Held::Bytes(bytes) => Some(bytes.to_vec()),
                Held::Deleted => None,
            });
        }
        // A chunk this world wrote (or deleted), committed and then dropped from memory lives in
        // the store; a committed deletion reads as absent rather than falling through to the base.
        if let Some(store) = &self.store
            && store.has(key)
        {
            let committed = store.read(key)?;
            if let Some(bytes) = &committed {
                self.count_read(bytes);
            }
            return Ok(committed);
        }
        let Some(base) = &self.base else {
            return Ok(None);
        };
        let from_base = base.read(key)?;
        if let Some(bytes) = &from_base {
            self.metrics.materialized.fetch_add(1, Ordering::Relaxed);
            self.count_read(bytes);
        }
        Ok(from_base)
    }

    /// What the world holds in memory for the chunk, counting this as its latest use.
    fn touch(&self, key: ChunkKey) -> Option<Held> {
        let chunks = &mut *lock(&self.chunks);
        let entry = chunks.owned.get_mut(&key)?;
        chunks.access_clock += 1;
        entry.accessed = chunks.access_clock;
        Some(entry.held.clone())
    }

    fn count_read(&self, bytes: &[u8]) {
        self.metrics
            .bytes_read
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    }

    /// Every chunk this world can return.
    #[must_use]
    pub fn keys(&self) -> BTreeSet<ChunkKey> {
        let mut keys = self
            .base
            .as_ref()
            .map_or_else(BTreeSet::new, WorldBase::keys);
        self.overlay(&mut keys);
        keys
    }

    /// Chunks this world holds itself rather than reading them from its base: committed, or
    /// waiting to be.
    #[must_use]
    pub fn owned_count(&self) -> usize {
        let mut keys = BTreeSet::new();
        self.overlay(&mut keys);
        keys.len()
    }

    /// Applies what the store says, and then what memory says, on top of `keys`.
    fn overlay(&self, keys: &mut BTreeSet<ChunkKey>) {
        if let Some(store) = &self.store {
            keys.extend(store.keys());
            for deleted in store.deleted() {
                keys.remove(&deleted);
            }
        }
        let chunks = lock(&self.chunks);
        for (key, entry) in &chunks.owned {
            match entry.held {
                Held::Bytes(_) => {
                    keys.insert(*key);
                }
                Held::Deleted => {
                    keys.remove(key);
                }
            }
        }
    }

    /// Takes ownership of a chunk's new bytes. Called by whoever owns the chunk.
    pub fn write(&self, key: ChunkKey, serialized: &[u8]) -> Result<()> {
        self.require_mutable()?;
        self.put(key, Held::Bytes(Arc::from(serialized)));
        Ok(())
    }

    /// Records that the world no longer has a chunk. Called by whoever owns the chunk. The
    /// deletion shadows the base until the chunk is written again.
    pub fn delete(&self, key: ChunkKey) -> Result<()> {
        self.require_mutable()?;
        self.put(key, Held::Deleted);
        Ok(())
    }

    fn require_mutable(&self) -> Result<()> {
        if self.role.mutable() {
            Ok(())
        } else {
            Err(AwfError::InvalidState(format!(
                "a {} world is not mutable",
                self.role
            )))
        }
    }

    /// Installs a dirty entry. A chunk that is already dirty keeps the time it became dirty.
    fn put(&self, key: ChunkKey, held: Held) {
        let now = Instant::now();
        let mut chunks = lock(&self.chunks);
        chunks.versions += 1;
        chunks.access_clock += 1;
        let dirty_since = chunks
            .owned
            .get(&key)
            .and_then(|previous| previous.dirty_since)
            .unwrap_or(now);
        let entry = Entry {
            held,
            version: chunks.versions,
            dirty_since: Some(dirty_since),
            accessed: chunks.access_clock,
        };
        chunks.owned.insert(key, entry);
        chunks.dirty.insert(key);
    }

    /// Whether the base (a template, an image) has the chunk. A base that is a store answers
    /// without listing its chunks.
    #[must_use]
    pub fn base_has(&self, key: ChunkKey) -> bool {
        match &self.base {
            None => false,
            Some(WorldBase::Store(store)) => store.has(key),
            Some(WorldBase::Source(source)) => source.keys().contains(&key),
        }
    }

    /// Whether this world has written or deleted the chunk rather than reading it through from
    /// the base. A chunk it does not own is the base's business.
    #[must_use]
    pub fn owns(&self, key: ChunkKey) -> bool {
        self.resident(key) || self.store.as_ref().is_some_and(|store| store.has(key))
    }

    /// Chunks written or deleted since the last successful save.
    #[must_use]
    pub fn dirty_count(&self) -> usize {
        lock(&self.chunks).dirty.len()
    }

    /// Whether the unsaved chunks hold at least `limit` bytes of stored data; a deletion counts as
    /// zero. It stops counting once the limit is reached, so a large backlog costs at most about
    /// `limit` bytes' worth of chunks, not a walk of every dirty entry. A `limit` of zero is
    /// always met.
    #[must_use]
    pub fn dirty_bytes_at_least(&self, limit: u64) -> bool {
        if limit == 0 {
            return true;
        }
        let chunks = lock(&self.chunks);
        let mut total: u64 = 0;
        for key in &chunks.dirty {
            if let Some(Entry {
                held: Held::Bytes(bytes),
                ..
            }) = chunks.owned.get(key)
            {
                total = total.saturating_add(bytes.len() as u64);
                if total >= limit {
                    return true;
                }
            }
        }
        false
    }

    /// Saves queued or running.
    #[must_use]
    pub fn pending_save_count(&self) -> usize {
        lock(&self.pending_saves).queue.len()
    }

    /// Whether [`Self::read`] of the chunk would fetch bytes from the store or from a template
    /// store: it is not held in memory, not a deletion and not absent. A base that is not a store
    /// answers `false`.
    #[must_use]
    pub fn read_leaves_memory(&self, key: ChunkKey) -> bool {
        if self.resident(key) {
            return false;
        }
        if let Some(store) = &self.store
            && store.has(key)
        {
            return store.stores_bytes(key);
        }
        matches!(&self.base, Some(WorldBase::Store(template)) if template.stores_bytes(key))
    }

    /// Whether the world holds the chunk in memory: its bytes, or the fact that it deleted it.
    #[must_use]
    pub fn resident(&self, key: ChunkKey) -> bool {
        lock(&self.chunks).owned.contains_key(&key)
    }

    /// Queues a commit of the dirty chunks and returns at once. The caller hands the returned save
    /// to its storage lane, which runs it.
    ///
    /// A world that does not persist, a `max_attempts` below 1 and a world that already has
    /// [`MAX_PENDING_SAVES`] saves pending are refused here; the last one counts as a failed save.
    pub fn save(
        self: &Arc<Self>,
        mode: PersistenceMode,
        max_attempts: usize,
    ) -> Result<QueuedSave> {
        self.require_saving(mode)?;
        require_attempts(max_attempts)?;
        let ticket = {
            let mut pending = lock(&self.pending_saves);
            (pending.queue.len() < MAX_PENDING_SAVES).then(|| {
                pending.tickets += 1;
                let ticket = pending.tickets;
                pending.queue.push_back((ticket, Instant::now()));
                ticket
            })
        };
        let Some(ticket) = ticket else {
            self.metrics.failures.fetch_add(1, Ordering::Relaxed);
            return Err(AwfError::Rejected(format!(
                "{MAX_PENDING_SAVES} saves already pending for {}; the backend is not keeping up, \
                 refusing rather than queueing more",
                self.name
            )));
        };
        Ok(QueuedSave {
            world: Arc::clone(self),
            mode,
            max_attempts,
            ticket,
            ran: false,
        })
    }

    /// Commits the dirty chunks on the calling thread and waits for it. For flushes and shutdown,
    /// where the caller is already off the tick and must know the data is durable before it
    /// continues.
    ///
    /// When every attempt failed the last failure is returned and the chunks stay dirty.
    /// `Ok(None)` means there was nothing to commit and the store was not asked.
    pub fn save_now(
        &self,
        mode: PersistenceMode,
        max_attempts: usize,
    ) -> Result<Option<CommitResult>> {
        self.require_saving(mode)?;
        require_attempts(max_attempts)?;
        self.commit(mode, max_attempts)
    }

    /// [`Self::save_now`] within the shutdown budget: the commit lock is taken with the time that
    /// is left, no attempt starts once the deadline has passed, and an attempt cut off by it is
    /// not retried.
    ///
    /// When the deadline stopped it the failure is [`AwfError::DeadlineExceeded`]. Its
    /// `request_sent` is `true` when a commit was sent (by this call, or by the commit still
    /// running that held the lock) and may have been applied, and `false` when nothing of this
    /// commit reached the backend.
    pub fn save_now_within(
        &self,
        mode: PersistenceMode,
        max_attempts: usize,
        budget: &ShutdownBudget,
    ) -> Result<Option<CommitResult>> {
        self.require_saving(mode)?;
        require_attempts(max_attempts)?;
        if budget.expired() {
            return Err(AwfError::DeadlineExceeded {
                message: "the AWF shutdown deadline passed before this commit could start"
                    .to_string(),
                request_sent: false,
            });
        }
        let Some(_running) = self.commit_lock.try_lock_within(budget) else {
            // Another commit (a periodic one on the storage lane) holds the lock past the deadline.
            return Err(AwfError::DeadlineExceeded {
                message: format!(
                    "a commit of {} was still running at the AWF shutdown deadline; this commit \
                     could not start",
                    self.name
                ),
                request_sent: true,
            });
        };
        self.refuse_if_fenced("this commit was refused before it took its snapshot")?;
        self.commit_in_order(mode, max_attempts, Some(budget))
    }

    fn require_saving(&self, mode: PersistenceMode) -> Result<()> {
        if self.store.is_none() || !self.role.accepts_commits() || mode == PersistenceMode::ReadOnly
        {
            return Err(AwfError::InvalidState(format!(
                "a {} world in {mode} mode does not save",
                self.role
            )));
        }
        Ok(())
    }

    fn refuse_if_fenced(&self, what: &str) -> Result<()> {
        if self.fenced() {
            return Err(AwfError::Fenced(format!(
                "AWF world {} was discarded or closed; {what}",
                self.name
            )));
        }
        Ok(())
    }

    /// Writes the store's committed state as a self-contained generation, under the commit lock so
    /// that it never interleaves with a commit and is refused once the world is fenced. Dirty
    /// chunks are not part of it; a flush before or after carries them.
    ///
    /// `Ok(None)` when the store had nothing to compact.
    pub fn compact(&self) -> Result<Option<CommitResult>> {
        let Some(store) = self.store.as_ref().filter(|_| self.role.accepts_commits()) else {
            return Err(AwfError::InvalidState(format!(
                "a {} world does not compact",
                self.role
            )));
        };
        let _running = self.commit_lock.lock();
        self.refuse_if_fenced("no compaction")?;
        store.compact()
    }

    /// Whether the store's committed state is self-contained; `true` without a store.
    #[must_use]
    pub fn full_index(&self) -> bool {
        self.store.as_ref().is_none_or(|store| store.full_index())
    }

    /// The store's committed generation, 0 before the first commit or without a store.
    pub fn store_generation(&self) -> Result<u64> {
        self.store
            .as_ref()
            .map_or(Ok(0), |store| store.generation())
    }

    /// Closes the store after the last commit. The world is fenced first and a commit that is
    /// already running is waited for, so the store (and a network backend's claim on the world) is
    /// never released under a commit, and no commit starts on a closed store.
    pub fn close_store(&self) -> Result<()> {
        let _running = self.commit_lock.lock();
        self.fence();
        self.store.as_ref().map_or(Ok(()), |store| store.close())
    }

    /// [`Self::close_store`] within the shutdown budget. The world is fenced either way. When a
    /// commit is still running at the deadline the store is not closed under it (a network
    /// backend's claim then expires on its own) and this returns `false`.
    pub fn close_store_within(&self, budget: &ShutdownBudget) -> Result<bool> {
        self.fence();
        let Some(_running) = self.commit_lock.try_lock_within(budget) else {
            return Ok(false);
        };
        if let Some(store) = &self.store {
            store.close()?;
        }
        Ok(true)
    }

    /// Whether a commit holds the commit lock right now: running, not queued.
    #[must_use]
    pub fn commit_running(&self) -> bool {
        self.commit_lock.is_locked()
    }

    /// Chunks of the last commit attempt whose outcome is unknown (sent, then cut off), 0 for
    /// none.
    #[must_use]
    pub fn uncertain_chunks(&self) -> usize {
        self.uncertain_chunks.load(Ordering::SeqCst)
    }

    /// Up to `limit` of the dirty positions, lowest first, for reports.
    #[must_use]
    pub fn dirty_sample(&self, limit: usize) -> Vec<ChunkKey> {
        lock(&self.chunks)
            .dirty
            .iter()
            .take(limit)
            .copied()
            .collect()
    }

    /// Refuses every commit that has not started yet, also the ones already queued by
    /// [`Self::save`]: each fails with [`AwfError::Fenced`] and commits nothing. A commit that is
    /// already running completes as one atomic commit. Never blocks; there is no way back.
    pub fn fence(&self) {
        self.fenced.store(true, Ordering::SeqCst);
    }

    /// Whether the world was fenced, by [`Self::fence`] or by closing its store.
    #[must_use]
    pub fn fenced(&self) -> bool {
        self.fenced.load(Ordering::SeqCst)
    }

    fn commit(&self, mode: PersistenceMode, max_attempts: usize) -> Result<Option<CommitResult>> {
        let _running = self.commit_lock.lock();
        self.refuse_if_fenced("this commit was refused before it took its snapshot")?;
        self.commit_in_order(mode, max_attempts, None)
    }

    /// The commit itself; the caller holds the commit lock. `budget` is the shutdown budget, or
    /// `None` outside shutdown.
    fn commit_in_order(
        &self,
        mode: PersistenceMode,
        max_attempts: usize,
        budget: Option<&ShutdownBudget>,
    ) -> Result<Option<CommitResult>> {
        let Some(store) = &self.store else {
            return self.require_saving(mode).map(|()| None);
        };
        let prepared = Instant::now();
        let taken = self.take_dirty();
        self.metrics.serialization.record(prepared.elapsed());
        if mode == PersistenceMode::Incremental
            && taken.changed.is_empty()
            && taken.deleted.is_empty()
        {
            // Nothing to commit: a save queued behind another one must not cost the store an empty
            // generation or a backend round trip. FULL and CHECKPOINT rewrite or verify what the
            // store already holds, so they still run.
            return Ok(None);
        }
        let removed = BTreeSet::new();
        let mut attempt: usize = 1;
        loop {
            if budget.is_some_and(ShutdownBudget::expired) {
                self.metrics.failures.fetch_add(1, Ordering::Relaxed);
                return Err(AwfError::DeadlineExceeded {
                    message: format!(
                        "the AWF shutdown deadline passed before commit attempt {attempt} of {} \
                         could start",
                        self.name
                    ),
                    // An earlier attempt of this commit failed and may have reached the backend.
                    request_sent: attempt > 1,
                });
            }
            let begun = Instant::now();
            match store.commit(&taken.changed, &removed, &taken.deleted, mode) {
                Ok(committed) => {
                    self.metrics.backend.record(begun.elapsed());
                    self.metrics
                        .bytes_written
                        .fetch_add(committed.bytes_written, Ordering::Relaxed);
                    self.mark_committed(&taken.versions, prepared);
                    self.uncertain_chunks.store(0, Ordering::SeqCst);
                    self.evict_clean_beyond_limit();
                    return Ok(Some(committed));
                }
                Err(failure) => {
                    // A failed disk or backend call is tried again. A refused call is not, and
                    // neither is a commit cut off by the deadline: the deadline is shared, and a
                    // retry could only wait for nothing.
                    let cut_off = matches!(failure, AwfError::DeadlineExceeded { .. });
                    if failure.is_io() && !cut_off && attempt < max_attempts {
                        self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                        attempt += 1;
                        continue;
                    }
                    self.metrics.failures.fetch_add(1, Ordering::Relaxed);
                    if let AwfError::DeadlineExceeded {
                        request_sent: true, ..
                    } = &failure
                    {
                        self.uncertain_chunks
                            .store(taken.changed.len() + taken.deleted.len(), Ordering::SeqCst);
                    }
                    return Err(failure);
                }
            }
        }
    }

    /// The dirty chunks as they are now. The entries are taken under the lock and their bytes
    /// copied after it is released, so writers wait for neither.
    fn take_dirty(&self) -> Taken {
        let held: Vec<(ChunkKey, u64, Held)> = {
            let chunks = lock(&self.chunks);
            chunks
                .dirty
                .iter()
                .filter_map(|key| {
                    chunks
                        .owned
                        .get(key)
                        .filter(|entry| entry.dirty())
                        .map(|entry| (*key, entry.version, entry.held.clone()))
                })
                .collect()
        };
        let mut taken = Taken::default();
        for (key, version, held) in held {
            taken.versions.push((key, version));
            match held {
                Held::Bytes(bytes) => {
                    taken.changed.insert(key, bytes.to_vec());
                }
                Held::Deleted => {
                    taken.deleted.insert(key);
                }
            }
        }
        taken
    }

    /// After a successful commit: marks clean exactly the entries it carried. An entry replaced
    /// since the snapshot (the chunk was written or deleted again) is not the one committed and
    /// stays dirty. Its unsaved write happened after the snapshot began, so its dirty time moves
    /// up to `prepared` rather than keeping the time of bytes that are now committed.
    fn mark_committed(&self, taken: &[(ChunkKey, u64)], prepared: Instant) {
        let chunks = &mut *lock(&self.chunks);
        for (key, version) in taken {
            let Some(entry) = chunks.owned.get_mut(key) else {
                continue;
            };
            if entry.version == *version {
                entry.version = 0;
                entry.dirty_since = None;
                chunks.dirty.remove(key);
            } else if entry.dirty_since.is_some_and(|since| since < prepared) {
                entry.dirty_since = Some(prepared);
            }
        }
    }

    /// Drops clean world-owned chunks, least recently used first, until at most the resident limit
    /// remain. Runs after a successful commit, under the commit lock.
    fn evict_clean_beyond_limit(&self) {
        if self.resident_limit == 0 {
            return;
        }
        let mut clean: Vec<(u64, ChunkKey)> = {
            let chunks = lock(&self.chunks);
            if chunks.owned.len() <= self.resident_limit {
                return;
            }
            chunks
                .owned
                .iter()
                .filter(|(_, entry)| !entry.dirty())
                .map(|(key, entry)| (entry.accessed, *key))
                .collect()
        };
        // Sorted outside the lock: writers do not wait for it.
        clean.sort_unstable();
        let mut chunks = lock(&self.chunks);
        for (_, key) in clean {
            if chunks.owned.len() <= self.resident_limit {
                break;
            }
            // Only an entry that is still clean. A write since the list was made dirtied it, and a
            // dirty entry must stay. Nothing else can have changed it: commits are the only other
            // writers, and this one holds the commit lock.
            if chunks.owned.get(&key).is_some_and(|entry| !entry.dirty()) {
                chunks.owned.remove(&key);
                self.evicted.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Clean chunks dropped from memory by the resident limit.
    #[must_use]
    pub fn evicted(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }

    /// Milliseconds since the oldest unsaved chunk became dirty, 0 when none is: how much work a
    /// crash now would lose, in time. A chunk rewritten while a commit carried its previous bytes
    /// counts from that commit's snapshot. Walks the dirty entries only.
    #[must_use]
    pub fn oldest_dirty_millis(&self) -> u64 {
        oldest_dirty_millis(&lock(&self.chunks))
    }

    /// A point-in-time copy of the world's metrics.
    #[must_use]
    pub fn metrics(&self) -> MetricsSnapshot {
        let (save_queue_depth, oldest_pending_save_millis) = {
            let pending = lock(&self.pending_saves);
            let oldest = pending
                .queue
                .front()
                .map_or(0, |(_, queued)| queued.elapsed().as_millis() as u64);
            (pending.queue.len() as u64, oldest)
        };
        let (resident_chunks, dirty_chunks, oldest_dirty_millis) = {
            let chunks = lock(&self.chunks);
            (
                chunks.owned.len() as u64,
                chunks.dirty.len() as u64,
                oldest_dirty_millis(&chunks),
            )
        };
        MetricsSnapshot {
            resident_chunks,
            dirty_chunks,
            save_queue_depth,
            oldest_pending_save_millis,
            oldest_dirty_millis,
            serialization: self.metrics.serialization.percentiles(),
            backend: self.metrics.backend.percentiles(),
            retries: self.metrics.retries.load(Ordering::Relaxed),
            failures: self.metrics.failures.load(Ordering::Relaxed),
            bytes_read: self.metrics.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.metrics.bytes_written.load(Ordering::Relaxed),
            materialized: self.metrics.materialized.load(Ordering::Relaxed),
        }
    }
}

fn require_attempts(max_attempts: usize) -> Result<()> {
    if max_attempts < 1 {
        return Err(AwfError::InvalidArgument(
            "maxAttempts must be at least 1".to_string(),
        ));
    }
    Ok(())
}

fn oldest_dirty_millis(chunks: &Chunks) -> u64 {
    chunks
        .dirty
        .iter()
        .filter_map(|key| chunks.owned.get(key).and_then(|entry| entry.dirty_since))
        .min()
        .map_or(0, |since| since.elapsed().as_millis() as u64)
}

#[cfg(test)]
mod tests;
