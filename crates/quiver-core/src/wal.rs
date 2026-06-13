// SPDX-License-Identifier: AGPL-3.0-only
//! The write-ahead log: the durability anchor of the storage engine.
//!
//! Every mutation is appended to the WAL and `fsync`'d before it is acknowledged
//! (ADR-0005), so an acknowledged write survives `kill -9` and power loss. The
//! log is a sequence of length-prefixed, CRC32C-framed records, each carrying a
//! monotonic [`Lsn`]. A torn trailing record — the signature of a crash mid
//! append — fails its length or CRC check and is discarded on recovery; it was
//! never acknowledged.
//!
//! Recovery uses *point-in-time* semantics: it stops at the first invalid frame
//! and treats everything after it as never-committed. Because the log is append
//! only and each record is `fsync`'d before acknowledgement, the only place an
//! invalid frame can legitimately appear is the tail.
//!
//! Each record's bytes pass through a [`PageCodec`] before framing, so when
//! encryption-at-rest is enabled the AEAD codec seals every record and the log
//! holds no plaintext user data; under the plaintext codec the bytes are written
//! verbatim. The frame CRC is computed over the on-disk (sealed) bytes, so a
//! torn or bit-rotted tail is still detected without a key, and the AEAD tag
//! additionally authenticates each record (a wrong key or tampering on an
//! otherwise-intact frame is a hard error, never silently dropped).
//!
//! File layout (little-endian):
//!
//! ```text
//! 0  magic:u32  4  format_ver:u16  6  _pad:u16  8  base_lsn:u64   (16-byte header)
//! 16 frame[0] | frame[1] | ...
//!
//! frame: len:u32 | crc32c:u32 | record: codec.seal_record(postcard(WalEntry)) [len bytes]
//! ```

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ids::{CollectionId, Lsn};
use crate::page::PageCodec;

/// Magic identifying a WAL segment file (`b"QVWL"`, little-endian).
pub const WAL_MAGIC: u32 = u32::from_le_bytes(*b"QVWL");
/// Current WAL format version.
pub const WAL_FORMAT_VERSION: u16 = 1;

const WAL_FILE_HEADER_SIZE: usize = 16;
const FRAME_PREFIX_SIZE: usize = 8; // len:u32 + crc32c:u32
/// Hard cap on a single record's encoded size, so a corrupt length field cannot
/// trigger a huge allocation during recovery.
pub const MAX_RECORD_BYTES: u32 = 64 * 1024 * 1024;

/// A single logical mutation recorded in the WAL.
///
/// The vector and payload are stored as opaque bytes: the engine validates and
/// interprets them (the descriptor fixes the vector dtype/dim; payloads are
/// validated JSON), keeping the log a dumb, stable durability primitive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalOp {
    /// Create a collection with the given id, name, and postcard-encoded
    /// descriptor.
    CreateCollection {
        /// Identifier assigned to the new collection.
        collection_id: CollectionId,
        /// Human-readable collection name, unique within the store.
        name: String,
        /// Postcard-encoded collection descriptor (schema, dim, dtype, metric).
        descriptor: Vec<u8>,
    },
    /// Drop a collection and all of its data.
    DropCollection {
        /// Identifier of the collection to drop.
        collection_id: CollectionId,
    },
    /// Insert or replace a point.
    Upsert {
        /// Owning collection.
        collection_id: CollectionId,
        /// Caller-supplied external identifier.
        external_id: String,
        /// Raw little-endian vector element bytes (dtype per the descriptor).
        vector: Vec<u8>,
        /// Opaque, pre-validated payload bytes (UTF-8 JSON in Phase 1).
        payload: Vec<u8>,
    },
    /// Delete a point by external id.
    Delete {
        /// Owning collection.
        collection_id: CollectionId,
        /// External identifier to delete.
        external_id: String,
    },
    /// Record that state up to `last_checkpointed_lsn` is durable in segments
    /// referenced by manifest version `manifest_version`.
    Checkpoint {
        /// Highest LSN now captured in sealed segments.
        last_checkpointed_lsn: Lsn,
        /// Manifest version that references those segments.
        manifest_version: u64,
    },
}

