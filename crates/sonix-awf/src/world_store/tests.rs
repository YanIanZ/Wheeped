//! Ported from `SourbyCraft`'s `AwfWorldStoreTest`, `AwfDeltaGenerationTest`, `AwfRollbackTest`
//! and the store-level cases of `AwfDurabilityTest` and `AwfCompactionLifecycleTest`.

use std::fs;
use std::io;

use super::*;
use crate::durability::testing::{Call, Recorder};
use crate::generation::Stage;
use crate::hash::sha256_hex;

/// A chunk's expected state: its bytes, or `None` for a recorded deletion.
type Expected = BTreeMap<ChunkKey, Option<Vec<u8>>>;

const fn k(x: i32, z: i32) -> ChunkKey {
    ChunkKey::new(x, z)
}

fn b(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

fn chunks<const N: usize>(entries: [(ChunkKey, &str); N]) -> BTreeMap<ChunkKey, Vec<u8>> {
    entries
        .into_iter()
        .map(|(key, value)| (key, b(value)))
        .collect()
}

fn keys<const N: usize>(keys: [ChunkKey; N]) -> BTreeSet<ChunkKey> {
    BTreeSet::from(keys)
}

fn open(dir: &Path, role: WorldRole, retained: usize) -> WorldStore {
    WorldStore::open(dir, role, retained).unwrap()
}

fn open_policy(dir: &Path, role: WorldRole, retained: usize, compaction: Compaction) -> WorldStore {
    WorldStore::open_with(dir, role, retained, None, Arc::new(DISK), compaction).unwrap()
}

fn open_deltas(dir: &Path, role: WorldRole, retained: usize) -> WorldStore {
    open_policy(dir, role, retained, Compaction::DELTAS)
}

fn commit(
    store: &WorldStore,
    changed: &BTreeMap<ChunkKey, Vec<u8>>,
    removed: &BTreeSet<ChunkKey>,
    deleted: &BTreeSet<ChunkKey>,
    mode: PersistenceMode,
) -> CommitResult {
    store.commit(changed, removed, deleted, mode).unwrap()
}

fn incremental(store: &WorldStore, changed: &BTreeMap<ChunkKey, Vec<u8>>) -> CommitResult {
    commit(
        store,
        changed,
        &keys([]),
        &keys([]),
        PersistenceMode::Incremental,
    )
}

fn objects(dir: &Path) -> ObjectStore {
    ObjectStore::open(&dir.join("objects"), Arc::new(DISK), WorldRole::Vanilla).unwrap()
}

fn object_path(dir: &Path, raw: &str) -> PathBuf {
    let name = ObjectName::of(raw.as_bytes()).to_hex();
    dir.join("objects").join(&name[..2]).join(name)
}

fn object_exists(dir: &Path, raw: &str) -> bool {
    object_path(dir, raw).is_file()
}

fn object_files(dir: &Path) -> usize {
    fs::read_dir(dir.join("objects"))
        .unwrap()
        .map(|shard| fs::read_dir(shard.unwrap().path()).unwrap().count())
        .sum()
}

fn generation_dir(dir: &Path, number: u64) -> PathBuf {
    dir.join("generations-root/generations")
        .join(number.to_string())
}

fn generations_on_disk(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir.join("generations-root/generations"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn numbers<const N: usize>(numbers: [u64; N]) -> BTreeSet<String> {
    numbers.iter().map(u64::to_string).collect()
}

fn manifest(dir: &Path, number: u64) -> String {
    fs::read_to_string(generation_dir(dir, number).join("manifest")).unwrap()
}

fn lines(file: &Path) -> Vec<String> {
    fs::read_to_string(file)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn object_line(key: ChunkKey, raw: &str) -> String {
    format!("{} {} {}", key.x, key.z, ObjectName::of(raw.as_bytes()))
}

/// Every chunk, deletion and absence in `expected` matches the store.
fn assert_state(expected: &Expected, store: &WorldStore, place: &str) {
    let mut live = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    for (key, value) in expected {
        match value {
            None => {
                deleted.insert(*key);
                assert!(store.has(*key), "{place}: {key} is a recorded deletion");
                assert_eq!(
                    store.read(*key).unwrap(),
                    None,
                    "{place}: {key} reads as absent"
                );
            }
            Some(bytes) => {
                live.insert(*key);
                assert_eq!(
                    store.read(*key).unwrap().as_ref(),
                    Some(bytes),
                    "{place}: {key}"
                );
            }
        }
    }
    assert_eq!(store.keys(), live, "{place}");
    assert_eq!(store.deleted(), deleted, "{place}");
}

fn expect(chunks: &BTreeMap<ChunkKey, Vec<u8>>) -> Expected {
    chunks
        .iter()
        .map(|(key, bytes)| (*key, Some(bytes.clone())))
        .collect()
}

fn crash_hook(crash_at: Stage, armed: Arc<std::sync::atomic::AtomicBool>) -> Arc<StageHook> {
    Arc::new(move |stage| {
        if stage == crash_at && armed.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(io::Error::other("injected").into());
        }
        Ok(())
    })
}

#[test]
fn incremental_writes_only_new_objects_but_commits_the_whole_world() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 2);
    let first = incremental(&store, &chunks([(k(0, 0), "a"), (k(1, 0), "b")]));
    assert_eq!(first.objects_written, 2);
    let second = incremental(&store, &chunks([(k(1, 0), "b2")]));
    assert_eq!(second.objects_written, 1);
    assert_eq!(second.chunks, 2);

    let reopened = open(temp.path(), WorldRole::Vanilla, 2);
    assert_eq!(reopened.read(k(0, 0)).unwrap().unwrap(), b"a");
    assert_eq!(reopened.read(k(1, 0)).unwrap().unwrap(), b"b2");
}

#[test]
fn the_index_on_disk_is_the_format_the_java_store_writes() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 2);
    commit(
        &store,
        &chunks([(k(3, -1), "a"), (k(-2, 0), "b"), (k(1, 0), "c")]),
        &keys([]),
        &keys([k(0, 5)]),
        PersistenceMode::Incremental,
    );
    let index = fs::read_to_string(generation_dir(temp.path(), 1).join("index")).unwrap();
    // One "x z value" line per chunk, ordered by z then x, a deletion as "-".
    assert_eq!(
        index,
        format!(
            "{}\n{}\n{}\n0 5 -\n",
            object_line(k(3, -1), "a"),
            object_line(k(-2, 0), "b"),
            object_line(k(1, 0), "c")
        )
    );
    assert_eq!(
        manifest(temp.path(), 1),
        format!("index\t{}\t{}\n", index.len(), sha256_hex(index.as_bytes()))
    );
}

