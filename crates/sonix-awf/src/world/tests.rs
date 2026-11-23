//! Ported from `SourbyCraft`'s `AwfWorldTest` and `AwfCommitOrderTest`, and from the cases of
//! `AwfConcurrentFlushTest`, `AwfChunkUnloadRetentionTest`, `AwfLifecycleFenceTest`,
//! `AwfShutdownDeadlineTest` and `AwfDirtyBytesCommitTest` that are about the world rather than
//! the storage around it. The Java tests reach some of those through the region storage; here they
//! call the world. The last section holds cases the Java tests do not have.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Condvar, Weak};
use std::thread;
use std::time::Duration;

use super::*;
use crate::durability::DISK;
use crate::image::Image;
use crate::source::LayeredSource;
use crate::world_store::{Compaction, WorldStore};

use PersistenceMode::{Full, Incremental};
use WorldRole::{Instance, ReadOnly, Temporary, Vanilla};

/// Long enough for a thread that is making progress, on the slowest machine these tests run on.
const PATIENCE: Duration = Duration::from_secs(60);
/// Long enough for a call that does not wait to have finished.
const A_MOMENT: Duration = Duration::from_millis(500);

const fn k(x: i32, z: i32) -> ChunkKey {
    ChunkKey::new(x, z)
}

fn b(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

fn open(dir: &Path, role: WorldRole, retained: usize) -> Arc<WorldStore> {
    Arc::new(WorldStore::open(dir, role, retained).unwrap())
}

/// The store as a world takes it.
fn shared<S: Store + Send + Sync + 'static>(store: &Arc<S>) -> SharedStore {
    store.clone()
}

/// A template image with a floor at 0,0 and a wall at 1,0.
fn template(dir: &Path) -> Arc<Image> {
    let file = dir.join("template.awf");
    Image::write(
        &file,
        &BTreeMap::from([("name".to_string(), "arena".to_string())]),
        &BTreeMap::from([(k(0, 0), b("floor")), (k(1, 0), b("wall"))]),
    )
    .unwrap();
    Arc::new(Image::open(&file).unwrap())
}

/// The template as a world's base.
fn over(template: &Arc<Image>) -> WorldBase {
    WorldBase::Source(template.clone())
}

fn new_world(
    name: &str,
    role: WorldRole,
    base: Option<WorldBase>,
    store: Option<SharedStore>,
) -> Arc<World> {
    Arc::new(World::new(name, role, base, store).unwrap())
}

/// A vanilla world on a FILE store that keeps at most `limit` clean chunks in memory.
fn limited(dir: &Path, limit: usize) -> Arc<World> {
    let store = Some(shared(&open(dir, Vanilla, 1)));
    Arc::new(World::with_resident_limit("limited", Vanilla, None, store, limit).unwrap())
}

/// Queues a save and runs it on the calling thread, as a storage lane would.
fn save(world: &Arc<World>, mode: PersistenceMode) -> Option<CommitResult> {
    world.save(mode, 1).unwrap().run().unwrap()
}

/// A weak handle on the bytes the world holds for a chunk, to see later whether anything still
/// holds them.
fn weak_bytes(world: &World, key: ChunkKey) -> Weak<[u8]> {
    let chunks = lock(&world.chunks);
    let Held::Bytes(bytes) = &chunks.owned.get(&key).unwrap().held else {
        panic!("{key} is a deletion");
    };
    Arc::downgrade(bytes)
}

/// Something a test thread waits for.
#[derive(Default)]
struct Latch {
    state: Mutex<LatchState>,
    changed: Condvar,
}

#[derive(Default)]
struct LatchState {
    open: bool,
}

impl Latch {
    fn open(&self) {
        lock(&self.state).open = true;
        self.changed.notify_all();
    }

    /// Whether the latch opened within `timeout`.
    fn wait(&self, timeout: Duration) -> bool {
        let state = lock(&self.state);
        self.changed
            .wait_timeout_while(state, timeout, |state| !state.open)
            .unwrap()
            .0
            .open
    }
}

/// A store in memory that counts the commits it is asked for.
#[derive(Default)]
struct MemoryStore {
    state: Mutex<MemoryState>,
    commits: AtomicUsize,
}

#[derive(Default)]
struct MemoryState {
    chunks: BTreeMap<ChunkKey, Vec<u8>>,
    tombstones: BTreeSet<ChunkKey>,
    generation: u64,
}

impl ChunkSource for MemoryStore {
    fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>> {
        Ok(lock(&self.state).chunks.get(&key).cloned())
    }

    fn keys(&self) -> BTreeSet<ChunkKey> {
        lock(&self.state).chunks.keys().copied().collect()
    }
}

impl Store for MemoryStore {
    fn has(&self, key: ChunkKey) -> bool {
        let state = lock(&self.state);
        state.chunks.contains_key(&key) || state.tombstones.contains(&key)
    }

    fn deleted(&self) -> BTreeSet<ChunkKey> {
        lock(&self.state).tombstones.clone()
    }

    fn generation(&self) -> Result<u64> {
        Ok(lock(&self.state).generation)
    }

