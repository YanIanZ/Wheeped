//! The `.awf` version 1 file: an immutable world image for templates and virtual worlds.
//!
//! Layout: a header, immutable string metadata, a chunk index, then each chunk deflate-compressed.
//! [`Image::open`] reads only the header, metadata and index, enough to register a world, and a
//! chunk is decompressed the first time [`Image::read`] asks for it: registering a world costs its
//! index, not its size.
//!
//! Every chunk carries the SHA-256 of its uncompressed bytes. A chunk that fails to inflate or to
//! match its hash is reported as corrupt, never returned.
//!
//! ```text
//! "AWF1" int(version)
//! int(metaCount)  { UTF key, UTF value } * metaCount
//! int(chunkCount) { int x, int z, long offset, int storedLength, int rawLength, byte[32] sha256 } * chunkCount
//! chunk data
//! ```
//!
//! Integers are big-endian and strings are Java's `DataOutput.writeUTF`: a two-byte length and
//! modified UTF-8.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pumpkin_nbt::cesu8::{from_java_cesu8, to_java_cesu8};

use crate::codec;
use crate::error::{AwfError, Result};
use crate::hash::sha256;
use crate::key::ChunkKey;
use crate::object_store::remove_file_if_exists;
use crate::source::ChunkSource;

const MAGIC: u32 = 0x4157_4631; // "AWF1"
const VERSION: i32 = 1;
/// The most metadata entries an image may carry.
pub const MAX_META: usize = 1024;
/// The most chunks an image may carry.
pub const MAX_CHUNKS: usize = 4_000_000;
/// No serialized chunk approaches this; a larger claim is corruption.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;
const INDEX_ENTRY_BYTES: u64 = 4 + 4 + 8 + 4 + 4 + 32;
const TEMP_SUFFIX: &str = ".tmp";

struct Slot {
    offset: u64,
    stored_length: usize,
    raw_length: usize,
    sha256: [u8; 32],
}

/// An open `.awf` version 1 image.
pub struct Image {
    file: File,
    metadata: BTreeMap<String, String>,
    index: BTreeMap<ChunkKey, Slot>,
    materialized: AtomicU64,
    bytes_read: AtomicU64,
}

