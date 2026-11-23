//! The portable world file, `.awf` version 2: one file holding a whole world (its chunks, entities
//! and POI) and what it needs to be created again (environment, seed, generator).
//!
//! - **Random access.** Chunks are compressed one by one and located through an index, so a reader
//!   decompresses only the chunks it asks for; opening a file reads its index alone.
//! - **A codec per chunk**: none, deflate or zstd. Either side reads the other's files.
//! - **Streaming writes.** The index is written after the chunks, so a writer holds only the index
//!   in memory, never the world.
//! - **Checked.** Every chunk carries the CRC32C of its uncompressed bytes and the index its own;
//!   damage is reported, never returned as a chunk. A file is written to a temporary sibling,
//!   forced and moved into place.
//!
//! ```text
//! "AWFW" int(2)
//! chunk data ...
//! index:   int(metaCount) { UTF key, UTF value }
//!          int(streamCount) { UTF name, int(count) { int x, int z, long offset, int stored, int raw,
//!                                                    byte codec, int crc32c } }
//! trailer: long(indexOffset) int(indexLength) int(indexCrc32c) "AWFW"
//! ```
//!
//! The version 1 [`Image`](crate::Image) (a single chunk stream with SHA-256) remains the
//! immutable image; this is the format worlds are exported to and imported from.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::codec::Codec;
use crate::error::{AwfError, Result};
use crate::image::{
    MAX_CHUNK_BYTES, MAX_CHUNKS, MAX_META, create_temp, read_count, read_exact_at, read_i32,
    read_i64, read_utf, write_utf,
};
use crate::key::ChunkKey;
use crate::object_store::remove_file_if_exists;
use crate::source::ChunkSource;

const MAGIC: u32 = 0x4157_4657; // "AWFW"
const VERSION: i32 = 2;
const HEADER_BYTES: u64 = 8;
const TRAILER_BYTES: u64 = 8 + 4 + 4 + 4;
const MAX_INDEX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_STREAMS: usize = 16;
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

/// Where one chunk is.
#[derive(Clone, Copy)]
struct Slot {
    offset: u64,
    stored: u32,
    raw: u32,
    /// The codec id as stored; an id this build does not know fails the read of that chunk only.
    codec: u8,
    crc: u32,
}

/// An open `.awf` version 2 world file.
pub struct WorldFile {
    file: File,
    metadata: BTreeMap<String, String>,
    /// Stream name to its chunks, in the order the file lists the streams.
    streams: IndexMap<String, BTreeMap<ChunkKey, Slot>>,
}

impl WorldFile {
    /// The suffix of a world file's name.
    pub const SUFFIX: &'static str = ".awf";

    /// Whether a file starts like an AWF world file.
    #[must_use]
    pub fn looks_like(head: &[u8]) -> bool {
        head.first_chunk::<4>()
            .is_some_and(|magic| u32::from_be_bytes(*magic) == MAGIC)
    }

    /// Opens a file, reading its trailer and index only.
    pub fn open(file: &Path) -> Result<Self> {
        let handle = File::open(file)?;
        let size = handle.metadata()?.len();
        let corrupt = |what: &str| AwfError::Corrupt(format!("{} {what}", file.display()));
        if size < HEADER_BYTES + TRAILER_BYTES {
            return Err(corrupt("is too short to be an AWF world"));
        }
        let head = read_at(&handle, 0, HEADER_BYTES)?;
        let mut head = head.as_slice();
        if read_i32(&mut head)?.cast_unsigned() != MAGIC {
            return Err(corrupt("is not an AWF world file"));
        }
        let version = read_i32(&mut head)?;
        if version != VERSION {
            return Err(corrupt(&format!(
                "is AWF world version {version}, not {VERSION}"
            )));
        }
        let trailer = read_at(&handle, size - TRAILER_BYTES, TRAILER_BYTES)?;
        let mut trailer = trailer.as_slice();
        let index_offset = read_i64(&mut trailer)?;
        let index_length = read_i32(&mut trailer)?;
        let index_crc = read_i32(&mut trailer)?.cast_unsigned();
        if read_i32(&mut trailer)?.cast_unsigned() != MAGIC {
            return Err(corrupt("has no AWF trailer (truncated?)"));
        }
        let (index_offset, index_length) = u64::try_from(index_offset)
            .ok()
            .zip(u64::try_from(index_length).ok())
            .filter(|(offset, length)| {
                *offset >= HEADER_BYTES
                    && *length <= MAX_INDEX_BYTES
                    && offset.checked_add(*length) == Some(size - TRAILER_BYTES)
            })
            .ok_or_else(|| corrupt("has an index outside the file"))?;
        let index = read_at(&handle, index_offset, index_length)?;
        if crc32c::crc32c(&index) != index_crc {
            return Err(corrupt("index fails its CRC32C"));
        }
        let (metadata, streams) = parse_index(&index, index_offset)?;
        Ok(Self {
            file: handle,
            metadata,
            streams,
        })
    }

