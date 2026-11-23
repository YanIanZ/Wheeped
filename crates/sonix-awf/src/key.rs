use std::cmp::Ordering;
use std::fmt;

/// A chunk's position in a world.
///
/// Ordered by `z`, then `x`: the order chunks are laid out in an index and in an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    /// Chunk x.
    pub x: i32,
    /// Chunk z.
    pub z: i32,
}

impl ChunkKey {
    /// The chunk at `x`, `z`.
    #[must_use]
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }

    /// Packs both coordinates into one value: `z` in the high half, `x` in the low half.
    #[must_use]
    pub const fn packed(self) -> i64 {
        ((self.z as i64) << 32) | (self.x as i64 & 0xFFFF_FFFF)
    }

    /// The inverse of [`Self::packed`].
    #[must_use]
    pub const fn unpack(packed: i64) -> Self {
        Self {
            x: packed as i32,
            z: ((packed as u64) >> 32) as i32,
        }
    }
}

impl Ord for ChunkKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.z.cmp(&other.z).then(self.x.cmp(&other.x))
    }
}

impl PartialOrd for ChunkKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for ChunkKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{}", self.x, self.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_round_trips_negative_and_extreme_coordinates() {
        for (x, z) in [
            (0, 0),
            (-1, 1),
            (1, -1),
            (i32::MIN, i32::MAX),
            (i32::MAX, i32::MIN),
        ] {
            assert_eq!(
                ChunkKey::unpack(ChunkKey::new(x, z).packed()),
                ChunkKey::new(x, z)
            );
        }
        // The value the Java original produces for (-1, 1): z in the high half, x unsigned below.
        assert_eq!(ChunkKey::new(-1, 1).packed(), 0x0000_0001_FFFF_FFFF);
    }

    #[test]
    fn order_is_z_then_x() {
        let mut keys = [
            ChunkKey::new(5, 0),
            ChunkKey::new(-3, 1),
            ChunkKey::new(-9, 0),
            ChunkKey::new(0, -1),
        ];
        keys.sort();
        assert_eq!(
            keys,
            [
                ChunkKey::new(0, -1),
                ChunkKey::new(-9, 0),
                ChunkKey::new(5, 0),
                ChunkKey::new(-3, 1),
            ]
        );
    }
}