/// A WAL record: a monotonic LSN paired with the operation it commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalEntry {
    /// Monotonic log sequence number identifying this record.
    pub lsn: Lsn,
    /// The mutation committed at `lsn`.
    pub op: WalOp,
}

/// Appends records to a WAL segment and controls the `fsync` durability policy.
///
/// LSNs are assigned by the caller (the engine owns the global counter); the
/// writer only frames, appends, and syncs.
#[derive(Debug)]
pub struct WalWriter {
    file: File,
    path: PathBuf,
    unsynced: bool,
}

impl WalWriter {
    /// Create a new WAL segment file and write its header. Fails if the file
    /// already exists.
    pub fn create(path: &Path, base_lsn: Lsn) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| CoreError::io(path, e))?;
        let mut hdr = [0u8; WAL_FILE_HEADER_SIZE];
        hdr[0..4].copy_from_slice(&WAL_MAGIC.to_le_bytes());
        hdr[4..6].copy_from_slice(&WAL_FORMAT_VERSION.to_le_bytes());
        // Bytes 6..8 are reserved padding (zero).
        hdr[8..16].copy_from_slice(&base_lsn.value().to_le_bytes());
        file.write_all(&hdr).map_err(|e| CoreError::io(path, e))?;
        file.sync_data().map_err(|e| CoreError::io(path, e))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            unsynced: false,
        })
    }

    /// Open an existing WAL segment for appending, validating its header. New
    /// records are written at the end of the file.
    pub fn open_append(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .map_err(|e| CoreError::io(path, e))?;
        let mut hdr = [0u8; WAL_FILE_HEADER_SIZE];
        file.read_exact(&mut hdr)
            .map_err(|e| CoreError::io(path, e))?;
        let magic = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        if magic != WAL_MAGIC {
            return Err(CoreError::BadMagic {
                expected: WAL_MAGIC,
                found: magic,
            });
        }
        let ver = u16::from_le_bytes([hdr[4], hdr[5]]);
        if ver != WAL_FORMAT_VERSION {
            return Err(CoreError::UnsupportedVersion {
                found: ver,
                supported: WAL_FORMAT_VERSION,
            });
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            unsynced: false,
        })
    }

    /// Frame and append a record, sealing its bytes with `codec` first (so an
    /// encrypting codec leaves no plaintext in the log). Does not `fsync`; the
    /// record is durable only after a subsequent [`WalWriter::sync`].
    pub fn append(&mut self, codec: &dyn PageCodec, entry: &WalEntry) -> Result<()> {
        let plaintext = postcard::to_allocvec(entry)?;
        let sealed = codec.seal_record(&plaintext)?;
        let len = u32::try_from(sealed.len())
            .map_err(|_| CoreError::TooLarge(format!("wal record {} bytes", sealed.len())))?;
        if len > MAX_RECORD_BYTES {
            return Err(CoreError::TooLarge(format!(
                "wal record {len} bytes exceeds cap {MAX_RECORD_BYTES}"
            )));
        }
        // The CRC covers the on-disk (sealed) bytes, so a torn or bit-rotted
        // tail is detected on recovery without needing the key.
        let crc = crc32c::crc32c(&sealed);
        let mut frame = Vec::with_capacity(FRAME_PREFIX_SIZE + sealed.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&crc.to_le_bytes());
        frame.extend_from_slice(&sealed);
        self.file
            .write_all(&frame)
            .map_err(|e| CoreError::io(&self.path, e))?;
        self.unsynced = true;
        Ok(())
    }

    /// Flush and `fsync` the segment, making every appended record durable.
    pub fn sync(&mut self) -> Result<()> {
        if self.unsynced {
            self.file
                .sync_data()
                .map_err(|e| CoreError::io(&self.path, e))?;
            self.unsynced = false;
        }
        Ok(())
    }

    /// Append a record and immediately `fsync` — strict per-commit durability.
    pub fn append_sync(&mut self, codec: &dyn PageCodec, entry: &WalEntry) -> Result<()> {
        self.append(codec, entry)?;
        self.sync()
    }
}

/// The result of replaying a WAL segment to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalReplay {
    /// The intact records, in log order.
    pub entries: Vec<WalEntry>,
    /// Byte offset of the torn trailing record, if the log ended on one rather
    /// than cleanly at a frame boundary.
    pub torn_at: Option<u64>,
    /// The segment's base LSN, from its header.
    pub base_lsn: Lsn,
}