    /// The file's metadata.
    #[must_use]
    pub const fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// The names of the file's streams, in the order it lists them.
    pub fn stream_names(&self) -> impl Iterator<Item = &str> {
        self.streams.keys().map(String::as_str)
    }

    /// One stream's chunks, decompressed and checked on demand; an absent stream is empty.
    #[must_use]
    pub fn stream<'a>(&'a self, name: &'a str) -> Stream<'a> {
        Stream {
            file: &self.file,
            name,
            index: self.streams.get(name),
        }
    }

    /// Chunks per stream, for display.
    #[must_use]
    pub fn counts(&self) -> Vec<(&str, usize)> {
        self.streams
            .iter()
            .map(|(name, index)| (name.as_str(), index.len()))
            .collect()
    }

    /// Starts a file at `target`; nothing appears there until [`Writer::finish`].
    pub fn writer(target: &Path, codec: Codec) -> Result<Writer> {
        Writer::new(target, codec)
    }
}

/// One stream of an open world file.
pub struct Stream<'a> {
    file: &'a File,
    name: &'a str,
    index: Option<&'a BTreeMap<ChunkKey, Slot>>,
}

impl ChunkSource for Stream<'_> {
    fn read(&self, key: ChunkKey) -> Result<Option<Vec<u8>>> {
        let Some(slot) = self.index.and_then(|index| index.get(&key)) else {
            return Ok(None);
        };
        let what = format!("{} chunk {key}", self.name);
        let stored = read_at(self.file, slot.offset, u64::from(slot.stored))?;
        let codec = Codec::from_id(slot.codec).ok_or_else(|| {
            AwfError::Corrupt(format!("{what} uses unknown codec {}", slot.codec))
        })?;
        let raw = codec.decompress(&stored, slot.raw as usize, &what)?;
        if crc32c::crc32c(&raw) != slot.crc {
            return Err(AwfError::Corrupt(format!("{what} fails its CRC32C")));
        }
        Ok(Some(raw))
    }

    fn keys(&self) -> BTreeSet<ChunkKey> {
        self.index
            .map(|index| index.keys().copied().collect())
            .unwrap_or_default()
    }
}

type Streams = IndexMap<String, BTreeMap<ChunkKey, Slot>>;

/// Parses the index block. `index_offset` is where the chunk data ends.
fn parse_index(index: &[u8], index_offset: u64) -> Result<(BTreeMap<String, String>, Streams)> {
    let mut input = index;
    let meta_count = read_count(&mut input, MAX_META, "metadata")?;
    let mut metadata = BTreeMap::new();
    for _ in 0..meta_count {
        let key = read_utf(&mut input)?;
        let value = read_utf(&mut input)?;
        metadata.insert(key, value);
    }
    let stream_count = read_count(&mut input, MAX_STREAMS, "stream")?;
    let mut streams = Streams::new();
    for _ in 0..stream_count {
        let name = read_utf(&mut input)?;
        let count = read_count(&mut input, MAX_CHUNKS, "chunk")?;
        let mut chunks = BTreeMap::new();
        for _ in 0..count {
            let key = ChunkKey::new(read_i32(&mut input)?, read_i32(&mut input)?);
            let offset = read_i64(&mut input)?;
            let stored = read_i32(&mut input)?;
            let raw = read_i32(&mut input)?;
            let mut codec = [0u8; 1];
            input.read_exact(&mut codec)?;
            let crc = read_i32(&mut input)?.cast_unsigned();
            let slot = slot(index_offset, offset, stored, raw, codec[0], crc).ok_or_else(|| {
                AwfError::Corrupt(format!("{name} chunk {key} points outside the chunk data"))
            })?;
            if chunks.insert(key, slot).is_some() {
                return Err(AwfError::Corrupt(format!("duplicate {name} chunk {key}")));
            }
        }
        if streams.contains_key(&name) {
            return Err(AwfError::Corrupt(format!("duplicate stream {name}")));
        }
        streams.insert(name, chunks);
    }
    Ok((metadata, streams))
}

