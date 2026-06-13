// SPDX-License-Identifier: AGPL-3.0-only
//! Strongly-typed identifiers used throughout the storage engine.
//!
//! Wrapping the raw integers in newtypes keeps an LSN from being mistaken for a
//! collection id (or a row, or a segment) at a call site — a cheap, compile-time
//! guard for a component where mixing them up corrupts data.

use serde::{Deserialize, Serialize};

/// A log sequence number: a monotonically increasing id assigned to every WAL
/// record. LSNs totally order all mutations and anchor checkpointing and
/// recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Lsn(pub u64);

impl Lsn {
    /// The zero LSN — precedes every real record and is the initial checkpoint
    /// floor of a fresh store.
    pub const ZERO: Lsn = Lsn(0);

    /// The next LSN in sequence.
    #[must_use]
    pub const fn next(self) -> Lsn {
        Lsn(self.0 + 1)
    }

    /// The underlying integer value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A collection identifier, assigned monotonically by the catalog and stable for
/// the life of the collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CollectionId(pub u64);

impl CollectionId {
    /// The underlying integer value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CollectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsn_orders_and_increments() {
        assert_eq!(Lsn::ZERO.value(), 0);
        assert_eq!(Lsn::ZERO.next(), Lsn(1));
        assert!(Lsn(1) < Lsn(2));
        assert_eq!(Lsn(41).next().value(), 42);
    }

    #[test]
    fn ids_display() {
        assert_eq!(Lsn(7).to_string(), "7");
        assert_eq!(CollectionId(3).to_string(), "3");
        assert_eq!(CollectionId(3).value(), 3);
    }
}