#[test]
fn identical_chunks_share_one_object() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    let result = incremental(&store, &chunks([(k(0, 0), "same"), (k(5, 5), "same")]));
    assert_eq!(result.objects_written, 1);
    assert_eq!(object_files(temp.path()), 1);
}

#[test]
fn full_rewrites_every_chunk() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    incremental(&store, &chunks([(k(0, 0), "a"), (k(1, 1), "b")]));
    let full = commit(
        &store,
        &chunks([(k(0, 0), "a2")]),
        &keys([]),
        &keys([]),
        PersistenceMode::Full,
    );
    assert_eq!(full.objects_written, 2);
}

#[test]
fn objects_only_an_old_generation_used_are_collected_once_it_is_no_longer_retained() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 2);
    incremental(&store, &chunks([(k(0, 0), "v1")]));
    incremental(&store, &chunks([(k(0, 0), "v2")]));
    assert_eq!(
        object_files(temp.path()),
        2,
        "v1 is still referenced by the retained generation"
    );
    assert!(object_exists(temp.path(), "v1"));

    let third = incremental(&store, &chunks([(k(0, 0), "v3")]));
    assert_eq!(third.objects_removed, 1);
    assert_eq!(object_files(temp.path()), 2);
    assert!(!object_exists(temp.path(), "v1"));
    assert!(object_exists(temp.path(), "v2"));
}

#[test]
fn checkpoint_verifies_every_referenced_object_before_committing() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    incremental(&store, &chunks([(k(0, 0), "keep"), (k(1, 1), "rot")]));
    let before = store.generation().unwrap();
    // Damage the object behind (1,1).
    fs::write(object_path(temp.path(), "rot"), [0, 0, 0, 3, 1, 2, 3]).unwrap();

    // Incremental does not look at an unchanged object...
    assert_eq!(
        incremental(&store, &chunks([(k(2, 2), "new")])).generation,
        before + 1
    );
    // ...checkpoint does, and refuses to commit a generation that references it.
    let refused = store.commit(
        &chunks([(k(3, 3), "x")]),
        &keys([]),
        &keys([]),
        PersistenceMode::Checkpoint,
    );
    assert!(matches!(refused, Err(AwfError::Corrupt(_))));
    assert_eq!(store.generation().unwrap(), before + 1);
    assert!(!store.has(k(3, 3)));
}

#[test]
fn removed_chunks_leave_the_world() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    incremental(&store, &chunks([(k(0, 0), "a"), (k(1, 1), "b")]));
    commit(
        &store,
        &chunks([]),
        &keys([k(1, 1)]),
        &keys([]),
        PersistenceMode::Incremental,
    );
    assert_eq!(
        open(temp.path(), WorldRole::Vanilla, 1).keys(),
        keys([k(0, 0)])
    );
}

#[test]
fn read_only_and_template_worlds_refuse_commits() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    let nothing = chunks([]);
    assert!(matches!(
        store.commit(&nothing, &keys([]), &keys([]), PersistenceMode::ReadOnly),
        Err(AwfError::InvalidState(_))
    ));
    let template = open(&temp.path().join("t"), WorldRole::Template, 1);
    assert!(matches!(
        template.commit(&nothing, &keys([]), &keys([]), PersistenceMode::Full),
        Err(AwfError::InvalidState(_))
    ));
    assert!(
        !temp.path().join("t").exists(),
        "a template open creates nothing"
    );
}

#[test]
fn unreferenced_objects_from_a_crashed_commit_do_not_survive_the_next_one() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    incremental(&store, &chunks([(k(0, 0), "a")]));
    // An object written by a commit that never reached its generation swap.
    let stray = objects(temp.path());
    stray.put(b"orphan", false).unwrap();
    stray.sync().unwrap();
    assert_eq!(object_files(temp.path()), 2);

    let reopened = open(temp.path(), WorldRole::Vanilla, 1);
    assert_eq!(
        reopened.keys(),
        keys([k(0, 0)]),
        "the orphan is not part of the world"
    );
    incremental(&reopened, &chunks([(k(1, 1), "b")]));
    assert!(!object_exists(temp.path(), "orphan"));
}