    fn commit(
        &self,
        changed: &BTreeMap<ChunkKey, Vec<u8>>,
        _removed: &BTreeSet<ChunkKey>,
        deleted: &BTreeSet<ChunkKey>,
        _mode: PersistenceMode,
    ) -> Result<CommitResult> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        let mut state = lock(&self.state);
        let mut bytes_written = 0;
        for (key, bytes) in changed {
            state.tombstones.remove(key);
            state.chunks.insert(*key, bytes.clone());
            bytes_written += bytes.len() as u64;
        }
        for key in deleted {
            state.chunks.remove(key);
            state.tombstones.insert(*key);
        }
        state.generation += 1;
        Ok(CommitResult {
            generation: state.generation,
            chunks: changed.len() + deleted.len(),
            objects_written: changed.len(),
            bytes_written,
            objects_removed: deleted.len(),
        })
    }
}

/// A FILE store that holds its first commit inside the store until released, and records whether
/// it was closed while a commit ran.
struct GatedStore {
    real: Arc<WorldStore>,
    entered: Latch,
    release: Latch,
    calls: AtomicUsize,
    committing: AtomicUsize,
    closed: AtomicBool,
    closed_during_commit: AtomicBool,
}

impl GatedStore {
    fn over(real: &Arc<WorldStore>) -> Arc<Self> {
        Arc::new(Self {
            real: real.clone(),
            entered: Latch::default(),
            release: Latch::default(),
            calls: AtomicUsize::new(0),
            committing: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            closed_during_commit: AtomicBool::new(false),
        })
    }
}

impl ChunkSource for GatedStore {
    fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>> {
        self.real.read(key)
    }

    fn keys(&self) -> BTreeSet<ChunkKey> {
        self.real.keys()
    }
}

impl Store for GatedStore {
    fn has(&self, key: ChunkKey) -> bool {
        self.real.has(key)
    }

    fn deleted(&self) -> BTreeSet<ChunkKey> {
        self.real.deleted()
    }

    fn generation(&self) -> Result<u64> {
        self.real.generation()
    }

    fn commit(
        &self,
        changed: &BTreeMap<ChunkKey, Vec<u8>>,
        removed: &BTreeSet<ChunkKey>,
        deleted: &BTreeSet<ChunkKey>,
        mode: PersistenceMode,
    ) -> Result<CommitResult> {
        self.committing.fetch_add(1, Ordering::SeqCst);
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.open();
            self.release.wait(PATIENCE);
        }
        let committed = self.real.commit(changed, removed, deleted, mode);
        self.committing.fetch_sub(1, Ordering::SeqCst);
        committed
    }

    fn close(&self) -> Result<()> {
        if self.committing.load(Ordering::SeqCst) > 0 {
            self.closed_during_commit.store(true, Ordering::SeqCst);
        }
        self.closed.store(true, Ordering::SeqCst);
        self.real.close()
    }
}

/// A store with nothing in it whose every commit fails the way `failure` says.
struct FailingStore {
    commits: AtomicUsize,
    failure: Box<dyn Fn() -> AwfError + Send + Sync>,
}

impl FailingStore {
    fn with(failure: impl Fn() -> AwfError + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            commits: AtomicUsize::new(0),
            failure: Box::new(failure),
        })
    }
}

impl ChunkSource for FailingStore {
    fn read(&self, _key: ChunkKey) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn keys(&self) -> BTreeSet<ChunkKey> {
        BTreeSet::new()
    }
}

impl Store for FailingStore {
    fn has(&self, _key: ChunkKey) -> bool {
        false
    }

    fn deleted(&self) -> BTreeSet<ChunkKey> {
        BTreeSet::new()
    }

    fn generation(&self) -> Result<u64> {
        Ok(0)
    }

    fn commit(
        &self,
        _changed: &BTreeMap<ChunkKey, Vec<u8>>,
        _removed: &BTreeSet<ChunkKey>,
        _deleted: &BTreeSet<ChunkKey>,
        _mode: PersistenceMode,
    ) -> Result<CommitResult> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        Err((self.failure)())
    }
}

// AwfWorldTest

#[test]
fn instances_share_a_template_and_stay_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let template = template(temp.path());
    let a = new_world(
        "a",
        Instance,
        Some(over(&template)),
        Some(shared(&open(&temp.path().join("a"), Instance, 1))),
    );
    let other = new_world(
        "b",
        Instance,
        Some(over(&template)),
        Some(shared(&open(&temp.path().join("b"), Instance, 1))),
    );
    a.write(k(0, 0), b"floor-broken").unwrap();
    assert!(a.owns(k(0, 0)));
    assert!(!other.owns(k(0, 0)));
    assert_eq!(a.read(k(0, 0)).unwrap().unwrap(), b"floor-broken");
    assert_eq!(other.read(k(0, 0)).unwrap().unwrap(), b"floor");
    assert_eq!(
        template.read(k(0, 0)).unwrap().unwrap(),
        b"floor",
        "the template is untouched"
    );
}

#[test]
fn the_callers_buffer_and_the_returned_bytes_are_never_shared() {
    let world = new_world("w", Temporary, None, None);
    let mut buffer = b("abc");
    world.write(k(0, 0), &buffer).unwrap();
    buffer[0] = b'X';
    let mut read = world.read(k(0, 0)).unwrap().unwrap();
    read[1] = b'Y';
    assert_eq!(world.read(k(0, 0)).unwrap().unwrap(), b"abc");
    assert_eq!(buffer, b"Xbc");
    assert_eq!(read, b"aYc");
}

