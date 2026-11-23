//! Holds the port against stores and an image written by `SourbyCraft`'s own Java classes.
//!
//! `fixtures/java-b57/` was written by `java/Fixtures.java` running the Build 57 sources; how, and
//! how to run the opposite direction (Java reading what Rust wrote), is in `fixtures/README.md`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sonix_awf::{
    ChunkKey, ChunkSource, Compaction, DISK, Image, ObjectName, PersistenceMode, Store, WorldRole,
    WorldStore,
};

/// Everything one line of `expected.tsv` can say about a store or an image.
#[derive(Debug, Default, PartialEq, Eq)]
struct Facts {
    generation: Option<u64>,
    chain: Option<usize>,
    rollback: Option<Vec<u64>>,
    /// Chunk to its length and the SHA-256 of its bytes.
    chunks: BTreeMap<ChunkKey, (usize, String)>,
    deleted: BTreeSet<ChunkKey>,
    /// Metadata as the hex of each string's UTF-8 bytes.
    meta: BTreeMap<String, String>,
}

impl Facts {
    fn of_store(store: &WorldStore) -> Self {
        let mut facts = Self {
            generation: Some(store.generation().unwrap()),
            chain: Some(store.chain_length()),
            rollback: Some(store.rollback_points().unwrap()),
            deleted: store.deleted(),
            ..Self::default()
        };
        for key in store.keys() {
            let raw = store.read(key).unwrap().unwrap();
            facts
                .chunks
                .insert(key, (raw.len(), ObjectName::of(&raw).to_hex()));
        }
        facts
    }

    fn of_image(image: &Image) -> Self {
        let mut facts = Self::default();
        for (key, value) in image.metadata() {
            facts.meta.insert(hex(key), hex(value));
        }
        for key in image.keys() {
            let raw = image.read(key).unwrap().unwrap();
            facts
                .chunks
                .insert(key, (raw.len(), ObjectName::of(&raw).to_hex()));
        }
        facts
    }

    /// The lines `Fixtures.java` writes for the same facts.
    fn to_tsv(&self, name: &str) -> String {
        let mut out = String::new();
        if let (Some(generation), Some(chain), Some(rollback)) =
            (self.generation, self.chain, &self.rollback)
        {
            let points: Vec<String> = rollback.iter().map(u64::to_string).collect();
            let _ = writeln!(out, "{name}\tgeneration\t{generation}");
            let _ = writeln!(out, "{name}\tchain\t{chain}");
            let _ = writeln!(out, "{name}\trollback\t{}", points.join(","));
        }
        for (key, value) in &self.meta {
            let _ = writeln!(out, "{name}\tmeta\t{key}\t{value}");
        }
        for (key, (length, sha)) in &self.chunks {
            let _ = writeln!(out, "{name}\tchunk\t{}\t{}\t{length}\t{sha}", key.x, key.z);
        }
        for key in &self.deleted {
            let _ = writeln!(out, "{name}\tdeleted\t{}\t{}", key.x, key.z);
        }
        out
    }
}

fn hex(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for byte in text.bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn expected(file: &Path) -> BTreeMap<String, Facts> {
    let mut all: BTreeMap<String, Facts> = BTreeMap::new();
    for line in fs::read_to_string(file).unwrap().lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        let facts = all.entry(parts[0].to_string()).or_default();
        let key = |x: &str, z: &str| ChunkKey::new(x.parse().unwrap(), z.parse().unwrap());
        match parts[1..] {
            ["generation", number] => facts.generation = Some(number.parse().unwrap()),
            ["chain", hops] => facts.chain = Some(hops.parse().unwrap()),
            ["rollback", points] => {
                facts.rollback = Some(
                    points
                        .split(',')
                        .filter(|point| !point.is_empty())
                        .map(|point| point.parse().unwrap())
                        .collect(),
                );
            }
            ["chunk", x, z, length, sha] => {
                facts
                    .chunks
                    .insert(key(x, z), (length.parse().unwrap(), sha.to_string()));
            }
            ["deleted", x, z] => {
                facts.deleted.insert(key(x, z));
            }
            ["meta", name, value] => {
                facts.meta.insert(name.to_string(), value.to_string());
            }
            _ => panic!("unexpected line: {line}"),
        }
    }
    all
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/java-b57")
}

fn java_expected() -> BTreeMap<String, Facts> {
    expected(&fixtures().join("expected.tsv"))
}

