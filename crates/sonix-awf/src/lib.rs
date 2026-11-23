//! Aurora World Fabric (AWF) storage core.
//!
//! The FILE backend of `SourbyCraft`'s world storage, ported from Java. A chunk is a
//! content-addressed object, a world's state is a chunk index committed as an atomic generation,
//! an `.awf` image carries an immutable world, and an `.awf` world file carries a portable one. The
//! on-disk format is the one `SourbyCraft` writes, so either server can open what the other wrote.
//!
//! Over a store, a [`World`] is one world at runtime: the chunks it owns in memory, copy-on-write
//! over a base, committed through the store in the order their snapshots were taken.
//!
//! Nothing here knows about a running server. Every call that touches the disk blocks, so callers
//! run it off the tick.
//!
//! Layout of one world store:
//!
//! ```text
//! <store>/generations-root/CURRENT                 "<N>\t<sha256 of N's manifest>"
//! <store>/generations-root/generations/<N>/manifest
//! <store>/generations-root/generations/<N>/index   or  delta
//! <store>/objects/<first two hex digits>/<sha256 of the chunk's bytes>
//! ```
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod codec;
mod commit_lock;
mod durability;
mod error;
mod generation;
mod hash;
mod image;
mod key;
mod metrics;
mod object_store;
mod role;
mod shutdown;
mod source;
mod store;
mod text;
mod world;
mod world_file;
mod world_store;

pub use codec::Codec;
pub use durability::{DISK, Disk, Durability, degraded_directory_forces, degraded_reason};
pub use error::{AwfError, Result};
pub use generation::{Generation, GenerationStore, Stage, StageHook};
pub use image::{Image, MAX_CHUNK_BYTES, MAX_CHUNKS, MAX_META};
pub use key::ChunkKey;
pub use metrics::{LatencyRecorder, MetricsSnapshot, Percentiles};
pub use object_store::{ObjectName, ObjectStore, Written};
pub use role::{PersistenceMode, WorldRole};
pub use shutdown::{ShutdownBudget, monotonic_nanos};
pub use source::{ChunkSource, LayeredSource};
pub use store::{CommitResult, Store};
pub use world::{MAX_PENDING_SAVES, QueuedSave, SharedSource, SharedStore, World, WorldBase};
pub use world_file::{Stream as WorldFileStream, WorldFile, Writer as WorldFileWriter};
pub use world_store::{Compaction, Entry, WorldStore};

/// Locks a mutex, taking the guard even when a holder panicked.
///
/// The stores keep their state consistent at every point a call can fail, as the Java originals do
/// for a `synchronized` method that throws: the lock is released and the next caller proceeds.
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