impl WalReplay {
    /// The highest LSN among the recovered records, if any.
    #[must_use]
    pub fn max_lsn(&self) -> Option<Lsn> {
        self.entries.iter().map(|e| e.lsn).max()
    }
}

enum ReadOutcome {
    Full,
    Partial,
    Eof,
}

fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<ReadOutcome> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Ok(if filled == 0 {
                    ReadOutcome::Eof
                } else {
                    ReadOutcome::Partial
                });
            }
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(CoreError::BareIo(e)),
        }
    }
    Ok(ReadOutcome::Full)
}

/// Read every intact record from a WAL segment, stopping cleanly at a torn
/// trailing record. Each record is opened with `codec` (the identity transform
/// under the plaintext codec; authenticated decryption under the AEAD codec).
/// Errors on a structurally invalid header, an underlying I/O failure, or a
/// frame that is intact on disk yet fails authentication (a wrong key or
/// tampering) — a torn tail is a normal, expected outcome reported via
/// [`WalReplay::torn_at`].
pub fn read_all(path: &Path, codec: &dyn PageCodec) -> Result<WalReplay> {
    let file = File::open(path).map_err(|e| CoreError::io(path, e))?;
    let file_len = file.metadata().map_err(|e| CoreError::io(path, e))?.len();
    let mut reader = BufReader::new(file);

    let mut hdr = [0u8; WAL_FILE_HEADER_SIZE];
    match read_full(&mut reader, &mut hdr)? {
        ReadOutcome::Full => {}
        _ => {
            return Err(CoreError::MalformedPage(format!(
                "wal {} is shorter than its header",
                path.display()
            )));
        }
    }
    let magic = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if magic != WAL_MAGIC {
        return Err(CoreError::BadMagic {
            expected: WAL_MAGIC,
            found: magic,
        });
    }
    let ver = u16::from_le_bytes([hdr[4], hdr[5]]);
    if ver != WAL_FORMAT_VERSION {
        return Err(CoreError::UnsupportedVersion {
            found: ver,
            supported: WAL_FORMAT_VERSION,
        });
    }
    let base_lsn = Lsn(u64::from_le_bytes([
        hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
    ]));

    let mut entries = Vec::new();
    let mut offset = WAL_FILE_HEADER_SIZE as u64;
    let mut torn_at = None;
    loop {
        let mut prefix = [0u8; FRAME_PREFIX_SIZE];
        match read_full(&mut reader, &mut prefix)? {
            ReadOutcome::Eof => break, // clean end of log
            ReadOutcome::Partial => {
                torn_at = Some(offset);
                break;
            }
            ReadOutcome::Full => {}
        }
        let len = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
        let crc = u32::from_le_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]);
        let frame_end = offset
            .checked_add(FRAME_PREFIX_SIZE as u64)
            .and_then(|o| o.checked_add(u64::from(len)));
        match frame_end {
            Some(end) if len != 0 && len <= MAX_RECORD_BYTES && end <= file_len => {}
            _ => {
                // Implausible length or a record that runs past EOF: a torn tail.
                torn_at = Some(offset);
                break;
            }
        }
        let mut buf = vec![0u8; len as usize];
        match read_full(&mut reader, &mut buf)? {
            ReadOutcome::Full => {}
            _ => {
                torn_at = Some(offset);
                break;
            }
        }
        if crc32c::crc32c(&buf) != crc {
            torn_at = Some(offset);
            break;
        }
        // The frame is intact on disk (the CRC covers the sealed bytes). Open it:
        // under the plaintext codec this is the identity, and under the AEAD codec
        // a failure means a wrong key or tampering on an otherwise-complete,
        // acknowledged record — a hard error, not a recoverable torn tail.
        let plaintext = codec.open_record(&buf)?;
        match postcard::from_bytes::<WalEntry>(&plaintext) {
            Ok(entry) => {
                entries.push(entry);
                offset += FRAME_PREFIX_SIZE as u64 + u64::from(len);
            }
            Err(_) => {
                // Authenticated bytes that nonetheless do not decode: torn.
                torn_at = Some(offset);
                break;
            }
        }
    }
    Ok(WalReplay {
        entries,
        torn_at,
        base_lsn,
    })
}