#[test]
fn a_deletion_is_recorded_as_a_tombstone_that_survives_reopening() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    incremental(&store, &chunks([(k(0, 0), "a"), (k(1, 0), "b")]));
    commit(
        &store,
        &chunks([]),
        &keys([]),
        &keys([k(0, 0), k(5, 5)]),
        PersistenceMode::Checkpoint,
    );

    let reopened = open(temp.path(), WorldRole::Vanilla, 1);
    assert!(reopened.has(k(0, 0)));
    assert!(
        reopened.has(k(5, 5)),
        "a chunk only the base had can be deleted too"
    );
    assert_eq!(reopened.read(k(0, 0)).unwrap(), None);
    assert!(!reopened.stores_bytes(k(0, 0)));
    assert!(reopened.stores_bytes(k(1, 0)));
    assert_eq!(reopened.keys(), keys([k(1, 0)]));
    assert_eq!(reopened.deleted(), keys([k(0, 0), k(5, 5)]));
    assert!(
        !object_exists(temp.path(), "a"),
        "a deleted chunk's bytes are collected once no retained generation needs them"
    );
}

#[test]
fn a_full_commit_skips_tombstones() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    commit(
        &store,
        &chunks([(k(0, 0), "a")]),
        &keys([]),
        &keys([k(1, 1)]),
        PersistenceMode::Incremental,
    );
    let full = commit(
        &store,
        &chunks([]),
        &keys([]),
        &keys([]),
        PersistenceMode::Full,
    );
    assert_eq!(full.chunks, 2);
    assert_eq!(full.objects_written, 1);
    assert_eq!(store.read(k(1, 1)).unwrap(), None);
}

#[test]
fn a_shared_object_survives_while_any_retained_generation_uses_it() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    incremental(&store, &chunks([(k(0, 0), "same"), (k(1, 0), "x")]));
    incremental(&store, &chunks([(k(0, 0), "other")]));
    incremental(&store, &chunks([(k(2, 0), "same")]));
    assert!(
        object_exists(temp.path(), "same"),
        "chunk 2 now references it"
    );
    assert!(object_exists(temp.path(), "x"));
}

#[test]
fn objects_of_a_failed_commit_are_collected_by_the_next_one() {
    let temp = tempfile::tempdir().unwrap();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let store = WorldStore::open_with(
        temp.path(),
        WorldRole::Vanilla,
        1,
        Some(crash_hook(Stage::Verified, armed.clone())),
        Arc::new(DISK),
        Compaction::DELTAS,
    )
    .unwrap();
    assert!(
        store
            .commit(
                &chunks([(k(0, 0), "lost")]),
                &keys([]),
                &keys([]),
                PersistenceMode::Incremental
            )
            .is_err()
    );
    assert!(
        object_exists(temp.path(), "lost"),
        "written before the failure"
    );

    armed.store(false, std::sync::atomic::Ordering::Relaxed);
    incremental(&store, &chunks([(k(1, 0), "kept")]));
    assert!(
        !object_exists(temp.path(), "lost"),
        "unreferenced, so collected"
    );
    assert_eq!(
        store.read(k(0, 0)).unwrap(),
        None,
        "the failed commit never became visible"
    );
}

#[test]
fn reopening_scans_for_objects_a_crash_left_behind() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    incremental(&store, &chunks([(k(0, 0), "a")]));
    let stray = objects(temp.path());
    stray.put(b"crash-leftover", false).unwrap();
    stray.sync().unwrap();

    open(temp.path(), WorldRole::Vanilla, 1);
    assert!(!object_exists(temp.path(), "crash-leftover"));
    assert!(object_exists(temp.path(), "a"));
}

#[test]
fn a_read_only_open_never_deletes() {
    let temp = tempfile::tempdir().unwrap();
    incremental(
        &open(temp.path(), WorldRole::Vanilla, 1),
        &chunks([(k(0, 0), "a")]),
    );
    let stray = objects(temp.path());
    stray.put(b"stray", false).unwrap();
    stray.sync().unwrap();

    open(temp.path(), WorldRole::ReadOnly, 1);
    assert!(object_exists(temp.path(), "stray"));
}

#[test]
fn a_closed_store_refuses_commits_and_still_reads() {
    let temp = tempfile::tempdir().unwrap();
    let store = open(temp.path(), WorldRole::Vanilla, 1);
    incremental(&store, &chunks([(k(0, 0), "a")]));
    store.close().unwrap();
    assert!(matches!(
        store.commit(
            &chunks([(k(1, 0), "b")]),
            &keys([]),
            &keys([]),
            PersistenceMode::Incremental
        ),
        Err(AwfError::InvalidState(_))
    ));
    assert_eq!(store.read(k(0, 0)).unwrap().unwrap(), b"a");
}

