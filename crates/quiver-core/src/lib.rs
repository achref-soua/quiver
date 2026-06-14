// SPDX-License-Identifier: AGPL-3.0-only
//! Quiver's storage engine: from-scratch, durable, crash-safe, encrypted-at-rest
//! storage built on 16 KiB pages, a write-ahead log, and a versioned manifest.
//!
//! This crate owns all durable state (ADR-0004 / ADR-0005) and is deliberately
//! built without an embedded KV/DB engine. Phase 1 ships the foundational
//! primitives; the columnar segment layout, the store-level engine, and crash
//! recovery build on top of them.
//!
//! - [`page`] — the 16 KiB page: the unit of checksum, encryption, and I/O,
//!   with a swappable [`page::PageCodec`] (plaintext now, AEAD with
//!   `quiver-crypto`).
//! - [`wal`] — the write-ahead log: the durability anchor. An acknowledged write
//!   is `fsync`'d to the log first, so it survives `kill -9`.
//! - [`manifest`] — the versioned catalog, made current via an atomic
//!   write-new + fsync + rename of `CURRENT`.
//! - [`store`] — the [`store::Store`] engine: the write path, checkpointing, and
//!   crash recovery that compose the primitives above, over internal sealed
//!   segments.
//! - [`descriptor`] — collection schema; [`ids`] — typed [`ids::Lsn`] /
//!   [`ids::CollectionId`].

mod blockfile;
pub mod descriptor;
pub mod error;
pub mod ids;
pub mod keyring;
pub mod manifest;
pub mod page;
mod paged;
pub mod sec;
mod segment;
pub mod store;
pub mod wal;

pub use descriptor::{
    Descriptor, DistanceMetric, Dtype, FieldType, FilterableField, IndexKind, IndexSpec,
};
pub use error::{CoreError, Result};
pub use ids::{CollectionId, Lsn};
pub use keyring::{KeyRing, SingleCodecKeyRing};
pub use sec::{SecPredicate, SecValue};
pub use store::{Record, Store};