#[test]
fn chunks_materialise_only_when_read() {
    let temp = tempfile::tempdir().unwrap();
    let template = template(temp.path());
    let world = new_world("v", ReadOnly, Some(over(&template)), None);
    assert_eq!(world.keys().len(), 2);
    assert_eq!(world.metrics().materialized, 0);
    world.read(k(1, 0)).unwrap();
    assert_eq!(world.metrics().materialized, 1);
    assert!(matches!(
        world.write(k(1, 0), b"x"),
        Err(AwfError::InvalidState(_))
    ));
}

#[test]
fn an_instance_survives_a_restart_over_its_template() {
    let temp = tempfile::tempdir().unwrap();
    let template = template(temp.path());
    let dir = temp.path().join("i");
    let world = new_world(
        "i",
        Instance,
        Some(over(&template)),
        Some(shared(&open(&dir, Instance, 1))),
    );
    world.write(k(1, 0), b"door").unwrap();
    let queued = world.save(Incremental, 1).unwrap();
    // The storage lane is another thread.
    let committed = thread::spawn(move || queued.run()).join().unwrap();
    assert!(committed.unwrap().is_some());
    assert_eq!(world.metrics().dirty_chunks, 0);

    let store = open(&dir, Instance, 1);
    let base = WorldBase::Source(Arc::new(LayeredSource {
        upper: store.clone(),
        lower: template,
    }));
    let restarted = new_world("i", Instance, Some(base), Some(shared(&store)));
    assert_eq!(restarted.read(k(1, 0)).unwrap().unwrap(), b"door");
    assert_eq!(restarted.read(k(0, 0)).unwrap().unwrap(), b"floor");
}

/// `AwfWorldTest.aWriteDuringASaveStaysDirty`. The write there lands between queueing the save and
/// running it, so the save carries it; the case of a write while the commit is in the store is
/// `a_chunk_rewritten_while_its_commit_is_in_the_store_stays_dirty` below.
#[test]
fn a_queued_save_takes_the_chunks_as_they_are_when_it_runs() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("w");
    let world = new_world("w", Vanilla, None, Some(shared(&open(&dir, Vanilla, 1))));
    world.write(k(0, 0), b"v1").unwrap();
    let pending = world.save(Incremental, 1).unwrap();
    assert_eq!(
        world.metrics().save_queue_depth,
        1,
        "queued, and the caller was not blocked"
    );
    world.write(k(0, 0), b"v2").unwrap();
    // The save snapshots when it runs: v2 is written by it and the entry is cleared.
    pending.run().unwrap();
    assert_eq!(world.metrics().dirty_chunks, 0);
    assert_eq!(world.metrics().save_queue_depth, 0);
    assert_eq!(
        open(&dir, Vanilla, 1).read(k(0, 0)).unwrap().unwrap(),
        b"v2"
    );
}

#[test]
fn a_failing_backend_is_retried_then_reported_and_the_chunks_stay_dirty() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("f");
    let world = new_world("f", Vanilla, None, Some(shared(&open(&root, Vanilla, 1))));
    world.write(k(0, 0), b"data").unwrap();
    // Make the object directory unwritable by replacing it with a file.
    fs::remove_dir_all(root.join("objects")).unwrap();
    fs::write(root.join("objects"), "not a directory").unwrap();
    let failed = world.save(Incremental, 3).unwrap().run();
    assert!(failed.is_err_and(|failure| failure.is_io()));
    let metrics = world.metrics();
    assert_eq!(metrics.retries, 2);
    assert_eq!(metrics.failures, 1);
    assert_eq!(metrics.dirty_chunks, 1);
}

#[test]
fn worlds_that_do_not_persist_refuse_to_save() {
    let temporary = new_world("t", Temporary, None, None);
    assert!(matches!(
        temporary.save(Full, 1),
        Err(AwfError::InvalidState(_))
    ));
    assert!(matches!(
        temporary.save_now(Full, 1),
        Err(AwfError::InvalidState(_))
    ));
    assert!(matches!(
        World::new("v", Vanilla, None, None),
        Err(AwfError::InvalidArgument(_))
    ));
}

#[test]
fn clean_chunks_beyond_the_limit_are_dropped_least_recently_used_first_and_read_back_from_the_store()
 {
    let temp = tempfile::tempdir().unwrap();
    let world = limited(temp.path(), 2);
    for x in 0..4 {
        world.write(k(x, 0), format!("c{x}").as_bytes()).unwrap();
    }
    world.read(k(0, 0)).unwrap(); // touch 0: now most recently used
    save(&world, Incremental);

    assert_eq!(world.evicted(), 2);
    assert_eq!(world.metrics().resident_chunks, 2);
    assert!(world.resident(k(0, 0)), "recently read, kept");
    assert!(world.resident(k(3, 0)), "most recently written, kept");
    assert!(!world.resident(k(1, 0)));
    assert!(
        world.owns(k(1, 0)),
        "dropped from memory, still this world's chunk"
    );
    assert_eq!(
        world.read(k(1, 0)).unwrap().unwrap(),
        b"c1",
        "read back from the store"
    );
}

#[test]
fn dirty_chunks_are_never_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let world = limited(temp.path(), 1);
    world.write(k(0, 0), b"a").unwrap();
    world.write(k(1, 0), b"b").unwrap();
    let pending = world.save(Incremental, 1).unwrap();
    pending.run().unwrap();
    // Written after the save: eviction must not drop anything a later write dirtied.
    world.write(k(2, 0), b"c").unwrap();
    world.write(k(3, 0), b"d").unwrap();
    assert_eq!(
        world.metrics().resident_chunks,
        3,
        "1 clean kept by the limit + 2 dirty"
    );
    assert!(world.resident(k(2, 0)));
    assert!(world.resident(k(3, 0)));
}