#[test]
fn a_delta_commit_writes_only_the_changed_entries_and_reopens_to_the_full_state() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let store = open_deltas(dir, WorldRole::Vanilla, 1);
    let world: BTreeMap<ChunkKey, Vec<u8>> = (0..40)
        .map(|i| (k(i, 0), b(&format!("chunk-{i}"))))
        .collect();
    incremental(&store, &world);
    assert!(
        store.last_commit_compacted(),
        "the first commit is a full index"
    );
    assert_eq!(store.chain_length(), 0);

    // Three incremental commits: a few changes, a removal, a deletion, an unchanged rewrite.
    let mut expected = expect(&world);
    for round in 1..=3 {
        let changed_text = format!("changed-{round}");
        let new_text = format!("new-{round}");
        let changed = chunks([
            (k(round, 0), changed_text.as_str()),
            (k(100 + round, 0), new_text.as_str()),
            (k(39, 0), "chunk-39"), // identical bytes: not a change
        ]);
        let removed = keys([k(10 + round, 0)]);
        let deleted = keys([k(20 + round, 0)]);
        let result = commit(
            &store,
            &changed,
            &removed,
            &deleted,
            PersistenceMode::Incremental,
        );
        expected.extend(expect(&changed));
        expected.remove(&k(10 + round, 0));
        expected.insert(k(20 + round, 0), None);

        assert!(!store.last_commit_compacted(), "round {round} is a delta");
        assert_eq!(store.chain_length(), round as usize);
        assert_eq!(result.chunks, expected.len());

        let number = round as u64 + 1;
        assert!(
            generation_dir(dir, number).join("delta").is_file(),
            "a delta blob"
        );
        assert!(
            !generation_dir(dir, number).join("index").exists(),
            "no full index"
        );
        let manifest = manifest(dir, number);
        assert!(
            manifest.starts_with(&format!("@format\t2\n@base\t{}\t", number - 1)),
            "{manifest}"
        );
        let delta = lines(&generation_dir(dir, number).join("delta"));
        assert_eq!(
            delta.len(),
            4,
            "two changes, one drop, one tombstone; the identical rewrite is absent: {delta:?}"
        );
        assert!(
            delta.contains(&format!("{} 0 drop", 10 + round)),
            "{delta:?}"
        );
        assert!(delta.contains(&format!("{} 0 -", 20 + round)), "{delta:?}");
        assert!(
            delta.contains(&object_line(k(round, 0), &changed_text)),
            "{delta:?}"
        );
    }
    assert_state(&expected, &store, "in memory");
    assert_state(
        &expected,
        &open(dir, WorldRole::ReadOnly, 1),
        "reopened read-only",
    );
    let writable = open_deltas(dir, WorldRole::Vanilla, 1);
    assert_state(
        &expected,
        &writable,
        "reopened writable (after the open scan)",
    );
    assert_eq!(
        writable.chain_length(),
        3,
        "the chain is what was committed"
    );
    assert_eq!(writable.generation().unwrap(), 4);
    // Retention keeps one generation, and every generation it is chained to.
    assert_eq!(generations_on_disk(dir), numbers([1, 2, 3, 4]));
}

#[test]
fn tombstones_and_drops_replay_through_the_chain_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let store = open_deltas(dir, WorldRole::Vanilla, 1);
    // Filler chunks keep the world larger than the deltas, so the size criterion does not compact.
    let mut initial = chunks([(k(0, 0), "a"), (k(1, 0), "b"), (k(2, 0), "c")]);
    initial.extend((0..20).map(|i| (k(i, 7), b(&format!("filler-{i}")))));
    incremental(&store, &initial);
    let nothing = chunks([]);
    // Generation 2: delete (0,0) (a tombstone that shadows the base), drop (1,0).
    commit(
        &store,
        &nothing,
        &keys([k(1, 0)]),
        &keys([k(0, 0)]),
        PersistenceMode::Incremental,
    );
    // Generation 3: a deleted chunk written again, a dropped chunk deleted.
    commit(
        &store,
        &chunks([(k(0, 0), "a-again")]),
        &keys([]),
        &keys([k(1, 0)]),
        PersistenceMode::Incremental,
    );
    // Generation 4: drop the tombstone itself; delete (2,0).
    commit(
        &store,
        &nothing,
        &keys([k(1, 0)]),
        &keys([k(2, 0)]),
        PersistenceMode::Incremental,
    );
    assert_eq!(store.chain_length(), 3);

    let mut expected = expect(&initial);
    expected.remove(&k(1, 0));
    expected.insert(k(0, 0), Some(b("a-again")));
    expected.insert(k(2, 0), None);
    assert_state(&expected, &store, "in memory");
    let reopened = open_deltas(dir, WorldRole::Vanilla, 1);
    assert_state(&expected, &reopened, "reopened");
    assert!(
        !reopened.has(k(1, 0)),
        "dropped: the index says nothing about it"
    );
    // The deleted chunk's bytes are gone once no retained generation references them.
    assert!(!object_exists(dir, "c"));
    assert!(!object_exists(dir, "a"));
    assert!(object_exists(dir, "a-again"));

    // The tombstone survives a compaction into the full index; the drop simply is not there.
    commit(
        &reopened,
        &nothing,
        &keys([]),
        &keys([]),
        PersistenceMode::Full,
    );
    assert!(reopened.last_commit_compacted());
    let index = lines(&generation_dir(dir, 5).join("index"));
    assert!(
        index.contains(&object_line(k(0, 0), "a-again")),
        "{index:?}"
    );
    assert!(index.contains(&"2 0 -".to_string()), "{index:?}");
    assert!(
        !index.iter().any(|line| line.starts_with("1 0 ")),
        "{index:?}"
    );
    assert_eq!(index.len(), initial.len() - 1, "{index:?}");
    assert_state(
        &expected,
        &open(dir, WorldRole::ReadOnly, 1),
        "after compaction",
    );
}