/// An index entry's place in the chunk data, which ends at `data_end`, or `None` when it points
/// outside it.
fn slot(data_end: u64, offset: i64, stored: i32, raw: i32, codec: u8, crc: u32) -> Option<Slot> {
    let offset = u64::try_from(offset).ok()?;
    let stored = u32::try_from(stored).ok()?;
    let raw = u32::try_from(raw).ok()?;
    // offset + stored <= data_end, written so that it cannot overflow for an offset near the maximum.
    let end = offset.checked_add(u64::from(stored))?;
    if offset < HEADER_BYTES || raw as usize > MAX_CHUNK_BYTES || end > data_end {
        return None;
    }
    Some(Slot {
        offset,
        stored,
        raw,
        codec,
        crc,
    })
}

fn read_at(file: &File, offset: u64, length: u64) -> Result<Vec<u8>> {
    let length = usize::try_from(length)
        .map_err(|_| AwfError::Corrupt("AWF world file is too large to read here".to_string()))?;
    let mut buffer = vec![0u8; length];
    match read_exact_at(file, &mut buffer, offset) {
        Ok(()) => Ok(buffer),
        Err(truncated) if truncated.kind() == ErrorKind::UnexpectedEof => {
            Err(AwfError::Corrupt("AWF world file is truncated".to_string()))
        }
        Err(failed) => Err(failed.into()),
    }
}

/// A failure the Java writer reports as an `IOException`: something the caller handed over that
/// cannot be written.
fn refused(message: String) -> AwfError {
    io::Error::new(ErrorKind::InvalidInput, message).into()
}

/// Writes chunks as they come; holds only the index in memory.
///
/// Dropping a writer that was not finished removes its temporary file.
pub struct Writer {
    target: PathBuf,
    temporary: PathBuf,
    out: Option<BufWriter<File>>,
    codec: Codec,
    streams: IndexMap<String, Vec<(ChunkKey, Slot)>>,
    position: u64,
}