impl Image {
    /// Writes an image atomically: to a temporary sibling, forced, then moved into place.
    pub fn write(
        file: &Path,
        metadata: &BTreeMap<String, String>,
        chunks: &BTreeMap<ChunkKey, Vec<u8>>,
    ) -> Result<()> {
        if metadata.len() > MAX_META || chunks.len() > MAX_CHUNKS {
            return Err(AwfError::InvalidArgument(
                "too many metadata entries or chunks".to_string(),
            ));
        }
        // Compress first so the index can carry final offsets.
        let mut stored = Vec::with_capacity(chunks.len());
        for (key, raw) in chunks {
            if raw.len() > MAX_CHUNK_BYTES {
                return Err(AwfError::InvalidArgument(format!(
                    "chunk {key} exceeds {MAX_CHUNK_BYTES} bytes"
                )));
            }
            stored.push(codec::deflate(raw)?);
        }
        let mut header = Vec::new();
        header.extend(MAGIC.to_be_bytes());
        header.extend(VERSION.to_be_bytes());
        header.extend(length_field(metadata.len())?);
        // Java orders the keys by UTF-16 code unit, which is not Rust's order for every string.
        let mut entries: Vec<(&String, &String)> = metadata.iter().collect();
        entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        for (key, value) in entries {
            write_utf(&mut header, key)?;
            write_utf(&mut header, value)?;
        }
        header.extend(length_field(chunks.len())?);
        let mut offset = header.len() as u64 + chunks.len() as u64 * INDEX_ENTRY_BYTES;
        for ((key, raw), data) in chunks.iter().zip(&stored) {
            header.extend(key.x.to_be_bytes());
            header.extend(key.z.to_be_bytes());
            header.extend(offset.to_be_bytes());
            header.extend(length_field(data.len())?);
            header.extend(length_field(raw.len())?);
            header.extend(sha256(raw));
            offset += data.len() as u64;
        }

        let absolute = std::path::absolute(file)?;
        let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
            return Err(AwfError::InvalidArgument(format!(
                "{} is not a file path",
                file.display()
            )));
        };
        fs::create_dir_all(parent)?;
        let (temp, mut out) = create_temp(parent, &name.to_string_lossy())?;
        let written = (|| {
            out.write_all(&header)?;
            for data in &stored {
                out.write_all(data)?;
            }
            out.sync_all()?;
            drop(out);
            match fs::rename(&temp, file) {
                // An image is a new file nobody reads yet, so where the filesystem cannot rename
                // it into place it is copied there, as the Java original does.
                Err(not_atomic) if not_atomic.kind() == ErrorKind::CrossesDevices => {
                    fs::copy(&temp, file).map(|_| ())
                }
                renamed => renamed,
            }
        })();
        // Gone already when the rename succeeded.
        remove_file_if_exists(&temp)?;
        Ok(written?)
    }

    /// Opens an image, reading its header, metadata and index only.
    pub fn open(file: &Path) -> Result<Self> {
        let handle = File::open(file)?;
        let size = handle.metadata()?.len();
        let mut input = BufReader::new(&handle);
        if read_u32(&mut input)? != MAGIC {
            return Err(AwfError::Corrupt(format!(
                "{} is not an AWF image",
                file.display()
            )));
        }
        let version = read_i32(&mut input)?;
        if version != VERSION {
            return Err(AwfError::Corrupt(format!(
                "{} has unsupported AWF version {version}",
                file.display()
            )));
        }
        let meta_count = read_count(&mut input, MAX_META, "metadata")?;
        let mut metadata = BTreeMap::new();
        for _ in 0..meta_count {
            let key = read_utf(&mut input)?;
            let value = read_utf(&mut input)?;
            metadata.insert(key, value);
        }
        let chunk_count = read_count(&mut input, MAX_CHUNKS, "chunk")?;
        let mut index = BTreeMap::new();
        for _ in 0..chunk_count {
            let key = ChunkKey::new(read_i32(&mut input)?, read_i32(&mut input)?);
            let offset = read_i64(&mut input)?;
            let stored = read_i32(&mut input)?;
            let raw = read_i32(&mut input)?;
            let mut sha256 = [0u8; 32];
            input.read_exact(&mut sha256)?;
            let slot = slot(size, offset, stored, raw, sha256).ok_or_else(|| {
                AwfError::Corrupt(format!("index entry {key} points outside the file"))
            })?;
            if index.insert(key, slot).is_some() {
                return Err(AwfError::Corrupt(format!("duplicate chunk {key}")));
            }
        }
        drop(input);
        Ok(Self {
            file: handle,
            metadata,
            index,
            materialized: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
        })
    }

    /// The image's metadata.
    #[must_use]
    pub const fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// Chunks decompressed so far.
    #[must_use]
    pub fn materialized(&self) -> u64 {
        self.materialized.load(Ordering::Relaxed)
    }

    /// Compressed bytes read so far.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.load(Ordering::Relaxed)
    }
}

impl ChunkSource for Image {
    /// Decompresses and verifies one chunk. Safe to call from several threads.
    fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>> {
        let Some(slot) = self.index.get(&key) else {
            return Ok(None);
        };
        let mut stored = vec![0u8; slot.stored_length];
        match read_exact_at(&self.file, &mut stored, slot.offset) {
            Ok(()) => {}
            Err(truncated) if truncated.kind() == ErrorKind::UnexpectedEof => {
                return Err(AwfError::Corrupt(format!("chunk {key} is truncated")));
            }
            Err(failed) => return Err(failed.into()),
        }
        self.bytes_read
            .fetch_add(slot.stored_length as u64, Ordering::Relaxed);
        let raw = codec::inflate(&stored, slot.raw_length, &format_args!("chunk {key}"))?;
        if sha256(&raw) != slot.sha256 {
            return Err(AwfError::Corrupt(format!("chunk {key} fails its SHA-256")));
        }
        self.materialized.fetch_add(1, Ordering::Relaxed);
        Ok(Some(raw))
    }

    fn keys(&self) -> BTreeSet<ChunkKey> {
        self.index.keys().copied().collect()
    }
}