/// A writable copy of a fixture store: opening one for writing recovers, scans and may commit.
fn copy_of(name: &str) -> (tempfile::TempDir, PathBuf) {
    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let copy = temp.path().join(name);
    copy_tree(&fixtures().join(name), &copy);
    (temp, copy)
}

fn open(dir: &Path, role: WorldRole, retained: usize, compaction: Compaction) -> WorldStore {
    WorldStore::open_with(dir, role, retained, None, Arc::new(DISK), compaction).unwrap()
}

fn files_in(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(
                    path.strip_prefix(dir).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    files
}

#[test]
fn a_store_of_full_indexes_written_by_java_reads_the_same() {
    let dir = fixtures().join("java-full");
    let before = files_in(&dir);
    let store = open(&dir, WorldRole::ReadOnly, 3, Compaction::FULL_ONLY);
    assert_eq!(Facts::of_store(&store), java_expected()["java-full"]);
    assert!(store.full_index());
    assert_eq!(
        files_in(&dir),
        before,
        "a read-only open changes nothing on disk"
    );
}

#[test]
fn a_delta_chain_written_by_java_replays_to_the_same_state() {
    let dir = fixtures().join("java-deltas");
    let store = open(&dir, WorldRole::ReadOnly, 2, Compaction::DELTAS);
    assert_eq!(Facts::of_store(&store), java_expected()["java-deltas"]);
    assert!(!store.full_index());
    // What the chain says about the three kinds of entry a delta can carry.
    assert!(store.has(ChunkKey::new(20, 0)), "a tombstone written again");
    assert!(!store.has(ChunkKey::new(21, 0)), "a tombstone dropped");
    assert!(!store.has(ChunkKey::new(10, 0)), "a chunk dropped");
}

#[test]
fn an_image_written_by_java_reads_the_same() {
    let image = Image::open(&fixtures().join("java-image.awf")).unwrap();
    assert_eq!(Facts::of_image(&image), java_expected()["java-image.awf"]);
    assert_eq!(image.metadata()["nul"], "a\u{0}b");
    assert_eq!(image.metadata()["emoji"], "\u{1F600}");
}

#[test]
fn an_image_rewritten_by_rust_has_the_header_java_wrote() {
    let java = fixtures().join("java-image.awf");
    let image = Image::open(&java).unwrap();
    let chunks: BTreeMap<ChunkKey, Vec<u8>> = image
        .keys()
        .into_iter()
        .map(|key| (key, image.read(key).unwrap().unwrap()))
        .collect();
    let temp = tempfile::tempdir().unwrap();
    let rust = temp.path().join("rust-image.awf");
    Image::write(&rust, image.metadata(), &chunks).unwrap();

    // Magic, version and the metadata block are byte for byte the same, in Java's key order. The
    // index and the data differ only where the two zlib implementations compress differently.
    let java_bytes = fs::read(&java).unwrap();
    let rust_bytes = fs::read(&rust).unwrap();
    let metadata_end: usize = 12
        + image
            .metadata()
            .iter()
            .map(|(key, value)| 4 + java_string_length(key) + java_string_length(value))
            .sum::<usize>();
    assert_eq!(rust_bytes[..metadata_end], java_bytes[..metadata_end]);
    assert_eq!(
        Facts::of_image(&Image::open(&rust).unwrap()),
        java_expected()["java-image.awf"]
    );
}

/// The encoded length of a string in Java's modified UTF-8.
fn java_string_length(text: &str) -> usize {
    text.chars()
        .map(|c| match c as u32 {
            1..=0x7F => 1,
            0 | 0x80..=0x7FF => 2,
            0x800..=0xFFFF => 3,
            _ => 6,
        })
        .sum()
}

#[test]
fn rust_commits_on_top_of_a_store_java_wrote() {
    for (name, retained, compaction) in [
        ("java-full", 3, Compaction::FULL_ONLY),
        ("java-deltas", 2, Compaction::DELTAS),
    ] {
        let (_temp, dir) = copy_of(name);
        // The writable open recovers and scans the object directory: nothing Java wrote is lost.
        let store = open(&dir, WorldRole::Vanilla, retained, compaction);
        let mut facts = java_expected().remove(name).unwrap();
        assert_eq!(Facts::of_store(&store), facts, "{name} opened for writing");

        let key = ChunkKey::new(1000, 1000);
        let changed = BTreeMap::from([(key, b"rust-on-top".to_vec())]);
        let result = store
            .commit(
                &changed,
                &BTreeSet::new(),
                &BTreeSet::new(),
                PersistenceMode::Checkpoint,
            )
            .unwrap();
        assert_eq!(
            Some(result.generation),
            facts.generation.map(|generation| generation + 1)
        );
        assert_eq!(
            store.last_commit_compacted(),
            !compaction.delta_generations,
            "{name}: a delta on Java's chain, a full index otherwise"
        );

        let reopened = open(&dir, WorldRole::ReadOnly, retained, compaction);
        facts
            .chunks
            .insert(key, (11, ObjectName::of(b"rust-on-top").to_hex()));
        let after = Facts::of_store(&reopened);
        assert_eq!(after.chunks, facts.chunks, "{name}");
        assert_eq!(after.deleted, facts.deleted, "{name}");
    }
}

#[test]
fn with_deltas_off_rust_compacts_a_chain_java_wrote() {
    let (_temp, dir) = copy_of("java-deltas");
    let store = open(&dir, WorldRole::Vanilla, 2, Compaction::FULL_ONLY);
    let expected = java_expected().remove("java-deltas").unwrap();
    let facts = Facts::of_store(&store);
    assert_eq!(
        facts.generation,
        Some(5),
        "the compaction is one more generation"
    );
    assert_eq!(facts.chain, Some(0));
    assert_eq!(facts.chunks, expected.chunks);
    assert_eq!(facts.deleted, expected.deleted);
    assert!(store.full_index());
}

#[test]
fn a_rollback_on_a_store_java_wrote_restores_its_first_generation() {
    let (_temp, dir) = copy_of("java-full");
    let store = open(&dir, WorldRole::Vanilla, 3, Compaction::FULL_ONLY);
    store.rollback_to(1).unwrap().unwrap();
    // Generation 1 as Fixtures.java committed it.
    assert_eq!(store.read(ChunkKey::new(0, 0)).unwrap().unwrap(), b"a");
    assert_eq!(
        store.read(ChunkKey::new(5, 5)).unwrap().unwrap(),
        b"to-delete"
    );
    assert_eq!(
        store.read(ChunkKey::new(6, 6)).unwrap().unwrap(),
        b"to-remove"
    );
    assert!(store.deleted().is_empty());
    assert!(!store.has(ChunkKey::new(8, 8)));
}

/// Bytes that do not compress to nothing; the same generator as `Fixtures.pattern`.
fn pattern(seed: i32, length: usize) -> Vec<u8> {
    let mut state = seed;
    (0..length)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            ((state as u32) >> 16) as u8
        })
        .collect()
}

