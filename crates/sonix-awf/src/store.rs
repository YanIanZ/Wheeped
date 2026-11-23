use std::collections::{BTreeMap, BTreeSet};

use crate::error::Result;
use crate::key::ChunkKey;
use crate::role::PersistenceMode;
use crate::source::ChunkSource;

/// What a commit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitResult {
    /// The generation the commit produced.
    pub generation: u64,
    /// Entries in the committed index, deletions included.
    pub chunks: usize,
    /// Objects this commit wrote.
    pub objects_written: usize,
    /// Bytes this commit wrote into objects.
    pub bytes_written: u64,
    /// Objects removed because no retained generation references them any more.
    pub objects_removed: usize,
}

/// A world's committed chunks on some backend.
///
/// The contract every backend keeps, because the world layer and the engine rely on it:
///
/// - [`Self::commit`] is atomic: afterwards either every change is visible or none is, also after
///   a crash or a lost connection part-way.
/// - A deleted chunk is remembered ([`Self::has`] true, [`ChunkSource::read`] `None`) until it is
///   written again, so it shadows the base.
/// - Blocking methods are called off the tick.
/// - Returned bytes belong to the caller; passed bytes are not retained.
pub trait Store: ChunkSource {
    /// Whether the committed state says anything about a chunk: bytes or a deletion.
    fn has(&self, key: ChunkKey) -> bool;

    /// Chunks this world deleted: they read as absent and shadow the base.
    fn deleted(&self) -> BTreeSet<ChunkKey>;

    /// Whether reading the chunk returns stored bytes rather than a deletion or nothing.
    fn stores_bytes(&self, key: ChunkKey) -> bool {
        self.has(key) && !self.deleted().contains(&key)
    }

    /// The committed generation, 0 before the first commit.
    fn generation(&self) -> Result<u64>;

    /// Commits a new generation: the current state with `changed` applied, `removed` forgotten
    /// (falling through to the base again) and `deleted` recorded as deletions.
    fn commit(
        &self,
        changed: &BTreeMap<ChunkKey, Vec<u8>>,
        removed: &BTreeSet<ChunkKey>,
        deleted: &BTreeSet<ChunkKey>,
        mode: PersistenceMode,
    ) -> Result<CommitResult>;

    /// Releases what the store holds on its backend. Called once, after the last commit.
    fn close(&self) -> Result<()> {
        Ok(())
    }

    /// Whether the committed state is self-contained rather than chained to earlier generations.
    fn full_index(&self) -> bool {
        true
    }

    /// Writes the committed state as a new self-contained generation, changing nothing in it.
    /// `None` when there is nothing to compact.
    fn compact(&self) -> Result<Option<CommitResult>> {
        Ok(None)
    }
}