#[test]
fn pending_saves_are_bounded_and_the_overflow_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let world = new_world(
        "s",
        Vanilla,
        None,
        Some(shared(&open(temp.path(), Vanilla, 1))),
    );
    world.write(k(0, 0), b"x").unwrap();
    let stalled: Vec<QueuedSave> = (0..MAX_PENDING_SAVES)
        .map(|_| world.save(Incremental, 1).unwrap())
        .collect();
    assert!(matches!(
        world.save(Incremental, 1),
        Err(AwfError::Rejected(_))
    ));
    assert_eq!(world.metrics().save_queue_depth, MAX_PENDING_SAVES as u64);
    assert_eq!(world.metrics().failures, 1, "the refusal is counted");
    for queued in stalled {
        queued.run().unwrap();
    }
    assert_eq!(
        world.metrics().save_queue_depth,
        0,
        "draining frees the slots"
    );
    assert_eq!(world.metrics().failures, 1);
}

#[test]
fn a_deletion_shadows_the_base_and_is_committed() {
    let temp = tempfile::tempdir().unwrap();
    let template = template(temp.path());
    let dir = temp.path().join("deleting");
    let world = new_world(
        "w",
        Instance,
        Some(over(&template)),
        Some(shared(&open(&dir, Instance, 1))),
    );
    world.delete(k(0, 0)).unwrap();
    assert_eq!(
        world.read(k(0, 0)).unwrap(),
        None,
        "not the template's floor"
    );
    assert!(world.owns(k(0, 0)));
    assert!(!world.keys().contains(&k(0, 0)));
    world.save_now(Incremental, 1).unwrap();

    let reopened = new_world(
        "w",
        Instance,
        Some(over(&template)),
        Some(shared(&open(&dir, Instance, 1))),
    );
    assert_eq!(
        reopened.read(k(0, 0)).unwrap(),
        None,
        "the deletion survived the restart"
    );
    assert_eq!(reopened.keys(), BTreeSet::from([k(1, 0)]));
    reopened.write(k(0, 0), b"rebuilt").unwrap();
    assert_eq!(reopened.read(k(0, 0)).unwrap().unwrap(), b"rebuilt");
}

#[test]
fn a_read_only_world_cannot_delete() {
    let world = new_world("r", ReadOnly, None, None);
    assert!(matches!(
        world.delete(k(0, 0)),
        Err(AwfError::InvalidState(_))
    ));
}

// AwfCommitOrderTest

/// Two commits of one world must reach the store in the order they took their snapshots. A
/// periodic commit that snapshotted a chunk, then lost the race for the store to a flush that
/// snapshotted the chunk's newer bytes, would land last and leave the store holding the older
/// bytes while the chunk was no longer dirty.
#[test]
fn a_later_snapshot_is_never_overwritten_by_an_earlier_one() {
    let temp = tempfile::tempdir().unwrap();
    let real = open(temp.path(), Vanilla, 2);
    let gated = GatedStore::over(&real);
    let world = new_world("w", Vanilla, None, Some(gated.clone()));
    let key = k(3, 4);

    world.write(key, b"older").unwrap();
    let periodic = {
        let world = world.clone();
        thread::spawn(move || world.save_now(Incremental, 1))
    };
    assert!(
        gated.entered.wait(PATIENCE),
        "the first commit should reach the store"
    );

    world.write(key, b"newer").unwrap();
    let (flushed, flush_done) = mpsc::channel();
    let flush = {
        let world = world.clone();
        thread::spawn(move || {
            let committed = world.save_now(Incremental, 1);
            flushed.send(()).unwrap();
            committed
        })
    };
    assert!(
        flush_done.recv_timeout(A_MOMENT).is_err(),
        "it finishes here only if commits are not ordered"
    );
    gated.release.open();
    periodic.join().unwrap().unwrap();
    flush.join().unwrap().unwrap();

    assert_eq!(
        real.read(key).unwrap().unwrap(),
        b"newer",
        "the store must hold the bytes of the later snapshot"
    );
    assert_eq!(world.dirty_count(), 0);
}

// AwfConcurrentFlushTest

#[test]
fn an_incremental_commit_with_nothing_dirty_skips_the_store() {
    let store = Arc::new(MemoryStore::default());
    let world = new_world("world/region", Vanilla, None, Some(store.clone()));
    assert_eq!(world.save_now(Incremental, 1).unwrap(), None);
    assert_eq!(
        store.commits.load(Ordering::SeqCst),
        0,
        "an empty incremental snapshot is not a store commit"
    );
    // A FULL commit rewrites the unchanged chunks too, so it still reaches the store with nothing
    // dirty.
    assert!(world.save_now(Full, 1).unwrap().is_some());
    assert_eq!(
        store.commits.load(Ordering::SeqCst),
        1,
        "a FULL commit is the store's to run even with nothing dirty"
    );
}

// AwfChunkUnloadRetentionTest. The Java test asks the garbage collector whether a chunk's array is
// still reachable; here a weak handle on the world's bytes answers it.

