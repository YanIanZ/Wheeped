use std::fmt;

/// The roles a world can have.
///
/// A store opened with a role that does not accept commits changes nothing on disk, so it can read
/// a directory a live writer is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorldRole {
    /// An ordinary world whose storage is opened for writing.
    Vanilla,
    /// Registered from header, metadata and chunk index; chunks materialise on demand.
    Virtual,
    /// Immutable source shared by instances; never written after creation.
    Template,
    /// Copy-on-write child of a template; a chunk becomes instance-owned on first mutation.
    Instance,
    /// Loaded for reading only.
    ReadOnly,
    /// Mutable but never persisted.
    Temporary,
}

impl WorldRole {
    /// Every role, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::Vanilla,
        Self::Virtual,
        Self::Template,
        Self::Instance,
        Self::ReadOnly,
        Self::Temporary,
    ];

    /// Whether gameplay may change this world's chunks.
    #[must_use]
    pub const fn mutable(self) -> bool {
        matches!(
            self,
            Self::Vanilla | Self::Virtual | Self::Instance | Self::Temporary
        )
    }

    /// Whether this world's state is ever committed to a backend.
    #[must_use]
    pub const fn persistent(self) -> bool {
        !matches!(self, Self::Temporary)
    }

    /// Whether a commit of a new generation is permitted for this role.
    #[must_use]
    pub const fn accepts_commits(self) -> bool {
        self.mutable() && self.persistent()
    }

    /// The role's name as the Java original and its configuration spell it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Vanilla => "VANILLA",
            Self::Virtual => "VIRTUAL",
            Self::Template => "TEMPLATE",
            Self::Instance => "INSTANCE",
            Self::ReadOnly => "READ_ONLY",
            Self::Temporary => "TEMPORARY",
        }
    }
}

impl fmt::Display for WorldRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How a save commits. Every mode commits through the same atomic generation sequence; they differ
/// in what is written and what is verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PersistenceMode {
    /// Every chunk's bytes are rewritten, whether or not they changed.
    Full,
    /// Only chunks whose bytes are not already stored are written; the rest are referenced.
    Incremental,
    /// As [`Self::Incremental`], then every object the new generation references is re-read and
    /// verified.
    Checkpoint,
    /// Commits are refused.
    ReadOnly,
}

impl PersistenceMode {
    /// The mode's name as the Java original and its configuration spell it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Full => "FULL",
            Self::Incremental => "INCREMENTAL",
            Self::Checkpoint => "CHECKPOINT",
            Self::ReadOnly => "READ_ONLY",
        }
    }
}

impl fmt::Display for PersistenceMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mutable_persistent_roles_accept_commits() {
        let accepting: Vec<WorldRole> = WorldRole::ALL
            .into_iter()
            .filter(|role| role.accepts_commits())
            .collect();
        assert_eq!(
            accepting,
            [WorldRole::Vanilla, WorldRole::Virtual, WorldRole::Instance]
        );
    }
}