impl Writer {
    fn new(target: &Path, codec: Codec) -> Result<Self> {
        let absolute = std::path::absolute(target)?;
        let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
            return Err(AwfError::InvalidArgument(format!(
                "{} is not a file path",
                target.display()
            )));
        };
        fs::create_dir_all(parent)?;
        let (temporary, file) = create_temp(parent, &format!(".{}", name.to_string_lossy()))?;
        let mut writer = Self {
            target: target.to_path_buf(),
            temporary,
            out: Some(BufWriter::with_capacity(WRITE_BUFFER_BYTES, file)),
            codec,
            streams: IndexMap::new(),
            position: HEADER_BYTES,
        };
        let mut header = Vec::with_capacity(HEADER_BYTES as usize);
        header.extend(MAGIC.to_be_bytes());
        header.extend(VERSION.to_be_bytes());
        writer.write(&header)?;
        Ok(writer)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        match &mut self.out {
            Some(out) => Ok(out.write_all(bytes)?),
            None => Err(AwfError::InvalidState(
                "this AWF world file was already finished".to_string(),
            )),
        }
    }

    /// The codec this writer stores chunks with.
    #[must_use]
    pub const fn codec(&self) -> Codec {
        self.codec
    }

    /// Appends one chunk to a stream. A chunk already in that stream is refused by
    /// [`Self::finish`].
    pub fn add(&mut self, stream: &str, key: ChunkKey, raw: &[u8]) -> Result<()> {
        if raw.len() > MAX_CHUNK_BYTES {
            return Err(refused(format!("{stream} chunk {key} is too large")));
        }
        let stored = self.codec.compress(raw)?;
        let (Ok(stored_length), Ok(raw_length)) =
            (u32::try_from(stored.len()), u32::try_from(raw.len()))
        else {
            return Err(refused(format!("{stream} chunk {key} is too large")));
        };
        self.write(&stored)?;
        let slot = Slot {
            offset: self.position,
            stored: stored_length,
            raw: raw_length,
            codec: self.codec.id(),
            crc: crc32c::crc32c(raw),
        };
        self.streams
            .entry(stream.to_string())
            .or_default()
            .push((key, slot));
        self.position += u64::from(stored_length);
        Ok(())
    }

    /// Writes the index and trailer, forces the file and moves it into place.
    pub fn finish(mut self, metadata: &BTreeMap<String, String>) -> Result<()> {
        if metadata.len() > MAX_META {
            return Err(AwfError::InvalidArgument(
                "too many metadata entries".to_string(),
            ));
        }
        if self.streams.len() > MAX_STREAMS {
            return Err(AwfError::InvalidArgument("too many streams".to_string()));
        }
        let index = self.encode_index(metadata)?;
        let mut tail = index.clone();
        tail.extend(self.position.to_be_bytes());
        tail.extend(field(index.len())?);
        tail.extend(crc32c::crc32c(&index).to_be_bytes());
        tail.extend(MAGIC.to_be_bytes());
        self.write(&tail)?;
        let Some(out) = self.out.take() else {
            return Err(AwfError::InvalidState(
                "this AWF world file was already finished".to_string(),
            ));
        };
        let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        share(&file)?;
        drop(file);
        match fs::rename(&self.temporary, &self.target) {
            // As for an image: a new file nobody reads yet may be copied into place where the
            // filesystem cannot rename it there.
            Err(not_atomic) if not_atomic.kind() == ErrorKind::CrossesDevices => {
                fs::copy(&self.temporary, &self.target)?;
            }
            renamed => renamed?,
        }
        Ok(())
    }

    fn encode_index(&self, metadata: &BTreeMap<String, String>) -> Result<Vec<u8>> {
        let mut index = Vec::new();
        index.extend(field(metadata.len())?);
        // Java orders the keys by UTF-16 code unit, which is not Rust's order for every string.
        let mut entries: Vec<(&String, &String)> = metadata.iter().collect();
        entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        for (key, value) in entries {
            write_utf(&mut index, key)?;
            write_utf(&mut index, value)?;
        }
        index.extend(field(self.streams.len())?);
        for (name, chunks) in &self.streams {
            write_utf(&mut index, name)?;
            index.extend(field(chunks.len())?);
            let mut seen = HashSet::new();
            for (key, slot) in chunks {
                if !seen.insert(*key) {
                    return Err(refused(format!("duplicate {name} chunk {key}")));
                }
                index.extend(key.x.to_be_bytes());
                index.extend(key.z.to_be_bytes());
                index.extend(slot.offset.to_be_bytes());
                index.extend(slot.stored.to_be_bytes());
                index.extend(slot.raw.to_be_bytes());
                index.push(slot.codec);
                index.extend(slot.crc.to_be_bytes());
            }
        }
        Ok(index)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Finished: the temporary file was renamed away and this finds nothing. Abandoned: the
        // file is closed first, then removed. A failure here has nobody to report to.
        drop(self.out.take());
        if let Err(failed) = remove_file_if_exists(&self.temporary) {
            tracing::warn!(
                "could not remove the unfinished AWF world file {}: {failed}",
                self.temporary.display()
            );
        }
    }
}

/// A count or length as the four-byte signed field the format stores it in.
fn field(length: usize) -> Result<[u8; 4]> {
    i32::try_from(length)
        .map(i32::to_be_bytes)
        .map_err(|_| refused(format!("{length} does not fit an AWF world file field")))
}

/// A world file is meant to be shared; a temporary file may have been created owner-only.
#[cfg(unix)]
fn share(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o644))
}