#[test]
fn chunks_the_world_dropped_are_released_and_the_ones_it_kept_are_held() {
    let temp = tempfile::tempdir().unwrap();
    let world = limited(temp.path(), 2);
    for x in 0..5 {
        world
            .write(k(x, 0), format!("chunk-{x}").as_bytes())
            .unwrap();
    }
    let handles: Vec<Weak<[u8]>> = (0..5).map(|x| weak_bytes(&world, k(x, 0))).collect();
    save(&world, Incremental);

    assert_eq!(
        world.evicted(),
        3,
        "5 chunks, limit 2: three clean ones dropped"
    );
    let mut released = 0;
    let mut kept = 0;
    for (x, handle) in (0..5).zip(&handles) {
        if world.resident(k(x, 0)) {
            // Control: the world really holds what it did not drop.
            assert!(handle.upgrade().is_some());
            kept += 1;
        } else {
            assert!(
                handle.upgrade().is_none(),
                "the dropped chunk's bytes are held by nothing"
            );
            released += 1;
        }
    }
    assert_eq!(released, 3);
    assert_eq!(kept, 2);
}

#[test]
fn loading_evicted_chunks_back_and_unloading_them_again_never_grows_the_resident_set() {
    let temp = tempfile::tempdir().unwrap();
    let world = limited(temp.path(), 2);
    for x in 0..6 {
        world
            .write(k(x, 0), format!("chunk-{x}").as_bytes())
            .unwrap();
    }
    save(&world, Incremental);
    assert_eq!(world.metrics().resident_chunks, 2);

    for cycle in 0..50 {
        for x in 0..6 {
            assert_eq!(
                world.read(k(x, 0)).unwrap().unwrap(),
                format!("chunk-{x}").as_bytes(),
                "cycle {cycle} chunk {x}"
            );
        }
    }

    assert_eq!(
        world.metrics().resident_chunks,
        2,
        "reading evicted chunks from the store re-pins nothing"
    );
    assert_eq!(world.dirty_count(), 0);
    assert_eq!(world.pending_save_count(), 0);
}

#[test]
fn rewriting_a_chunk_releases_its_previous_bytes_without_an_eviction() {
    let temp = tempfile::tempdir().unwrap();
    let world = limited(temp.path(), 0); // unlimited: nothing evicts
    let key = k(0, 0);
    world.write(key, b"chunk-0").unwrap();
    let first = weak_bytes(&world, key);
    save(&world, Incremental);
    assert!(
        first.upgrade().is_some(),
        "committed and still held in memory"
    );

    world.write(key, b"chunk-0 v2").unwrap();
    save(&world, Incremental);

    assert_eq!(world.evicted(), 0);
    assert!(first.upgrade().is_none());
    assert!(world.resident(key));
    assert_eq!(world.read(key).unwrap().unwrap(), b"chunk-0 v2");
}

// AwfLifecycleFenceTest

#[test]
fn a_fenced_world_refuses_queued_commits_without_touching_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), Vanilla, 2);
    let world = new_world("w", Vanilla, None, Some(shared(&store)));
    world.write(k(0, 0), b"queued").unwrap();
    let queued = world.save(Incremental, 2).unwrap();
    world.fence();

    assert!(matches!(queued.run(), Err(AwfError::Fenced(_))));
    assert_eq!(
        store.generation().unwrap(),
        0,
        "the fenced commit reached nothing"
    );
    assert_eq!(world.dirty_count(), 1, "nothing pretended to be committed");
    assert_eq!(world.pending_save_count(), 0);
    assert!(matches!(
        world.save_now(Incremental, 1),
        Err(AwfError::Fenced(_))
    ));
    assert!(matches!(world.compact(), Err(AwfError::Fenced(_))));
}

#[test]
fn a_running_commit_completes_and_the_store_closes_only_after_it() {
    let temp = tempfile::tempdir().unwrap();
    let real = open(temp.path(), Vanilla, 2);
    let gated = GatedStore::over(&real);
    let world = new_world("w", Vanilla, None, Some(gated.clone()));
    world.write(k(0, 0), b"in flight").unwrap();
    let committer = {
        let world = world.clone();
        thread::spawn(move || world.save_now(Incremental, 1))
    };
    assert!(
        gated.entered.wait(PATIENCE),
        "the commit is inside the store"
    );
    assert!(world.commit_running());

    world.fence(); // discard: never blocks
    let (closed, close_done) = mpsc::channel();
    let closer = {
        let world = world.clone();
        thread::spawn(move || {
            let outcome = world.close_store();
            closed.send(()).unwrap();
            outcome
        })
    };
    assert!(
        close_done.recv_timeout(A_MOMENT).is_err(),
        "a close that does not wait finishes here"
    );
    gated.release.open();
    assert!(
        committer.join().unwrap().unwrap().is_some(),
        "a commit that started before the fence completes"
    );
    closer.join().unwrap().unwrap();

    assert!(gated.closed.load(Ordering::SeqCst));
    assert!(
        !gated.closed_during_commit.load(Ordering::SeqCst),
        "the store was never closed under the running commit"
    );
    assert!(!world.commit_running());
    assert_eq!(real.generation().unwrap(), 1);
    assert!(
        matches!(world.save_now(Incremental, 1), Err(AwfError::Fenced(_))),
        "nothing commits after close"
    );
}

// AwfShutdownDeadlineTest