#[cfg(test)]
mod tests {
    // `super::*` also re-exports the parent module's imports (`OpenOptions`,
    // `Write`, `Path`, `CoreError`, `CollectionId`, `Lsn`, `PageCodec`), so they
    // need no separate `use` here. The concrete plaintext codec is a sibling type
    // the parent does not import, so bring it in explicitly.
    use super::*;
    use crate::page::PlainCodec;
    use proptest::prelude::*;

    fn sample_ops() -> Vec<WalOp> {
        vec![
            WalOp::CreateCollection {
                collection_id: CollectionId(1),
                name: "alpha".into(),
                descriptor: vec![1, 2, 3, 4],
            },
            WalOp::Upsert {
                collection_id: CollectionId(1),
                external_id: "alpha".into(),
                vector: vec![0u8; 32],
                payload: br#"{"k":"v"}"#.to_vec(),
            },
            WalOp::Delete {
                collection_id: CollectionId(1),
                external_id: "alpha".into(),
            },
            WalOp::Checkpoint {
                last_checkpointed_lsn: Lsn(2),
                manifest_version: 5,
            },
            WalOp::DropCollection {
                collection_id: CollectionId(1),
            },
        ]
    }

    fn entries_from(ops: &[WalOp]) -> Vec<WalEntry> {
        ops.iter()
            .enumerate()
            .map(|(i, op)| WalEntry {
                lsn: Lsn(i as u64 + 1),
                op: op.clone(),
            })
            .collect()
    }

    fn write_log(path: &Path, entries: &[WalEntry]) {
        let mut w = WalWriter::create(path, Lsn(1)).unwrap();
        for e in entries {
            w.append(&PlainCodec, e).unwrap();
        }
        w.sync().unwrap();
    }

    #[test]
    fn roundtrips_every_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-1.log");
        let entries = entries_from(&sample_ops());
        write_log(&path, &entries);