#[test]
fn a_compaction_ends_the_chain_and_retention_drops_the_old_one() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let two_hops = Compaction {
        delta_generations: true,
        max_chain_length: 2,
    };
    let store = open_policy(dir, WorldRole::Vanilla, 2, two_hops);
    let base: BTreeMap<ChunkKey, Vec<u8>> = (0..20)
        .map(|i| (k(i, 0), b(&format!("base-{i}"))))
        .collect();
    incremental(&store, &base); // 1: full
    let mut expected = expect(&base);
    for round in 2..=7 {
        let text = format!("round-{round}");
        let changed = chunks([(k(round, 0), text.as_str())]);
        incremental(&store, &changed);
        expected.extend(expect(&changed));
        // 2: delta(1), 3: delta(2), 4: full (two hops reached), 5: delta(4), 6: delta(5), 7: full.
        let full = round == 4 || round == 7;
        assert_eq!(store.last_commit_compacted(), full, "generation {round}");
        let hops = match round {
            4 | 7 => 0,
            2 | 5 => 1,
            _ => 2,
        };
        assert_eq!(store.chain_length(), hops, "generation {round}");
        if full {
            assert!(
                !manifest(dir, round as u64).contains("@base"),
                "a full index is chained to nothing"
            );
            assert!(generation_dir(dir, round as u64).join("index").is_file());
        }
        assert_state(
            &expected,
            &open(dir, WorldRole::ReadOnly, 2),
            &format!("reopened at {round}"),
        );
    }
    // Retention keeps 7 and 6; 6 is chained to 5 and 5 to 4, so the old chain 1..3 is gone.
    assert_eq!(generations_on_disk(dir), numbers([4, 5, 6, 7]));
    // One more delta on 7: 6 falls out of retention and takes 5 and 4 with it.
    let last = chunks([(k(0, 0), "last")]);
    incremental(&store, &last);
    expected.extend(expect(&last));
    assert_eq!(generations_on_disk(dir), numbers([7, 8]));
    assert_state(
        &expected,
        &open_deltas(dir, WorldRole::Vanilla, 2),
        "after the old chain is pruned",
    );
}

#[test]
fn a_commit_whose_replay_would_reach_the_full_index_size_compacts() {
    let temp = tempfile::tempdir().unwrap();
    let store = open_deltas(&temp.path().join("w"), WorldRole::Vanilla, 1);
    let world: BTreeMap<ChunkKey, Vec<u8>> =
        (0..10).map(|i| (k(i, 0), b(&format!("w{i}")))).collect();
    incremental(&store, &world);
    // 4 + 4 = 8 entries replayed < 10: deltas. 4 + 4 + 4 = 12 >= 10: a full index.
    for round in 1..=3 {
        let changed: BTreeMap<ChunkKey, Vec<u8>> = (0..4)
            .map(|i| (k(i, 0), b(&format!("r{round}-{i}"))))
            .collect();
        incremental(&store, &changed);
        assert_eq!(store.last_commit_compacted(), round == 3, "round {round}");
    }
    // An empty world compacts every time (nothing to replay is cheaper as a full index).
    let empty = open_deltas(&temp.path().join("empty"), WorldRole::Vanilla, 1);
    incremental(&empty, &chunks([]));
    incremental(&empty, &chunks([]));
    assert!(empty.last_commit_compacted());
    // Checkpoint mode chains like incremental (it differs in what it verifies, not in the index).
    let checkpoint = open_deltas(&temp.path().join("cp"), WorldRole::Vanilla, 1);
    commit(
        &checkpoint,
        &world,
        &keys([]),
        &keys([]),
        PersistenceMode::Checkpoint,
    );
    commit(
        &checkpoint,
        &chunks([(k(0, 0), "x")]),
        &keys([]),
        &keys([]),
        PersistenceMode::Checkpoint,
    );
    assert!(!checkpoint.last_commit_compacted());
    assert_eq!(checkpoint.chain_length(), 1);
}

#[test]
fn garbage_collection_keeps_every_object_a_replayed_retained_generation_references() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let store = open_deltas(dir, WorldRole::Vanilla, 1);
    let mut initial = chunks([(k(0, 0), "object-a"), (k(1, 0), "object-b")]);
    initial.extend((0..20).map(|i| (k(i, 7), b(&format!("filler-{i}")))));
    incremental(&store, &initial); // 1: full {a, b, filler}
    incremental(&store, &chunks([(k(2, 0), "object-c")])); // 2: delta {c}; a, b inherited
    assert_eq!(store.chain_length(), 1);
    assert_eq!(generations_on_disk(dir), numbers([1, 2]));

    // A writable reopen scans the object directory: the inherited objects are live through the chain.
    let store = open_deltas(dir, WorldRole::Vanilla, 1);
    for raw in ["object-a", "object-b", "object-c"] {
        assert!(
            object_exists(dir, raw),
            "nothing inherited was collected: {raw}"
        );
    }
    assert_eq!(store.read(k(0, 0)).unwrap().unwrap(), b"object-a");
    assert_eq!(store.read(k(1, 0)).unwrap().unwrap(), b"object-b");

    // 3: delta replacing c. Generation 2 leaves retention; c is referenced by no replayed retained state.
    let third = incremental(&store, &chunks([(k(2, 0), "object-c2")]));
    assert_eq!(third.objects_removed, 1);
    assert!(!object_exists(dir, "object-c"));
    for raw in ["object-a", "object-b", "object-c2"] {
        assert!(object_exists(dir, raw), "{raw}");
    }
    assert_eq!(
        generations_on_disk(dir),
        numbers([1, 2, 3]),
        "2 left retention but stays on disk as 3's base, 1 as 2's"
    );

    // 4: drop a. Its entry in generation 1's full index is shadowed by the drop, so a is garbage
    // even though the ancestor's blob still names it; replay never dereferences a shadowed entry.
    let fourth = commit(
        &store,
        &chunks([]),
        &keys([k(0, 0)]),
        &keys([]),
        PersistenceMode::Incremental,
    );
    assert_eq!(fourth.objects_removed, 1);
    assert!(!object_exists(dir, "object-a"));
    let ancestor = fs::read_to_string(generation_dir(dir, 1).join("index")).unwrap();
    assert!(
        ancestor.contains(&ObjectName::of(b"object-a").to_hex()),
        "the ancestor still names it"
    );

    let reopened = open_deltas(dir, WorldRole::Vanilla, 1);
    let mut live: BTreeSet<ChunkKey> = initial.keys().copied().collect();
    live.remove(&k(0, 0));
    live.insert(k(2, 0));
    assert_eq!(reopened.keys(), live);
    assert_eq!(reopened.read(k(1, 0)).unwrap().unwrap(), b"object-b");
    assert_eq!(reopened.read(k(2, 0)).unwrap().unwrap(), b"object-c2");
    assert!(
        object_exists(dir, "object-b") && object_exists(dir, "object-c2"),
        "the open scan keeps what the chain references"
    );
    // Checkpoint verifies every object the replayed index references, through the chain.
    commit(
        &reopened,
        &chunks([(k(5, 0), "e")]),
        &keys([]),
        &keys([]),
        PersistenceMode::Checkpoint,
    );
    assert_eq!(
        open(dir, WorldRole::ReadOnly, 1)
            .read(k(1, 0))
            .unwrap()
            .unwrap(),
        b"object-b"
    );
}