#[test]
fn a_commit_cut_off_after_it_was_sent_is_uncertain_and_one_never_sent_is_not() {
    for sent in [true, false] {
        let cut = FailingStore::with(move || AwfError::DeadlineExceeded {
            message: "cut by the test".to_string(),
            request_sent: sent,
        });
        let world = new_world("w", Vanilla, None, Some(cut.clone()));
        world.write(k(0, 0), b"x").unwrap();
        world.write(k(0, 1), b"y").unwrap();
        let budget = ShutdownBudget::new(PATIENCE).unwrap();

        let failed = world.save_now_within(Incremental, 3, &budget);
        assert!(matches!(
            failed,
            Err(AwfError::DeadlineExceeded { request_sent, .. }) if request_sent == sent
        ));
        assert_eq!(
            cut.commits.load(Ordering::SeqCst),
            1,
            "one attempt only: a cut commit is not retried"
        );
        assert_eq!(world.uncertain_chunks(), if sent { 2 } else { 0 });
        assert_eq!(world.dirty_count(), 2);
        assert_eq!(world.metrics().retries, 0);
        assert_eq!(world.metrics().failures, 1);
    }
}

#[test]
fn an_expired_budget_starts_no_attempt_and_never_reports_success() {
    let now = Arc::new(AtomicU64::new(0));
    let clock = now.clone();
    let budget = ShutdownBudget::with_clock(Duration::from_millis(100), move || {
        clock.load(Ordering::SeqCst)
    })
    .unwrap();
    let expiries = Arc::new(AtomicUsize::new(0));
    let counted = expiries.clone();
    budget.on_expiry(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    });
    let store = Arc::new(MemoryStore::default());
    let world = new_world("w", Vanilla, None, Some(store.clone()));
    world.write(k(0, 0), b"x").unwrap();

    now.store(100_000_000, Ordering::SeqCst);
    assert!(matches!(
        world.save_now_within(Incremental, 3, &budget),
        Err(AwfError::DeadlineExceeded {
            request_sent: false,
            ..
        })
    ));
    assert!(
        world.close_store_within(&budget).unwrap(),
        "no commit is running, so the store is closed"
    );
    assert_eq!(store.commits.load(Ordering::SeqCst), 0);
    assert_eq!(
        expiries.load(Ordering::SeqCst),
        1,
        "expiry actions run once"
    );
    assert_eq!(world.dirty_count(), 1);
    assert!(world.fenced());
}

// AwfDirtyBytesCommitTest

#[test]
fn dirty_bytes_at_least_counts_owned_dirty_bytes() {
    let world = new_world("w", Vanilla, None, Some(Arc::new(MemoryStore::default())));
    world.write(k(0, 0), &[0; 30]).unwrap();
    world.write(k(1, 0), &[0; 70]).unwrap();
    assert_eq!(world.dirty_count(), 2);
    assert!(world.dirty_bytes_at_least(100));
    assert!(!world.dirty_bytes_at_least(101));
    assert!(world.dirty_bytes_at_least(0), "a zero limit is always met");

    for _ in 0..5 {
        world.write(k(1, 0), &[0; 70]).unwrap();
    }
    assert!(
        !world.dirty_bytes_at_least(101),
        "one dirty chunk of 70 bytes, however often it was rewritten"
    );

    world.delete(k(2, 0)).unwrap();
    assert_eq!(world.dirty_count(), 3);
    assert!(
        !world.dirty_bytes_at_least(101),
        "a deletion is dirty but holds no bytes"
    );
    world.delete(k(1, 0)).unwrap();
    assert!(world.dirty_bytes_at_least(30));
    assert!(!world.dirty_bytes_at_least(31));
}

// Cases the Java tests do not have.

#[test]
fn a_chunk_rewritten_while_its_commit_is_in_the_store_stays_dirty() {
    let temp = tempfile::tempdir().unwrap();
    let real = open(temp.path(), Vanilla, 2);
    let gated = GatedStore::over(&real);
    let world = new_world("w", Vanilla, None, Some(gated.clone()));
    world.write(k(0, 0), b"first").unwrap();
    world.write(k(1, 0), b"untouched").unwrap();
    let committer = {
        let world = world.clone();
        thread::spawn(move || world.save_now(Incremental, 1))
    };
    assert!(
        gated.entered.wait(PATIENCE),
        "the commit is inside the store"
    );

    world.write(k(0, 0), b"second").unwrap();
    gated.release.open();
    committer.join().unwrap().unwrap();

    assert_eq!(
        world.dirty_sample(8),
        [k(0, 0)],
        "the write the commit did not carry"
    );
    assert_eq!(real.read(k(0, 0)).unwrap().unwrap(), b"first");
    assert_eq!(
        world.read(k(0, 0)).unwrap().unwrap(),
        b"second",
        "the world answers with its newer bytes"
    );
    world.save_now(Incremental, 1).unwrap();
    assert_eq!(world.dirty_count(), 0);
    assert_eq!(real.read(k(0, 0)).unwrap().unwrap(), b"second");
}

#[test]
fn a_save_dropped_without_running_frees_its_slot_and_counts_as_failed() {
    let world = new_world("w", Vanilla, None, Some(Arc::new(MemoryStore::default())));
    world.write(k(0, 0), b"x").unwrap();
    let queued = world.save(Incremental, 1).unwrap();
    assert_eq!(world.pending_save_count(), 1);
    drop(queued);
    assert_eq!(world.pending_save_count(), 0);
    assert_eq!(world.metrics().failures, 1);
    assert_eq!(world.dirty_count(), 1, "nothing was committed");
}