        let replay = read_all(&path, &PlainCodec).unwrap();
        assert_eq!(replay.entries, entries);
        assert_eq!(replay.torn_at, None);
        assert_eq!(replay.base_lsn, Lsn(1));
        assert_eq!(replay.max_lsn(), Some(Lsn(entries.len() as u64)));
    }

    #[test]
    fn empty_log_replays_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-1.log");
        let _w = WalWriter::create(&path, Lsn(10)).unwrap();
        let replay = read_all(&path, &PlainCodec).unwrap();
        assert!(replay.entries.is_empty());
        assert_eq!(replay.torn_at, None);
        assert_eq!(replay.base_lsn, Lsn(10));
        assert_eq!(replay.max_lsn(), None);
    }

    #[test]
    fn reopen_and_append_continues_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-1.log");
        let entries = entries_from(&sample_ops());
        {
            let mut w = WalWriter::create(&path, Lsn(1)).unwrap();
            w.append_sync(&PlainCodec, &entries[0]).unwrap();
            w.append_sync(&PlainCodec, &entries[1]).unwrap();
        }
        {
            let mut w = WalWriter::open_append(&path).unwrap();
            for e in &entries[2..] {
                w.append(&PlainCodec, e).unwrap();
            }
            w.sync().unwrap();
        }
        let replay = read_all(&path, &PlainCodec).unwrap();
        assert_eq!(replay.entries, entries);
        assert_eq!(replay.torn_at, None);
    }

    #[test]
    fn torn_prefix_at_tail_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-1.log");
        let entries = entries_from(&sample_ops());
        write_log(&path, &entries);
        let clean_len = std::fs::metadata(&path).unwrap().len();
        // Append a partial 8-byte frame prefix (only 3 bytes of it).
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0xFF, 0xFF, 0xFF]).unwrap();
            f.sync_data().unwrap();
        }
        let replay = read_all(&path, &PlainCodec).unwrap();
        assert_eq!(replay.entries, entries);
        assert_eq!(replay.torn_at, Some(clean_len));
    }

    #[test]
    fn torn_payload_at_tail_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-1.log");
        let entries = entries_from(&sample_ops());
        write_log(&path, &entries);
        let clean_len = std::fs::metadata(&path).unwrap().len();
        // Append a frame claiming 100 payload bytes but supply only a few.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&100u32.to_le_bytes()).unwrap();
            f.write_all(&0u32.to_le_bytes()).unwrap();
            f.write_all(&[1, 2, 3]).unwrap();
            f.sync_data().unwrap();
        }
        let replay = read_all(&path, &PlainCodec).unwrap();
        assert_eq!(replay.entries, entries);
        assert_eq!(replay.torn_at, Some(clean_len));
    }

    #[test]
    fn corruption_stops_recovery_point_in_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-1.log");
        let entries = entries_from(&sample_ops());
        write_log(&path, &entries);

        // Corrupt a byte inside the *second* record's payload region. The first
        // record must still be recovered; the second and everything after it is
        // treated as a torn tail (point-in-time recovery).
        let len0 = postcard::to_allocvec(&entries[0]).unwrap().len() as u64;
        let second_frame_offset = WAL_FILE_HEADER_SIZE as u64 + FRAME_PREFIX_SIZE as u64 + len0;
        let corrupt_at = second_frame_offset + FRAME_PREFIX_SIZE as u64 + 1;

        let mut bytes = std::fs::read(&path).unwrap();
        bytes[corrupt_at as usize] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let replay = read_all(&path, &PlainCodec).unwrap();
        assert_eq!(replay.entries, vec![entries[0].clone()]);
        assert_eq!(replay.torn_at, Some(second_frame_offset));
    }

    #[test]
    fn foreign_file_is_rejected_by_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-1.log");
        std::fs::write(&path, vec![0u8; WAL_FILE_HEADER_SIZE + 4]).unwrap();
        assert!(matches!(
            read_all(&path, &PlainCodec),
            Err(CoreError::BadMagic { .. })
        ));
    }

    proptest! {
        #[test]
        fn entries_roundtrip(seeds in proptest::collection::vec(0u8..5, 0..40)) {
            let ops = sample_ops();
            let entries: Vec<WalEntry> = seeds
                .iter()
                .enumerate()
                .map(|(i, &s)| WalEntry { lsn: Lsn(i as u64 + 1), op: ops[s as usize].clone() })
                .collect();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("wal.log");
            write_log(&path, &entries);
            let replay = read_all(&path, &PlainCodec).unwrap();
            prop_assert_eq!(replay.entries, entries);
            prop_assert_eq!(replay.torn_at, None);
        }

        #[test]
        fn truncation_yields_a_clean_prefix(
            seeds in proptest::collection::vec(0u8..5, 1..20),
            cut_num in 0u64..1000,
        ) {
            let ops = sample_ops();
            let entries: Vec<WalEntry> = seeds
                .iter()
                .enumerate()
                .map(|(i, &s)| WalEntry { lsn: Lsn(i as u64 + 1), op: ops[s as usize].clone() })
                .collect();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("wal.log");
            write_log(&path, &entries);

            let full = std::fs::metadata(&path).unwrap().len();
            // Compute the byte boundary at the end of each record's frame.
            let mut frame_ends = Vec::new();
            let mut off = WAL_FILE_HEADER_SIZE as u64;
            for e in &entries {
                off += FRAME_PREFIX_SIZE as u64 + postcard::to_allocvec(e).unwrap().len() as u64;
                frame_ends.push(off);
            }
            // Truncate somewhere in [header, full].
            let cut = WAL_FILE_HEADER_SIZE as u64
                + (cut_num % (full - WAL_FILE_HEADER_SIZE as u64 + 1));
            let f = OpenOptions::new().write(true).open(&path).unwrap();
            f.set_len(cut).unwrap();
            drop(f);

            let replay = read_all(&path, &PlainCodec).unwrap();
            let survivors = frame_ends.iter().filter(|&&end| end <= cut).count();
            prop_assert_eq!(replay.entries.as_slice(), &entries[..survivors]);
            // A clean cut at a frame boundary has no torn tail; otherwise it does.
            let clean = cut == WAL_FILE_HEADER_SIZE as u64 || frame_ends.contains(&cut);
            prop_assert_eq!(replay.torn_at.is_none(), clean);
        }
    }
}