#[test]
fn a_chain_whose_link_or_delta_is_damaged_is_reported_not_replayed() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let store = open_deltas(dir, WorldRole::Vanilla, 1);
    incremental(&store, &chunks([(k(0, 0), "a")]));
    incremental(&store, &chunks([(k(1, 0), "b")]));
    incremental(&store, &chunks([(k(2, 0), "c")]));
    assert_eq!(
        store.chain_length(),
        2,
        "the fixture is a chain: 3 on 2 on 1"
    );
    let refused = || {
        matches!(
            WorldStore::open(dir, WorldRole::ReadOnly, 1),
            Err(AwfError::CorruptGeneration(_))
        )
    };

    // The base's manifest is replaced by a well-formed one that is not the one generation 3 chained to.
    let base_manifest = generation_dir(dir, 2).join("manifest");
    let original = fs::read(&base_manifest).unwrap();
    let mut altered = original.clone();
    altered.push(b'\n');
    fs::write(&base_manifest, altered).unwrap();
    assert!(refused());
    fs::write(&base_manifest, original).unwrap();
    open(dir, WorldRole::ReadOnly, 1);

    // A delta blob of an ancestor that no longer matches its manifest.
    let delta = generation_dir(dir, 2).join("delta");
    let delta_bytes = fs::read(&delta).unwrap();
    fs::write(&delta, "1 0 drop\n").unwrap();
    assert!(refused());
    fs::write(&delta, delta_bytes).unwrap();

    // An ancestor that is gone.
    let moved = dir.join("gone");
    fs::rename(generation_dir(dir, 1), &moved).unwrap();
    assert!(refused());
    fs::rename(&moved, generation_dir(dir, 1)).unwrap();
    assert_eq!(
        open(dir, WorldRole::ReadOnly, 1)
            .read(k(0, 0))
            .unwrap()
            .unwrap(),
        b"a"
    );
}

#[test]
fn an_index_that_names_anything_but_an_object_is_corrupt() {
    for (line, is_delta) in [
        ("0 0 ../../../outside", false),
        ("0 0 drop", false),
        ("0 0", false),
        ("0 0 - extra", false),
        ("x 0 -", false),
        ("0 2147483648 -", false),
        ("0 0 ABCDEF", true),
    ] {
        assert!(
            matches!(
                decode_entries(format!("{line}\n").as_bytes(), is_delta),
                Err(AwfError::CorruptGeneration(_))
            ),
            "{line}"
        );
    }
    let name = ObjectName::of(b"a");
    let delta = decode_entries(
        format!("-3 +4 {name}\n\n1 1 drop\n2 2 - \n").as_bytes(),
        true,
    )
    .unwrap();
    assert_eq!(
        delta,
        BTreeMap::from([
            (k(-3, 4), Change::Set(Entry::Object(name))),
            (k(1, 1), Change::Drop),
            (k(2, 2), Change::Set(Entry::Deleted)),
        ])
    );
}

#[test]
fn with_deltas_off_an_open_compacts_a_chained_store_and_a_crash_there_is_recoverable() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    // A chain written with deltas on.
    let on = open_deltas(dir, WorldRole::Vanilla, 2);
    let world: BTreeMap<ChunkKey, Vec<u8>> =
        (0..20).map(|i| (k(i, 0), b(&format!("w{i}")))).collect();
    incremental(&on, &world);
    incremental(&on, &chunks([(k(0, 0), "changed")]));

    // A crash in the middle of the compaction at open: the open fails, the delta generation stays
    // current and intact, and the next open completes the compaction.
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let crashed = WorldStore::open_with(
        dir,
        WorldRole::Vanilla,
        2,
        Some(crash_hook(Stage::Verified, armed)),
        Arc::new(DISK),
        Compaction::FULL_ONLY,
    );
    assert!(crashed.is_err());
    let read_only = open(dir, WorldRole::ReadOnly, 2);
    assert_eq!(read_only.generation().unwrap(), 2);
    assert_eq!(read_only.chain_length(), 1);
    assert!(!read_only.full_index());
    assert_eq!(read_only.read(k(0, 0)).unwrap().unwrap(), b"changed");

    let off = open(dir, WorldRole::Vanilla, 2);
    assert_eq!(off.generation().unwrap(), 3);
    assert!(off.full_index());
    assert_eq!(off.read(k(0, 0)).unwrap().unwrap(), b"changed");
    assert!(
        !manifest(dir, 3).contains('@'),
        "format 1, readable by every build"
    );
    assert_eq!(lines(&generation_dir(dir, 3).join("index")).len(), 20);
}