#[test]
fn a_save_needs_at_least_one_attempt() {
    let world = new_world("w", Vanilla, None, Some(Arc::new(MemoryStore::default())));
    assert!(matches!(
        world.save(Incremental, 0),
        Err(AwfError::InvalidArgument(_))
    ));
    assert!(matches!(
        world.save_now(Incremental, 0),
        Err(AwfError::InvalidArgument(_))
    ));
    assert_eq!(world.pending_save_count(), 0);
}

#[test]
fn a_refused_commit_is_not_retried() {
    let refusing =
        FailingStore::with(|| AwfError::InvalidState("refused, for the test".to_string()));
    let world = new_world("w", Vanilla, None, Some(refusing.clone()));
    world.write(k(0, 0), b"x").unwrap();
    assert!(matches!(
        world.save_now(Incremental, 5),
        Err(AwfError::InvalidState(_))
    ));
    assert_eq!(refusing.commits.load(Ordering::SeqCst), 1);
    assert_eq!(world.metrics().retries, 0);
    assert_eq!(world.metrics().failures, 1);
}

#[test]
fn an_attempt_that_failed_is_not_tried_again_once_the_deadline_has_passed() {
    let now = Arc::new(AtomicU64::new(0));
    let clock = now.clone();
    let budget = ShutdownBudget::with_clock(Duration::from_millis(100), move || {
        clock.load(Ordering::SeqCst)
    })
    .unwrap();
    // The first attempt fails, and by then the deadline has passed.
    let failing = FailingStore::with(move || {
        now.store(100_000_000, Ordering::SeqCst);
        AwfError::Io(io::Error::other("the disk failed, for the test"))
    });
    let world = new_world("w", Vanilla, None, Some(failing.clone()));
    world.write(k(0, 0), b"x").unwrap();

    assert!(
        matches!(
            world.save_now_within(Incremental, 3, &budget),
            Err(AwfError::DeadlineExceeded {
                request_sent: true,
                ..
            })
        ),
        "the failed attempt may have reached the backend"
    );
    assert_eq!(failing.commits.load(Ordering::SeqCst), 1);
    assert_eq!(world.metrics().retries, 1);
    assert_eq!(world.metrics().failures, 1);
    assert_eq!(world.dirty_count(), 1);
}

#[test]
fn a_commit_still_running_at_the_deadline_is_reported_as_sent_and_the_store_is_not_closed_under_it()
{
    let temp = tempfile::tempdir().unwrap();
    let real = open(temp.path(), Vanilla, 2);
    let gated = GatedStore::over(&real);
    let world = new_world("w", Vanilla, None, Some(gated.clone()));
    world.write(k(0, 0), b"in flight").unwrap();
    let committer = {
        let world = world.clone();
        thread::spawn(move || world.save_now(Incremental, 1))
    };
    assert!(
        gated.entered.wait(PATIENCE),
        "the commit is inside the store"
    );

    let budget = ShutdownBudget::new(Duration::from_millis(50)).unwrap();
    assert!(
        matches!(
            world.save_now_within(Incremental, 1, &budget),
            Err(AwfError::DeadlineExceeded {
                request_sent: true,
                ..
            })
        ),
        "the running commit holds the lock past the deadline"
    );
    assert!(
        !world.close_store_within(&budget).unwrap(),
        "not closed under the commit"
    );
    assert!(world.fenced(), "fenced either way");
    assert!(!gated.closed.load(Ordering::SeqCst));

    gated.release.open();
    assert!(committer.join().unwrap().unwrap().is_some());
    assert_eq!(real.generation().unwrap(), 1);
}

#[test]
fn keys_lay_memory_over_the_store_over_the_base() {
    let temp = tempfile::tempdir().unwrap();
    let template = template(temp.path());
    let world = new_world(
        "w",
        Instance,
        Some(over(&template)),
        Some(shared(&open(&temp.path().join("w"), Instance, 1))),
    );
    assert_eq!(world.keys(), BTreeSet::from([k(0, 0), k(1, 0)]));
    assert_eq!(world.owned_count(), 0, "everything is the template's");

    world.write(k(2, 0), b"roof").unwrap();
    world.delete(k(1, 0)).unwrap();
    assert_eq!(world.keys(), BTreeSet::from([k(0, 0), k(2, 0)]));
    assert_eq!(world.owned_count(), 1, "a deletion is not a chunk it holds");

    world.save_now(Incremental, 1).unwrap();
    world.write(k(1, 0), b"wall again").unwrap();
    world.delete(k(2, 0)).unwrap();
    assert_eq!(
        world.keys(),
        BTreeSet::from([k(0, 0), k(1, 0)]),
        "memory wins over what the store committed"
    );
    assert_eq!(world.owned_count(), 1);
    assert_eq!(world.dirty_sample(1), [k(1, 0)], "lowest first");
    assert_eq!(world.dirty_sample(8), [k(1, 0), k(2, 0)]);
}

