use std::collections::BTreeSet;

use crate::error::Result;
use crate::key::ChunkKey;

/// Somewhere serialized chunks can be read from: an image, a committed world store, or one
/// layered over another.
///
/// The returned bytes are the caller's to keep.
pub trait ChunkSource {
    /// The chunk's serialized bytes, or `None` when this source has no such chunk.
    fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>>;

    /// Every chunk this source can return.
    fn keys(&self) -> BTreeSet<ChunkKey>;
}

/// An upper source over a lower one: a committed instance store over its template, for example.
/// The upper source wins for any chunk it has; the lower is only read for the rest.
#[derive(Debug, Clone)]
pub struct LayeredSource<U, L> {
    /// The source that wins.
    pub upper: U,
    /// The source read for what the upper one lacks.
    pub lower: L,
}

impl<U: ChunkSource, L: ChunkSource> ChunkSource for LayeredSource<U, L> {
    fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>> {
        self.upper
            .read(key)?
            .map_or_else(|| self.lower.read(key), |bytes| Ok(Some(bytes)))
    }

    fn keys(&self) -> BTreeSet<ChunkKey> {
        let mut keys = self.upper.keys();
        keys.extend(self.lower.keys());
        keys
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct Fixed(BTreeMap<ChunkKey, Vec<u8>>);

    impl ChunkSource for Fixed {
        fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>> {
            Ok(self.0.get(&key).cloned())
        }

        fn keys(&self) -> BTreeSet<ChunkKey> {
            self.0.keys().copied().collect()
        }
    }

    #[test]
    fn the_upper_source_wins_and_the_lower_fills_the_rest() {
        let both = ChunkKey::new(0, 0);
        let lower_only = ChunkKey::new(1, 0);
        let layered = LayeredSource {
            upper: Fixed(BTreeMap::from([(both, b"upper".to_vec())])),
            lower: Fixed(BTreeMap::from([
                (both, b"lower".to_vec()),
                (lower_only, b"base".to_vec()),
            ])),
        };
        assert_eq!(layered.read(both).unwrap().unwrap(), b"upper");
        assert_eq!(layered.read(lower_only).unwrap().unwrap(), b"base");
        assert_eq!(layered.read(ChunkKey::new(9, 9)).unwrap(), None);
        assert_eq!(layered.keys(), BTreeSet::from([both, lower_only]));
    }
}
