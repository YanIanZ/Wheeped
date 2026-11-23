//! Chunk codecs.
//!
//! Objects and version 1 images are deflate only. A version 2 world file records a codec per
//! chunk: none, deflate or zstd.
//!
//! Both sides write a zlib stream at the default level, and a standard zstd frame. Two
//! implementations of either do not produce the same compressed bytes, and nothing depends on
//! that: an object is named by the hash of its uncompressed bytes, and a file records each chunk's
//! stored length.

use std::fmt::{self, Display};
use std::io::{Read, Write};

use flate2::write::ZlibEncoder;
use flate2::{Compression, Decompress, FlushDecompress, Status};
use ruzstd::decoding::StreamingDecoder;
use ruzstd::encoding::{CompressionLevel, compress_to_vec};

use crate::error::{AwfError, Result};

/// How a chunk of a version 2 world file is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// The raw bytes.
    None,
    /// A zlib stream.
    Deflate,
    /// A zstd frame.
    Zstd,
}

impl Codec {
    /// The codec a writer uses when it is not told one: zstd, as the Java writer picks whenever
    /// its server has zstd.
    pub const DEFAULT: Self = Self::Zstd;

    /// The codec's id as stored in a file.
    #[must_use]
    pub const fn id(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Deflate => 1,
            Self::Zstd => 2,
        }
    }

    /// The codec with this id, or `None` for an id this build does not know.
    #[must_use]
    pub const fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::None),
            1 => Some(Self::Deflate),
            2 => Some(Self::Zstd),
            _ => None,
        }
    }

    /// Compresses `raw` with this codec.
    pub fn compress(self, raw: &[u8]) -> Result<Vec<u8>> {
        match self {
            Self::None => Ok(raw.to_vec()),
            Self::Deflate => deflate(raw),
            Self::Zstd => Ok(compress_to_vec(raw, CompressionLevel::Fastest)),
        }
    }

    /// Restores a chunk that must be exactly `raw_length` bytes. `what` names it in the error.
    pub fn decompress(
        self,
        stored: &[u8],
        raw_length: usize,
        what: &dyn Display,
    ) -> Result<Vec<u8>> {
        match self {
            Self::None if stored.len() == raw_length => Ok(stored.to_vec()),
            Self::None => Err(AwfError::Corrupt(format!("{what} has the wrong length"))),
            Self::Deflate => inflate(stored, raw_length, what),
            Self::Zstd => unzstd(stored, raw_length, what),
        }
    }
}

impl Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Deflate => "deflate",
            Self::Zstd => "zstd",
        })
    }
}

/// Decodes a zstd frame that must produce exactly `raw_length` bytes.
fn unzstd(stored: &[u8], raw_length: usize, what: &dyn Display) -> Result<Vec<u8>> {
    let corrupt =
        |why: &dyn Display| AwfError::Corrupt(format!("{what} is corrupt zstd data: {why}"));
    let decoder = StreamingDecoder::new(stored).map_err(|failed| corrupt(&failed))?;
    let mut raw = Vec::with_capacity(raw_length);
    // One byte past the declared size is enough to know the frame is longer, without holding it.
    decoder
        .take(raw_length as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|failed| corrupt(&failed))?;
    if raw.len() != raw_length {
        return Err(AwfError::Corrupt(format!(
            "{what} does not decode to its declared size of {raw_length} bytes"
        )));
    }
    Ok(raw)
}

/// Compresses `raw` as a zlib stream.
pub fn deflate(raw: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(
        Vec::with_capacity((raw.len() / 2).max(64)),
        Compression::default(),
    );
    encoder.write_all(raw)?;
    Ok(encoder.finish()?)
}