/// An index entry's place in a file of `size` bytes, or `None` when it points outside it.
fn slot(size: u64, offset: i64, stored: i32, raw: i32, sha256: [u8; 32]) -> Option<Slot> {
    let offset = u64::try_from(offset).ok()?;
    let stored = u32::try_from(stored).ok()?;
    let raw_length = usize::try_from(raw).ok()?;
    // offset + stored <= size, written so that it cannot overflow for an offset near the maximum.
    let end = offset.checked_add(u64::from(stored))?;
    if raw_length > MAX_CHUNK_BYTES || end > size {
        return None;
    }
    Some(Slot {
        offset,
        stored_length: stored as usize,
        raw_length,
        sha256,
    })
}

/// A count or length as the four-byte signed field the format stores it in.
fn length_field(length: usize) -> Result<[u8; 4]> {
    i32::try_from(length)
        .map(i32::to_be_bytes)
        .map_err(|_| AwfError::InvalidArgument(format!("{length} does not fit an AWF image field")))
}

fn write_utf(out: &mut Vec<u8>, text: &str) -> Result<()> {
    let encoded = to_java_cesu8(text);
    let length = u16::try_from(encoded.len()).map_err(|_| {
        AwfError::InvalidArgument(format!(
            "a metadata string of {} encoded bytes does not fit an AWF image",
            encoded.len()
        ))
    })?;
    out.extend(length.to_be_bytes());
    out.extend_from_slice(&encoded);
    Ok(())
}

fn read_utf(input: &mut impl Read) -> Result<String> {
    let mut length = [0u8; 2];
    input.read_exact(&mut length)?;
    let mut encoded = vec![0u8; usize::from(u16::from_be_bytes(length))];
    input.read_exact(&mut encoded)?;
    from_java_cesu8(&encoded)
        .map(std::borrow::Cow::into_owned)
        .map_err(|_| AwfError::Corrupt("a metadata string is not valid modified UTF-8".to_string()))
}

fn read_count(input: &mut impl Read, max: usize, what: &str) -> Result<usize> {
    let count = read_i32(input)?;
    usize::try_from(count)
        .ok()
        .filter(|count| *count <= max)
        .ok_or_else(|| AwfError::Corrupt(format!("bad {what} count {count}")))
}

fn read_u32(input: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_i32(input: &mut impl Read) -> io::Result<i32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(i32::from_be_bytes(bytes))
}

fn read_i64(input: &mut impl Read) -> io::Result<i64> {
    let mut bytes = [0u8; 8];
    input.read_exact(&mut bytes)?;
    Ok(i64::from_be_bytes(bytes))
}