const A: ChunkKey = k(0, 0);
const B: ChunkKey = k(1, 0);
const C: ChunkKey = k(2, 0);

/// Generation 1: a, b. 2: b rewritten, c deleted. 3: a removed, c written.
fn history(store: WorldStore) -> WorldStore {
    incremental(&store, &chunks([(A, "a1"), (B, "b1")]));
    commit(
        &store,
        &chunks([(B, "b2")]),
        &keys([]),
        &keys([C]),
        PersistenceMode::Incremental,
    );
    commit(
        &store,
        &chunks([(C, "c3")]),
        &keys([A]),
        &keys([]),
        PersistenceMode::Incremental,
    );
    store
}

#[test]
fn a_rollback_commits_the_old_state_as_a_new_generation_and_survives_a_reopen() {
    for compaction in [Compaction::FULL_ONLY, Compaction::DELTAS] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let store = history(open_policy(root, WorldRole::Vanilla, 4, compaction));
        assert_eq!(store.rollback_points().unwrap(), [3, 2, 1]);

        let rolled = store.rollback_to(1).unwrap().unwrap();
        assert_eq!(
            rolled.generation, 4,
            "history is not rewritten: the rollback is generation 4"
        );
        assert_eq!(
            rolled.objects_written, 0,
            "the old state's objects are still retained"
        );
        assert!(
            store.full_index(),
            "a rollback writes a full index, whatever the delta setting"
        );
        assert_eq!(store.read(A).unwrap().unwrap(), b"a1");
        assert_eq!(store.read(B).unwrap().unwrap(), b"b1");
        assert!(!store.has(C));
        assert_eq!(store.rollback_points().unwrap(), [4, 3, 2, 1]);

        let reopened = open_policy(root, WorldRole::Vanilla, 4, compaction);
        assert_eq!(reopened.generation().unwrap(), 4);
        assert_eq!(reopened.keys(), keys([A, B]));
        assert_eq!(reopened.read(B).unwrap().unwrap(), b"b1");

        // A rollback can itself be undone while the later generation is retained.
        reopened.rollback_to(3).unwrap().unwrap();
        assert_eq!(reopened.keys(), keys([B, C]));
        assert_eq!(reopened.read(B).unwrap().unwrap(), b"b2");
        assert_eq!(reopened.read(C).unwrap().unwrap(), b"c3");
        assert!(
            open_policy(root, WorldRole::Vanilla, 4, compaction)
                .deleted()
                .is_empty()
        );
    }
}

#[test]
fn a_rollback_to_a_deletion_keeps_the_tombstone() {
    let temp = tempfile::tempdir().unwrap();
    let store = history(open_deltas(temp.path(), WorldRole::Vanilla, 3));
    store.rollback_to(2).unwrap().unwrap();
    assert_eq!(
        store.deleted(),
        keys([C]),
        "the deletion shadows the base again"
    );
    assert_eq!(store.keys(), keys([A, B]));
}

#[test]
fn only_retained_generations_are_rollback_points_and_a_refusal_changes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let store = history(open(temp.path(), WorldRole::Vanilla, 2));
    assert_eq!(
        store.rollback_points().unwrap(),
        [3, 2],
        "generation 1 fell out of retention"
    );
    assert!(
        matches!(store.rollback_to(1), Err(AwfError::InvalidArgument(message)) if message.contains("[3, 2]"))
    );
    assert!(matches!(
        store.rollback_to(9),
        Err(AwfError::InvalidArgument(_))
    ));
    assert_eq!(
        store.rollback_to(3).unwrap(),
        None,
        "rolling back to the current generation commits nothing"
    );
    assert_eq!(store.generation().unwrap(), 3);
    assert_eq!(
        open(temp.path(), WorldRole::Vanilla, 2)
            .generation()
            .unwrap(),
        3
    );
}

#[test]
fn a_missing_object_fails_the_rollback_before_its_commit_point() {
    let temp = tempfile::tempdir().unwrap();
    let store = history(open(temp.path(), WorldRole::Vanilla, 3));
    fs::remove_file(object_path(temp.path(), "a1")).unwrap();
    assert!(matches!(store.rollback_to(1), Err(AwfError::Corrupt(_))));
    assert_eq!(store.generation().unwrap(), 3, "nothing was committed");
    assert_eq!(store.keys(), keys([B, C]));
    assert_eq!(
        open(temp.path(), WorldRole::Vanilla, 3)
            .generation()
            .unwrap(),
        3
    );
}

#[test]
fn read_only_and_closed_stores_refuse_a_rollback() {
    let temp = tempfile::tempdir().unwrap();
    history(open(temp.path(), WorldRole::Vanilla, 3));
    let read_only = open(temp.path(), WorldRole::ReadOnly, 3);
    assert_eq!(
        read_only.rollback_points().unwrap(),
        [3, 2, 1],
        "listing is a read"
    );
    assert!(matches!(
        read_only.rollback_to(1),
        Err(AwfError::InvalidState(_))
    ));

    let closed = open(temp.path(), WorldRole::Vanilla, 3);
    closed.close().unwrap();
    assert!(matches!(
        closed.rollback_to(1),
        Err(AwfError::InvalidState(_))
    ));
    assert_eq!(
        open(temp.path(), WorldRole::Vanilla, 3)
            .generation()
            .unwrap(),
        3
    );
}

