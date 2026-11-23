use std::io;

use thiserror::Error;

/// What a store operation fails with.
#[derive(Debug, Error)]
pub enum AwfError {
    /// Stored chunk data does not match what its file or object says it is.
    #[error("{0}")]
    Corrupt(String),
    /// Committed generation data does not match its manifest or the `CURRENT` pointer.
    #[error("{0}")]
    CorruptGeneration(String),
    /// The caller passed something the store refuses.
    #[error("{0}")]
    InvalidArgument(String),
    /// The store's role or state does not allow the call.
    #[error("{0}")]
    InvalidState(String),
    /// A disk call failed while the store was doing what `context` says.
    #[error("{context}")]
    IoContext {
        /// What the store was doing, and what it means for the commit.
        context: String,
        /// The failure underneath.
        #[source]
        source: io::Error,
    },
    /// A disk call failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl AwfError {
    /// Whether this is damaged data or a failed disk call rather than a refused call.
    ///
    /// The Java original throws an `IOException` for the first group and an unchecked exception
    /// for the second. Code that skips a damaged generation skips only the first group.
    #[must_use]
    pub const fn is_io(&self) -> bool {
        !matches!(self, Self::InvalidArgument(_) | Self::InvalidState(_))
    }
}

/// The result of a store operation.
pub type Result<T> = std::result::Result<T, AwfError>;