/// Creates a new file next to `name` in `parent` for an atomic replace.
fn create_temp(parent: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    let mut attempt = 0u32;
    loop {
        let candidate = parent.join(format!(
            "{name}.{}.{attempt}{TEMP_SUFFIX}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(taken) if taken.kind() == ErrorKind::AlreadyExists => attempt += 1,
            Err(failed) => return Err(failed),
        }
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        match file.seek_read(buffer, offset) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                buffer = &mut buffer[read..];
                offset += read as u64;
            }
            Err(interrupted) if interrupted.kind() == ErrorKind::Interrupted => {}
            Err(failed) => return Err(failed),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(count: i32) -> BTreeMap<ChunkKey, Vec<u8>> {
        (0..count)
            .map(|i| {
                (
                    ChunkKey::new(i, -i),
                    format!("chunk {i} ").repeat(50).into_bytes(),
                )
            })
            .collect()
    }

    fn metadata<const N: usize>(entries: [(&str, &str); N]) -> BTreeMap<String, String> {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn opening_reads_no_chunk_and_reads_are_lazy() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("t.awf");
        Image::write(&file, &metadata([("role", "TEMPLATE")]), &chunks(10)).unwrap();
        let image = Image::open(&file).unwrap();
        assert_eq!(image.keys().len(), 10);
        assert_eq!(image.metadata()["role"], "TEMPLATE");
        assert_eq!(image.materialized(), 0);
        assert_eq!(
            image.read(ChunkKey::new(3, -3)).unwrap().as_ref(),
            Some(&chunks(10)[&ChunkKey::new(3, -3)])
        );
        assert_eq!(image.materialized(), 1);
        assert_eq!(image.read(ChunkKey::new(99, 99)).unwrap(), None);
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            1,
            "the temporary sibling is gone"
        );
    }

    #[test]
    fn the_header_is_big_endian_with_java_strings_and_final_offsets() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("h.awf");
        let one = BTreeMap::from([(ChunkKey::new(-2, 7), b"payload".to_vec())]);
        Image::write(&file, &metadata([("b", "\u{0}"), ("a", "é")]), &one).unwrap();
        let bytes = fs::read(&file).unwrap();
        let mut expected = Vec::new();
        expected.extend(b"AWF1");
        expected.extend(1i32.to_be_bytes());
        expected.extend(2i32.to_be_bytes());
        // Keys in order; "é" is two bytes, a NUL is the two-byte form 0xC0 0x80.
        expected.extend([0, 1, b'a', 0, 2, 0xC3, 0xA9]);
        expected.extend([0, 1, b'b', 0, 2, 0xC0, 0x80]);
        expected.extend(1i32.to_be_bytes());
        let data_offset = expected.len() as u64 + INDEX_ENTRY_BYTES;
        expected.extend((-2i32).to_be_bytes());
        expected.extend(7i32.to_be_bytes());
        expected.extend(data_offset.to_be_bytes());
        let stored = &bytes[data_offset as usize..];
        expected.extend((stored.len() as i32).to_be_bytes());
        expected.extend(7i32.to_be_bytes());
        expected.extend(sha256(b"payload"));
        assert_eq!(bytes[..data_offset as usize], expected);
        assert_eq!(codec::inflate(stored, 7, &"chunk").unwrap(), b"payload");
        assert_eq!(
            Image::open(&file).unwrap().metadata(),
            &metadata([("a", "é"), ("b", "\u{0}")])
        );
    }

    #[test]
    fn an_empty_chunk_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("e.awf");
        let one = BTreeMap::from([(ChunkKey::new(0, 0), Vec::new())]);
        Image::write(&file, &metadata([]), &one).unwrap();
        let image = Image::open(&file).unwrap();
        assert_eq!(image.read(ChunkKey::new(0, 0)).unwrap(), Some(Vec::new()));
    }

    #[test]
    fn a_corrupt_chunk_is_reported_not_returned() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("c.awf");
        Image::write(&file, &metadata([]), &chunks(2)).unwrap();
        let mut bytes = fs::read(&file).unwrap();
        let tail = bytes.len() - 6;
        bytes[tail..tail + 4].copy_from_slice(&[0x55; 4]);
        fs::write(&file, bytes).unwrap();

        let image = Image::open(&file).unwrap();
        // Data is laid out in key order, so the tail of the file is the last key's chunk.
        let keys: Vec<ChunkKey> = image.keys().into_iter().collect();
        let (intact, damaged) = (keys[0], keys[keys.len() - 1]);
        assert!(matches!(image.read(damaged), Err(AwfError::Corrupt(_))));
        assert_eq!(
            image.read(intact).unwrap().as_ref(),
            Some(&chunks(2)[&intact]),
            "damage to one chunk leaves the others readable"
        );
    }

    #[test]
    fn not_an_image_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("x.awf");
        fs::write(&file, "hello world, not an image").unwrap();
        assert!(matches!(Image::open(&file), Err(AwfError::Corrupt(_))));
    }

    #[test]
    fn a_truncated_index_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("t.awf");
        Image::write(&file, &metadata([]), &chunks(3)).unwrap();
        let bytes = fs::read(&file).unwrap();
        fs::write(&file, &bytes[..40]).unwrap();
        assert!(Image::open(&file).is_err());
    }

    #[test]
    fn an_index_entry_outside_the_file_is_refused_without_allocating_for_it() {
        let ok = |offset, stored, raw| slot(1000, offset, stored, raw, [0; 32]).is_some();
        assert!(ok(0, 1000, 5));
        assert!(ok(1000, 0, 0));
        assert!(!ok(1, 1000, 5), "one byte past the end");
        assert!(!ok(-1, 10, 5));
        assert!(!ok(0, -1, 5));
        assert!(!ok(0, 10, -1));
        assert!(!ok(i64::MAX, i32::MAX, 5), "the sum must not wrap");
        assert!(
            !ok(0, 10, MAX_CHUNK_BYTES as i32 + 1),
            "a declared size no chunk reaches"
        );
    }
}