#[test]
fn a_store_as_the_base_answers_for_one_chunk_and_says_where_a_read_goes() {
    let temp = tempfile::tempdir().unwrap();
    let stored = k(0, 0);
    let deleted = k(1, 0);
    let absent = k(9, 9);
    let template_dir = temp.path().join("template");
    // Written through a store that may commit, then opened as the template it is.
    open(&template_dir, Vanilla, 1)
        .commit(
            &BTreeMap::from([(stored, b("floor"))]),
            &BTreeSet::new(),
            &BTreeSet::from([deleted]),
            Incremental,
        )
        .unwrap();
    let template_store = open(&template_dir, WorldRole::Template, 1);
    let own = temp.path().join("own");
    let world = Arc::new(
        World::with_resident_limit(
            "w",
            Instance,
            Some(WorldBase::Store(template_store)),
            Some(shared(&open(&own, Instance, 1))),
            1,
        )
        .unwrap(),
    );

    assert!(world.base_has(stored));
    assert!(world.base_has(deleted), "the template says it is deleted");
    assert!(!world.base_has(absent));
    assert!(
        world.read_leaves_memory(stored),
        "the template store has the bytes"
    );
    assert!(!world.read_leaves_memory(deleted));
    assert!(!world.read_leaves_memory(absent));
    assert_eq!(world.read(stored).unwrap().unwrap(), b"floor");
    assert_eq!(world.metrics().materialized, 1);
    assert_eq!(world.metrics().bytes_read, 5);

    world.write(k(2, 0), b"a").unwrap();
    world.write(k(3, 0), b"bb").unwrap();
    world.delete(stored).unwrap();
    assert!(
        !world.read_leaves_memory(k(2, 0)),
        "held in memory, nothing is fetched"
    );
    assert!(!world.read_leaves_memory(stored));
    let committed = save(&world, Incremental).unwrap();
    assert_eq!(committed.objects_written, 2);
    assert_eq!(world.metrics().bytes_written, committed.bytes_written);
    // The limit of 1 keeps only the most recently used entry, which is the deletion.
    assert_eq!(world.evicted(), 2);
    assert!(world.resident(stored));
    assert!(!world.resident(k(2, 0)));
    assert!(
        world.read_leaves_memory(k(2, 0)),
        "committed and dropped: read from the world's own store"
    );
    assert_eq!(world.read(stored).unwrap(), None, "answered from memory");
    assert_eq!(
        world.metrics().bytes_read,
        5,
        "a read from memory is not counted"
    );
    assert_eq!(world.read(k(2, 0)).unwrap().unwrap(), b"a");
    assert_eq!(
        world.metrics().bytes_read,
        6,
        "one byte from the world's own store"
    );
}

#[test]
fn an_image_as_the_base_lists_its_chunks_and_never_counts_as_a_fetch() {
    let temp = tempfile::tempdir().unwrap();
    let template = template(temp.path());
    let world = new_world("w", ReadOnly, Some(over(&template)), None);
    assert!(world.base_has(k(0, 0)));
    assert!(!world.base_has(k(5, 5)));
    assert!(!world.read_leaves_memory(k(0, 0)));
    assert!(!world.owns(k(0, 0)));
    assert!(world.full_index(), "no store, nothing chained");
    assert_eq!(world.store_generation().unwrap(), 0);
    assert!(matches!(world.compact(), Err(AwfError::InvalidState(_))));
}

#[test]
fn compacting_ends_the_stores_delta_chain() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        WorldStore::open_with(
            temp.path(),
            Vanilla,
            2,
            None,
            Arc::new(DISK),
            Compaction::DELTAS,
        )
        .unwrap(),
    );
    let world = new_world("w", Vanilla, None, Some(shared(&store)));
    assert_eq!(world.compact().unwrap(), None, "an empty store");
    for x in 0..4 {
        world.write(k(x, 0), format!("c{x}").as_bytes()).unwrap();
    }
    world.save_now(Incremental, 1).unwrap();
    assert!(world.full_index(), "a first generation is a full index");
    world.write(k(0, 0), b"changed").unwrap();
    world.save_now(Incremental, 1).unwrap();
    assert!(!world.full_index(), "one changed chunk of four is a delta");
    assert_eq!(world.store_generation().unwrap(), 2);

    let compacted = world.compact().unwrap().unwrap();
    assert_eq!(compacted.generation, 3);
    assert!(world.full_index());
    assert_eq!(world.compact().unwrap(), None, "nothing left to compact");
    assert_eq!(world.read(k(0, 0)).unwrap().unwrap(), b"changed");
}

#[test]
fn the_oldest_dirty_time_ends_with_the_commit_that_carries_the_chunk() {
    let world = new_world("w", Vanilla, None, Some(Arc::new(MemoryStore::default())));
    assert_eq!(world.oldest_dirty_millis(), 0, "nothing is dirty");
    world.write(k(0, 0), b"x").unwrap();
    thread::sleep(Duration::from_millis(30));
    world.write(k(0, 0), b"y").unwrap();
    let dirty_for = world.oldest_dirty_millis();
    assert!(
        dirty_for >= 25,
        "a rewrite keeps the time the chunk became dirty: {dirty_for} ms"
    );
    assert!(world.metrics().oldest_dirty_millis >= dirty_for);
    world.save_now(Incremental, 1).unwrap();
    assert_eq!(world.oldest_dirty_millis(), 0);
    assert_eq!(world.metrics().dirty_chunks, 0);
    assert_eq!(world.metrics().backend.samples, 1);
    assert_eq!(world.metrics().serialization.samples, 1);
}