/// Inflates a zlib stream that must produce exactly `raw_length` bytes.
///
/// `what` names the chunk or object in the error. A stream that is damaged, ends early, or
/// produces fewer or more bytes than declared is corrupt, never returned.
pub fn inflate(stored: &[u8], raw_length: usize, what: &dyn Display) -> Result<Vec<u8>> {
    let mut inflater = Decompress::new(true);
    let mut raw = vec![0u8; raw_length];
    // Once the declared size is reached, any further output means the stream is longer.
    let mut probe = [0u8; 1];
    let mut consumed = 0usize;
    let mut filled = 0usize;
    loop {
        let in_before = inflater.total_in();
        let out_before = inflater.total_out();
        let within_declared_size = filled < raw_length;
        let output = if within_declared_size {
            &mut raw[filled..]
        } else {
            &mut probe[..]
        };
        let status = inflater
            .decompress(&stored[consumed..], output, FlushDecompress::None)
            .map_err(|_| AwfError::Corrupt(format!("{what} is not valid deflate data")))?;
        let read = (inflater.total_in() - in_before) as usize;
        let produced = (inflater.total_out() - out_before) as usize;
        consumed += read;
        if within_declared_size {
            filled += produced;
        } else if produced > 0 {
            return Err(AwfError::Corrupt(format!(
                "{what} inflates past its declared size"
            )));
        }
        if status == Status::StreamEnd {
            break;
        }
        if read == 0 && produced == 0 {
            return Err(AwfError::Corrupt(format!(
                "{what} is a truncated deflate stream"
            )));
        }
    }
    if filled != raw_length {
        return Err(AwfError::Corrupt(format!(
            "{what} does not inflate to its declared size"
        )));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corrupt_message(result: Result<Vec<u8>>) -> String {
        match result {
            Err(AwfError::Corrupt(message)) => message,
            other => panic!("expected corrupt data, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_including_the_empty_chunk() {
        let large = vec![7u8; 100_000];
        for raw in [&b""[..], b"a", &large] {
            let stored = deflate(raw).unwrap();
            assert_eq!(inflate(&stored, raw.len(), &"chunk 0,0").unwrap(), raw);
        }
    }

    #[test]
    fn a_stream_longer_than_declared_is_corrupt() {
        let stored = deflate(b"twelve bytes").unwrap();
        assert_eq!(
            corrupt_message(inflate(&stored, 5, &"chunk 1,2")),
            "chunk 1,2 inflates past its declared size"
        );
        assert_eq!(
            corrupt_message(inflate(&stored, 0, &"chunk 1,2")),
            "chunk 1,2 inflates past its declared size"
        );
    }

    #[test]
    fn a_stream_shorter_than_declared_is_corrupt() {
        let stored = deflate(b"twelve bytes").unwrap();
        assert_eq!(
            corrupt_message(inflate(&stored, 13, &"chunk 1,2")),
            "chunk 1,2 does not inflate to its declared size"
        );
    }

    #[test]
    fn a_truncated_stream_is_corrupt() {
        let stored = deflate(&[3u8; 4096]).unwrap();
        assert_eq!(
            corrupt_message(inflate(&stored[..stored.len() - 4], 4096, &"chunk 1,2")),
            "chunk 1,2 is a truncated deflate stream"
        );
        assert_eq!(
            corrupt_message(inflate(&[], 1, &"chunk 1,2")),
            "chunk 1,2 is a truncated deflate stream"
        );
    }

    #[test]
    fn every_codec_round_trips_and_checks_the_declared_size() {
        let raw = b"chunk bytes ".repeat(500);
        for codec in [Codec::None, Codec::Deflate, Codec::Zstd] {
            assert_eq!(Codec::from_id(codec.id()), Some(codec));
            let stored = codec.compress(&raw).unwrap();
            assert_eq!(
                codec.decompress(&stored, raw.len(), &"chunk 0,0").unwrap(),
                raw,
                "{codec}"
            );
            assert!(
                matches!(
                    codec.decompress(&stored, raw.len() - 1, &"chunk 0,0"),
                    Err(AwfError::Corrupt(_))
                ),
                "{codec}: longer than declared"
            );
            assert!(
                matches!(
                    codec.decompress(&stored, raw.len() + 1, &"chunk 0,0"),
                    Err(AwfError::Corrupt(_))
                ),
                "{codec}: shorter than declared"
            );
            assert_eq!(
                codec
                    .decompress(&codec.compress(b"").unwrap(), 0, &"chunk 0,0")
                    .unwrap(),
                b"",
                "{codec}"
            );
        }
        assert!(Codec::Zstd.compress(&raw).unwrap().len() < raw.len() / 2);
        assert_eq!(Codec::from_id(3), None);
        assert!(matches!(
            Codec::Zstd.decompress(&[1, 2, 3, 4, 5, 6, 7, 8], 4, &"chunk 0,0"),
            Err(AwfError::Corrupt(_))
        ));
    }

    #[test]
    fn bytes_that_are_not_deflate_are_corrupt() {
        assert_eq!(
            corrupt_message(inflate(&[1, 2, 3], 3, &"object ab")),
            "object ab is not valid deflate data"
        );
    }
}
