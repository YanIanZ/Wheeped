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
    /// A commit or a compaction refused because its world was fenced (discarded or closed) before
    /// it started. Nothing of it reached the store.
    #[error("{0}")]
    Fenced(String),
    /// A save refused because the world already has as many saves pending as it allows.
    #[error("{0}")]
    Rejected(String),
    /// A backend call or a commit stopped by the shutdown deadline.
    ///
    /// `request_sent` separates two outcomes that must never be confused. When it is `false`
    /// nothing reached the backend, so the operation certainly did not happen. When it is `true`
    /// the request, or part of it, may have reached the backend before it was cut off, so it may
    /// or may not have been applied: a caller reports that as unconfirmed, never as a failed
    /// commit.
    #[error("{message}")]
    DeadlineExceeded {
        /// What was stopped.
        message: String,
        /// Whether the request may have reached the backend, so its outcome is unknown.
        request_sent: bool,
    },
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
    /// for the second. Code that skips a damaged generation skips only the first group. A cut by
    /// the shutdown deadline is in the first group there too, and is told apart by its variant.
    #[must_use]
    pub const fn is_io(&self) -> bool {
        !matches!(
            self,
            Self::InvalidArgument(_) | Self::InvalidState(_) | Self::Fenced(_) | Self::Rejected(_)
        )
    }
}

/// The result of a store operation.
pub type Result<T> = std::result::Result<T, AwfError>;