/// Windows: the file inherits its folder's permissions.
#[cfg(not(unix))]
fn share(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(n: i32) -> Vec<u8> {
        format!("chunk {n} ").repeat(400).into_bytes()
    }

    fn metadata<const N: usize>(entries: [(&str, &str); N]) -> BTreeMap<String, String> {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// Seven region chunks at (n, 2n) for n in -3..=3, and one entities chunk.
    fn write(dir: &Path, codec: Codec) -> PathBuf {
        let file = dir.join(format!("world-{codec}.awf"));
        let mut writer = WorldFile::writer(&file, codec).unwrap();
        assert_eq!(writer.codec(), codec);
        for n in -3..=3 {
            writer
                .add("region", ChunkKey::new(n, 2 * n), &chunk(n))
                .unwrap();
        }
        writer.add("entities", ChunkKey::new(0, 0), b"cow").unwrap();
        writer
            .finish(&metadata([
                ("format", "awf-world"),
                ("environment", "normal"),
            ]))
            .unwrap();
        file
    }

    #[test]
    fn every_codec_round_trips_and_reads_chunks_on_demand() {
        let temp = tempfile::tempdir().unwrap();
        for codec in [Codec::None, Codec::Deflate, Codec::Zstd] {
            let file = WorldFile::open(&write(temp.path(), codec)).unwrap();
            assert_eq!(file.metadata()["environment"], "normal");
            assert_eq!(file.counts(), [("region", 7), ("entities", 1)]);
            assert_eq!(
                file.stream_names().collect::<Vec<_>>(),
                ["region", "entities"]
            );
            let region = file.stream("region");
            assert_eq!(
                region.read(ChunkKey::new(2, 4)).unwrap().unwrap(),
                chunk(2),
                "{codec}"
            );
            assert_eq!(region.read(ChunkKey::new(2, 5)).unwrap(), None);
            assert!(
                file.stream("poi").keys().is_empty(),
                "an absent stream is empty"
            );
            assert_eq!(file.stream("poi").read(ChunkKey::new(0, 0)).unwrap(), None);
            assert_eq!(
                file.stream("entities")
                    .read(ChunkKey::new(0, 0))
                    .unwrap()
                    .unwrap(),
                b"cow"
            );
        }
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            3,
            "no temporary file is left"
        );
    }

    #[test]
    fn the_layout_is_header_chunks_index_trailer() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("layout.awf");
        let mut writer = WorldFile::writer(&file, Codec::None).unwrap();
        writer
            .add("region", ChunkKey::new(-2, 7), b"payload")
            .unwrap();
        writer.finish(&metadata([("seed", "42")])).unwrap();

        let mut index = Vec::new();
        index.extend(1i32.to_be_bytes());
        index.extend([0, 4, b's', b'e', b'e', b'd', 0, 2, b'4', b'2']);
        index.extend(1i32.to_be_bytes());
        index.extend([0, 6, b'r', b'e', b'g', b'i', b'o', b'n']);
        index.extend(1i32.to_be_bytes());
        index.extend((-2i32).to_be_bytes());
        index.extend(7i32.to_be_bytes());
        index.extend(8u64.to_be_bytes());
        index.extend(7i32.to_be_bytes());
        index.extend(7i32.to_be_bytes());
        index.push(0);
        index.extend(crc32c::crc32c(b"payload").to_be_bytes());
        let mut expected = Vec::new();
        expected.extend(b"AWFW");
        expected.extend(2i32.to_be_bytes());
        expected.extend(b"payload");
        expected.extend(&index);
        expected.extend(15u64.to_be_bytes());
        expected.extend((index.len() as i32).to_be_bytes());
        expected.extend(crc32c::crc32c(&index).to_be_bytes());
        expected.extend(b"AWFW");
        assert_eq!(fs::read(&file).unwrap(), expected);
    }

    #[test]
    fn compression_shrinks_chunk_data() {
        let temp = tempfile::tempdir().unwrap();
        let size = |codec| fs::metadata(write(temp.path(), codec)).unwrap().len();
        let none = size(Codec::None);
        assert!(size(Codec::Deflate) < none / 2);
        assert!(size(Codec::Zstd) < none / 2);
    }

    #[test]
    fn a_damaged_chunk_is_reported_and_the_others_still_read() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(temp.path(), Codec::None);
        let mut bytes = fs::read(&path).unwrap();
        bytes[8 + 5] ^= 0x55; // inside the first chunk written: (-3, -6)
        fs::write(&path, bytes).unwrap();
        let file = WorldFile::open(&path).unwrap();
        let region = file.stream("region");
        assert!(
            matches!(region.read(ChunkKey::new(-3, -6)), Err(AwfError::Corrupt(message)) if message == "region chunk -3,-6 fails its CRC32C")
        );
        assert_eq!(region.read(ChunkKey::new(3, 6)).unwrap().unwrap(), chunk(3));
    }

    #[test]
    fn a_damaged_index_or_truncated_file_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(temp.path(), Codec::Deflate);
        let bytes = fs::read(&path).unwrap();
        let refused = |damaged: &[u8]| {
            fs::write(&path, damaged).unwrap();
            matches!(WorldFile::open(&path), Err(AwfError::Corrupt(_)))
        };
        let mut bad_index = bytes.clone();
        bad_index[bytes.len() - TRAILER_BYTES as usize - 3] ^= 1;
        assert!(refused(&bad_index));
        assert!(refused(&bytes[..bytes.len() - 7]));
        assert!(refused(b"AWF1 something else"));
        assert!(refused(b"AWFW"));
    }

    #[test]
    fn an_entry_outside_the_chunk_data_or_of_an_unknown_codec_is_refused() {
        let ok = |offset, stored, raw| slot(1000, offset, stored, raw, 0, 0).is_some();
        assert!(ok(8, 992, 5));
        assert!(!ok(7, 10, 5), "inside the header");
        assert!(!ok(9, 992, 5), "one byte into the index");
        assert!(!ok(-1, 10, 5));
        assert!(!ok(8, -1, 5));
        assert!(!ok(8, 10, -1));
        assert!(!ok(i64::MAX, i32::MAX, 5), "the sum must not wrap");
        assert!(!ok(8, 10, MAX_CHUNK_BYTES as i32 + 1));

        // A codec id from a newer writer fails that chunk, not the open.
        let temp = tempfile::tempdir().unwrap();
        let path = write(temp.path(), Codec::None);
        let mut bytes = fs::read(&path).unwrap();
        let index_end = bytes.len() - TRAILER_BYTES as usize;
        let index_offset =
            u64::from_be_bytes(bytes[index_end..index_end + 8].try_into().unwrap()) as usize;
        // The last entry of the last stream ends with codec (1 byte) and crc (4 bytes).
        bytes[index_end - 5] = 9;
        let crc = crc32c::crc32c(&bytes[index_offset..index_end]);
        bytes[index_end + 12..index_end + 16].copy_from_slice(&crc.to_be_bytes());
        fs::write(&path, bytes).unwrap();
        let file = WorldFile::open(&path).unwrap();
        assert!(
            matches!(file.stream("entities").read(ChunkKey::new(0, 0)), Err(AwfError::Corrupt(message)) if message == "entities chunk 0,0 uses unknown codec 9")
        );
        assert_eq!(
            file.stream("region")
                .read(ChunkKey::new(1, 2))
                .unwrap()
                .unwrap(),
            chunk(1)
        );
    }

    #[test]
    fn an_unfinished_write_leaves_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("never.awf");
        let mut writer = WorldFile::writer(&target, Codec::DEFAULT).unwrap();
        writer
            .add("region", ChunkKey::new(0, 0), &chunk(0))
            .unwrap();
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            1,
            "only the temporary file"
        );
        drop(writer);
        assert!(!target.exists());
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            0,
            "the temporary file is removed"
        );
    }

    #[test]
    fn a_chunk_is_written_once_per_stream() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("dup.awf");
        let mut writer = WorldFile::writer(&target, Codec::DEFAULT).unwrap();
        writer
            .add("region", ChunkKey::new(0, 0), &chunk(0))
            .unwrap();
        writer
            .add("region", ChunkKey::new(0, 0), &chunk(1))
            .unwrap();
        let refused = writer.finish(&metadata([]));
        assert!(
            matches!(&refused, Err(failed) if failed.is_io()),
            "{refused:?}"
        );
        assert!(!target.exists());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);

        assert!(WorldFile::looks_like(b"AWFW"));
        assert!(!WorldFile::looks_like(&[0xB1, 0x0B, 0x0D, 0]));
        assert!(!WorldFile::looks_like(b"AWF"));
    }
}
