//! The deflate codec of objects and version 1 images.
//!
//! Both sides write a zlib stream at the default level. Two zlib implementations do not produce
//! the same compressed bytes, and nothing depends on that: an object is named by the hash of its
//! uncompressed bytes, and an image records each chunk's stored length.

use std::fmt::Display;
use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::{Compression, Decompress, FlushDecompress, Status};

use crate::error::{AwfError, Result};

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
    fn bytes_that_are_not_deflate_are_corrupt() {
        assert_eq!(
            corrupt_message(inflate(&[1, 2, 3], 3, &"object ab")),
            "object ab is not valid deflate data"
        );
    }
}