#[test]
fn later_commits_collect_only_what_no_retained_generation_references() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let store = history(open_deltas(dir, WorldRole::Vanilla, 2));
    store.rollback_to(2).unwrap().unwrap(); // state: b2, c deleted
    incremental(&store, &chunks([(A, "a5")]));
    // Retained now: 5 (a5, b2) and 4 (b2). c3 (only in 3) is gone; b2 is kept.
    assert!(
        !object_exists(dir, "c3"),
        "the rolled-back generation's own object is collected"
    );
    assert!(object_exists(dir, "b2"));
    assert!(object_exists(dir, "a5"));

    let reopened = open_deltas(dir, WorldRole::Vanilla, 2);
    assert_eq!(reopened.read(B).unwrap().unwrap(), b"b2");
    assert_eq!(reopened.read(A).unwrap().unwrap(), b"a5");
}

/// A store over a recorder rooted at the store directory.
fn recorded(dir: &Path, compaction: Compaction) -> (WorldStore, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::new(dir));
    let store = WorldStore::open_with(
        dir,
        WorldRole::Vanilla,
        1,
        None,
        recorder.clone(),
        compaction,
    )
    .unwrap();
    (store, recorder)
}

#[test]
fn every_object_and_its_directory_is_forced_before_the_index() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap().join("store");
    let (store, recorder) = recorded(&dir, Compaction::FULL_ONLY);
    recorder.clear();
    incremental(&store, &chunks([(k(0, 0), "a"), (k(1, 0), "b")]));

    let calls = recorder.calls();
    let position = |wanted: &dyn Fn(&Call) -> bool| calls.iter().position(wanted).unwrap();
    let last = |wanted: &dyn Fn(&Call) -> bool| calls.iter().rposition(wanted).unwrap();
    let in_objects = |path: &Path| path.starts_with("objects");
    let last_object_call = last(&|call| match call {
        Call::File(path) | Call::Directory(path) => in_objects(path),
        Call::Move(_, to) => in_objects(to),
    });
    let first_generation_call = position(&|call| match call {
        Call::File(path) | Call::Directory(path) => {
            path.starts_with("generations-root/generations")
        }
        Call::Move(..) => false,
    });
    assert!(
        last_object_call < first_generation_call,
        "objects are durable and named before anything that references them is forced: {calls:?}"
    );
    assert_eq!(
        calls.last(),
        Some(&Call::Directory(PathBuf::from("generations-root"))),
        "the pointer's directory is the last force"
    );
    // One force per object, one per touched shard, one for the object root.
    let object_forces = calls
        .iter()
        .filter(|call| matches!(call, Call::File(path) if in_objects(path)))
        .count();
    assert_eq!(object_forces, 2);
}

#[test]
fn a_failure_at_every_durability_step_leaves_the_previous_generation_intact() {
    for compaction in [Compaction::FULL_ONLY, Compaction::DELTAS] {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap().join("store");
        let (store, recorder) = recorded(&dir, compaction);
        let stable: BTreeMap<ChunkKey, Vec<u8>> = (0..8)
            .map(|i| (k(i, 0), b(&format!("stable-{i}"))))
            .collect();
        incremental(&store, &stable);

        // How many durability calls one more commit makes.
        recorder.clear();
        incremental(&store, &chunks([(k(0, 0), "probe")]));
        let steps = recorder.calls().len();
        assert!(steps > 6, "{steps}");
        let mut expected = expect(&stable);
        expected.insert(k(0, 0), Some(b("probe")));

        for fail_at in 0..steps {
            let seen = std::sync::atomic::AtomicUsize::new(0);
            recorder.fail_when(move |_| {
                (seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == fail_at)
                    .then(|| io::Error::other("power loss"))
            });
            let generation_before = store.generation().unwrap();
            let attempt = store.commit(
                &chunks([(k(0, 0), "lost"), (k(50, 0), "lost-too")]),
                &keys([k(1, 0)]),
                &keys([k(2, 0)]),
                PersistenceMode::Incremental,
            );
            recorder.stop_failing();
            let committed = store.generation().unwrap() > generation_before;
            if committed {
                // Only the forces after the pointer rename can fail a commit that happened.
                assert!(attempt.is_err());
                let reopened = open_policy(&dir, WorldRole::ReadOnly, 1, compaction);
                assert_eq!(
                    reopened.read(k(0, 0)).unwrap().unwrap(),
                    b"lost",
                    "step {fail_at}"
                );
                break;
            }
            assert!(attempt.is_err(), "step {fail_at}");
            // What a restart sees: the previous generation, whole.
            let reopened = open_policy(&dir, WorldRole::ReadOnly, 1, compaction);
            assert_state(
                &expected,
                &reopened,
                &format!("after a failure at step {fail_at}"),
            );
        }
    }
}

#[test]
fn a_filesystem_that_refuses_an_atomic_rename_fails_the_commit_instead_of_copying() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap().join("store");
    let (store, recorder) = recorded(&dir, Compaction::FULL_ONLY);
    recorder.fail_when(|call| {
        matches!(call, Call::Move(..)).then(|| io::Error::from(io::ErrorKind::CrossesDevices))
    });
    let refused = store.commit(
        &chunks([(k(0, 0), "chunk")]),
        &keys([]),
        &keys([]),
        PersistenceMode::Incremental,
    );
    assert!(
        matches!(&refused, Err(AwfError::IoContext { context, .. }) if context.contains("atomic rename") && context.contains("refused")),
        "{refused:?}"
    );
    assert_eq!(store.generation().unwrap(), 0);
    assert!(store.keys().is_empty());
    assert!(!object_exists(&dir, "chunk"), "not copied into place");

    recorder.stop_failing();
    incremental(&store, &chunks([(k(0, 0), "chunk")]));
    assert_eq!(store.read(k(0, 0)).unwrap().unwrap(), b"chunk");
}