fn keys<const N: usize>(keys: [(i32, i32); N]) -> BTreeSet<ChunkKey> {
    keys.into_iter().map(|(x, z)| ChunkKey::new(x, z)).collect()
}

fn chunks<const N: usize>(entries: [((i32, i32), Vec<u8>); N]) -> BTreeMap<ChunkKey, Vec<u8>> {
    entries
        .into_iter()
        .map(|((x, z), bytes)| (ChunkKey::new(x, z), bytes))
        .collect()
}

/// The directory shared with `Fixtures.java`, or `None` when the cross-check was not asked for.
fn cross_dir() -> Option<PathBuf> {
    std::env::var_os("SONIX_AWF_CROSS_DIR").map(PathBuf::from)
}

/// Writes the same scenario `Fixtures.java write` does, for `Fixtures.java verify` to read.
#[test]
#[ignore = "needs the Java driver; see tests/fixtures/README.md"]
fn write_stores_for_the_java_check() {
    let dir = cross_dir().expect("SONIX_AWF_CROSS_DIR");
    let text = |text: &str| text.as_bytes().to_vec();
    let incremental = PersistenceMode::Incremental;
    let mut expected = String::new();

    let full = open(
        &dir.join("rust-full"),
        WorldRole::Vanilla,
        3,
        Compaction::FULL_ONLY,
    );
    let first = chunks([
        ((0, 0), text("a")),
        ((1, 0), Vec::new()),
        ((-3, 7), pattern(1, 20_000)),
        ((i32::MAX, i32::MIN), text("corner")),
        ((5, 5), text("to-delete")),
        ((6, 6), text("to-remove")),
        ((7, 7), text("h\u{e9}llo \u{0} \u{1F600}")),
    ]);
    full.commit(&first, &keys([]), &keys([]), incremental)
        .unwrap();
    full.commit(
        &chunks([((0, 0), text("a2"))]),
        &keys([(6, 6)]),
        &keys([(5, 5), (9, 9)]),
        incremental,
    )
    .unwrap();
    full.commit(
        &chunks([((8, 8), pattern(2, 5_000))]),
        &keys([]),
        &keys([]),
        PersistenceMode::Checkpoint,
    )
    .unwrap();
    expected.push_str(&Facts::of_store(&full).to_tsv("rust-full"));

    let deltas = open(
        &dir.join("rust-deltas"),
        WorldRole::Vanilla,
        2,
        Compaction::DELTAS,
    );
    let world: BTreeMap<ChunkKey, Vec<u8>> = (0..30)
        .map(|i| (ChunkKey::new(i, 0), text(&format!("chunk-{i}"))))
        .collect();
    deltas
        .commit(&world, &keys([]), &keys([]), incremental)
        .unwrap();
    deltas
        .commit(
            &chunks([((1, 0), text("changed-1")), ((100, 0), text("new"))]),
            &keys([(10, 0)]),
            &keys([(20, 0)]),
            incremental,
        )
        .unwrap();
    deltas
        .commit(
            &chunks([((2, 0), text("changed-2"))]),
            &keys([]),
            &keys([(21, 0)]),
            incremental,
        )
        .unwrap();
    deltas
        .commit(
            &chunks([((20, 0), text("back"))]),
            &keys([(21, 0)]),
            &keys([]),
            incremental,
        )
        .unwrap();
    expected.push_str(&Facts::of_store(&deltas).to_tsv("rust-deltas"));

    let metadata: BTreeMap<String, String> = [
        ("role", "TEMPLATE"),
        ("environment", "NORMAL"),
        ("na\u{ef}ve", "\u{e7}\u{e0}"),
        ("nul", "a\u{0}b"),
        ("emoji", "\u{1F600}"),
        ("\u{FFFD}-after-surrogates", "utf-16 order"),
        ("\u{1F600}-before-fffd", "utf-16 order"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    let image_chunks = chunks([
        ((0, 0), text("zero")),
        ((-1, -1), pattern(3, 9_000)),
        ((3, -3), Vec::new()),
        ((i32::MIN, i32::MAX), text("corner")),
    ]);
    let image = dir.join("rust-image.awf");
    Image::write(&image, &metadata, &image_chunks).unwrap();
    expected.push_str(&Facts::of_image(&Image::open(&image).unwrap()).to_tsv("rust-image.awf"));

    fs::write(dir.join("rust-expected.tsv"), expected).unwrap();
}

/// After `Fixtures.java verify` committed one more generation on each Rust-written store.
#[test]
#[ignore = "needs the Java driver; see tests/fixtures/README.md"]
fn read_what_java_appended() {
    let dir = cross_dir().expect("SONIX_AWF_CROSS_DIR");
    let mut all = expected(&dir.join("rust-expected.tsv"));
    let key = ChunkKey::new(1000, 1000);
    for (name, retained, compaction, chain) in [
        ("rust-full", 3, Compaction::FULL_ONLY, 0),
        ("rust-deltas", 2, Compaction::DELTAS, 4),
    ] {
        let mut wanted = all.remove(name).unwrap();
        let store = open(&dir.join(name), WorldRole::ReadOnly, retained, compaction);
        let facts = Facts::of_store(&store);
        wanted
            .chunks
            .insert(key, (11, ObjectName::of(b"java-on-top").to_hex()));
        assert_eq!(facts.chunks, wanted.chunks, "{name}");
        assert_eq!(facts.deleted, wanted.deleted, "{name}");
        assert_eq!(
            facts.generation,
            wanted.generation.map(|generation| generation + 1),
            "{name}"
        );
        assert_eq!(facts.chain, Some(chain), "{name}");
        assert_eq!(store.read(key).unwrap().unwrap(), b"java-on-top");
    }
}

#[test]
fn the_pattern_generator_matches_the_java_one() {
    // The (-3,7) chunk of java-full is Fixtures.pattern(1, 20000).
    let store = open(
        &fixtures().join("java-full"),
        WorldRole::ReadOnly,
        3,
        Compaction::FULL_ONLY,
    );
    assert_eq!(
        store.read(ChunkKey::new(-3, 7)).unwrap().unwrap(),
        pattern(1, 20_000)
    );
}
