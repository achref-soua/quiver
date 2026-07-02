// SPDX-License-Identifier: AGPL-3.0-only
//! Durable Raft log store (ADR-0067, increment 4c).
//!
//! The vendored in-memory [`LogStore`](super::LogStore) (4a/4b) loses its log on
//! restart, so a crashed voter cannot rejoin safely — which is why no release
//! ships write HA until this lands. This is the crash-safe replacement: a
//! [`RaftLogStorage`] that mirrors the log in memory for fast reads and persists
//! every **safety-critical** mutation — the granted vote, and appended /
//! truncated / purged entries — to an append-only file, `fsync`ed **before** the
//! call returns. That is openraft's durability contract: a vote or log entry the
//! protocol believes is stored must survive a crash, or consensus can lose an
//! acknowledged write or admit two leaders.
//!
//! Same `kill -9` discipline as the engine WAL ([ADR-0005]): each record is
//! length-prefixed and `fsync`ed, and a **torn tail** record from a crash
//! mid-write is discarded on replay (the write was never acknowledged, so no
//! reader ever depended on it). The **committed index is deliberately not
//! persisted** — in Raft it is advisory and openraft re-establishes it from the
//! durable log and vote on restart — so the hot commit path pays no extra `fsync`.
//!
//! Crash safety is exercised end to end (`committed_log_survives_a_restart`): a
//! single-member group commits, the store is dropped and **reopened from disk**,
//! and the committed entries are still there.
//!
//! [ADR-0005]: ../../../../docs/adr/0005-durability-and-recovery.md

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use openraft::storage::{LogFlushed, RaftLogReader, RaftLogStorage};
use openraft::{Entry, LogId, LogState, RaftLogId, StorageError, StorageIOError, Vote};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::{NodeId, TypeConfig};

/// One durable, replayable mutation. Only the safety-critical Raft state — the
/// vote and the log — is recorded; the committed index is advisory (recomputed on
/// restart) and deliberately omitted.
#[derive(Serialize, Deserialize)]
enum Record {
    /// The granted vote (the core consensus safety primitive).
    Vote(Vote<NodeId>),
    /// A batch of appended log entries.
    Append(Vec<Entry<TypeConfig>>),
    /// Conflict resolution: drop all entries with index ≥ this.
    Truncate(u64),
    /// Compaction: drop all entries with index ≤ this id's index; advance
    /// last-purged.
    Purge(LogId<NodeId>),
}

/// The in-memory mirror, reconstructed from the durable log on open.
#[derive(Default)]
struct Mem {
    last_purged_log_id: Option<LogId<NodeId>>,
    log: BTreeMap<u64, Entry<TypeConfig>>,
    committed: Option<LogId<NodeId>>,
    vote: Option<Vote<NodeId>>,
}

impl Mem {
    fn apply(&mut self, rec: Record) {
        match rec {
            Record::Vote(v) => self.vote = Some(v),
            Record::Append(entries) => {
                for e in entries {
                    self.log.insert(e.get_log_id().index, e);
                }
            }
            Record::Truncate(index) => self.remove_range(index..),
            Record::Purge(log_id) => {
                self.remove_range(..=log_id.index);
                self.last_purged_log_id = Some(log_id);
            }
        }
    }

    fn remove_range<R: RangeBounds<u64>>(&mut self, range: R) {
        let keys: Vec<u64> = self.log.range(range).map(|(k, _)| *k).collect();
        for k in keys {
            self.log.remove(&k);
        }
    }
}

/// A crash-safe [`RaftLogStorage`] backed by an append-only file.
#[derive(Clone)]
pub struct DurableLogStore {
    mem: Arc<Mutex<Mem>>,
    file: Arc<StdMutex<File>>,
}

fn store_err(e: impl std::error::Error + 'static) -> StorageError<NodeId> {
    StorageIOError::write_logs(&e).into()
}

impl DurableLogStore {
    /// Open (creating if absent) the durable log under `dir`, replaying any
    /// existing records to reconstruct the log and vote. A torn tail record left
    /// by a crash mid-write is discarded.
    ///
    /// # Errors
    /// Propagates filesystem errors creating the directory or opening the file.
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("log");

        let mut mem = Mem::default();
        if let Ok(f) = File::open(&path) {
            let mut r = BufReader::new(f);
            loop {
                let mut len = [0u8; 4];
                if r.read_exact(&mut len).is_err() {
                    break; // clean EOF, or a torn length prefix
                }
                let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
                if r.read_exact(&mut buf).is_err() {
                    break; // torn record body — a crash mid-write
                }
                match postcard::from_bytes::<Record>(&buf) {
                    Ok(rec) => mem.apply(rec),
                    Err(_) => break, // unreadable tail — stop replay here
                }
            }
        }

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        // Persist the newly-created file's directory entry. `durable()` fdatasyncs
        // the file contents, but that does not make a fresh dirent durable, so a
        // power loss right after the first vote/append could otherwise lose the
        // `log` file entirely — double-voting or losing acknowledged entries.
        // Matches the engine's fsync_dir discipline (quiver-core paged.rs).
        File::open(dir)?.sync_all()?;
        Ok(Self {
            mem: Arc::new(Mutex::new(mem)),
            file: Arc::new(StdMutex::new(file)),
        })
    }

    // Encode, append, and `fsync` one record before returning — the durability
    // point. The blocking file IO is offloaded so it never stalls the async
    // runtime; openraft drives the storage methods sequentially (`&mut self`), so
    // records are written in call order.
    async fn durable(&self, record: Record) -> Result<(), StorageError<NodeId>> {
        let bytes = postcard::to_allocvec(&record).map_err(store_err)?;
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let mut f = file
                .lock()
                .map_err(|_| io::Error::other("raft log file mutex poisoned"))?;
            f.write_all(&(bytes.len() as u32).to_le_bytes())?;
            f.write_all(&bytes)?;
            f.sync_data()?;
            Ok(())
        })
        .await
        .map_err(store_err)?
        .map_err(store_err)
    }

    // Persist + mirror a batch of entries (the durable half of `append`, without
    // openraft's flush callback). Split out so the crash-recovery replay can be
    // property-tested directly — openraft's `LogFlushed` callback is `pub(crate)`
    // and cannot be constructed from a test.
    async fn append_entries(
        &self,
        entries: Vec<Entry<TypeConfig>>,
    ) -> Result<(), StorageError<NodeId>> {
        self.durable(Record::Append(entries.clone())).await?;
        let mut mem = self.mem.lock().await;
        for e in entries {
            mem.log.insert(e.get_log_id().index, e);
        }
        Ok(())
    }
}

impl RaftLogReader<TypeConfig> for DurableLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        let mem = self.mem.lock().await;
        Ok(mem.log.range(range).map(|(_, v)| v.clone()).collect())
    }
}

impl RaftLogStorage<TypeConfig> for DurableLogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        let mem = self.mem.lock().await;
        let last = mem.log.iter().next_back().map(|(_, e)| *e.get_log_id());
        let last_purged = mem.last_purged_log_id;
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id: last.or(last_purged),
        })
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<NodeId>>,
    ) -> Result<(), StorageError<NodeId>> {
        // Advisory only: openraft re-establishes the committed index from the
        // durable log + vote on restart, so this is kept in memory, not fsynced.
        self.mem.lock().await.committed = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        Ok(self.mem.lock().await.committed)
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        // Durable before the in-memory mirror: disk is the source of truth.
        self.durable(Record::Vote(*vote)).await?;
        self.mem.lock().await.vote = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        Ok(self.mem.lock().await.vote)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>>,
    {
        // Persist + fsync the batch, then mirror in memory, then signal the flush —
        // so a crash before the fsync leaves no entry openraft believes is durable.
        self.append_entries(entries.into_iter().collect()).await?;
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.durable(Record::Truncate(log_id.index)).await?;
        self.mem.lock().await.remove_range(log_id.index..);
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.durable(Record::Purge(log_id)).await?;
        let mut mem = self.mem.lock().await;
        mem.remove_range(..=log_id.index);
        mem.last_purged_log_id = Some(log_id);
        Ok(())
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use openraft::{BasicNode, Config, ServerState, Vote};
    use quiver_core::{CollectionId, WalOp};

    use super::super::{ApplyOp, NoNetwork, StateMachineStore};
    use super::*;

    struct NoopApplier;
    impl ApplyOp for NoopApplier {
        async fn apply(&self, _op: WalOp) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn del(i: u64) -> WalOp {
        WalOp::Delete {
            collection_id: CollectionId(1),
            external_id: format!("e{i}"),
        }
    }

    #[tokio::test]
    async fn vote_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut s = DurableLogStore::open(dir.path()).unwrap();
            s.save_vote(&Vote::new(3, 2)).await.unwrap();
        }
        // A fresh open replays the durable file — the vote is recovered.
        let mut s = DurableLogStore::open(dir.path()).unwrap();
        assert_eq!(s.read_vote().await.unwrap(), Some(Vote::new(3, 2)));
    }

    #[tokio::test]
    async fn torn_tail_record_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut s = DurableLogStore::open(dir.path()).unwrap();
            s.save_vote(&Vote::new(1, 1)).await.unwrap();
        }
        // Simulate a crash mid-write: a length prefix that claims more bytes than
        // actually follow.
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(dir.path().join("log"))
                .unwrap();
            f.write_all(&999u32.to_le_bytes()).unwrap();
            f.write_all(b"short").unwrap();
            f.sync_data().unwrap();
        }
        // The torn tail is discarded; the prior durable vote survives intact.
        let mut s = DurableLogStore::open(dir.path()).unwrap();
        assert_eq!(s.read_vote().await.unwrap(), Some(Vote::new(1, 1)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn committed_log_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();

        // A single-member group commits a batch of writes; openraft drives the
        // durable store's `save_vote`/`append` (which fsync), so the log + vote are
        // on disk.
        {
            let store = DurableLogStore::open(dir.path()).unwrap();
            let config = Arc::new(
                Config {
                    heartbeat_interval: 250,
                    election_timeout_min: 500,
                    election_timeout_max: 1000,
                    ..Default::default()
                }
                .validate()
                .unwrap(),
            );
            let sm = Arc::new(StateMachineStore::new(NoopApplier));
            let raft = openraft::Raft::<TypeConfig>::new(1, config, NoNetwork, store, sm)
                .await
                .unwrap();
            let mut members = BTreeMap::new();
            members.insert(1, BasicNode::default());
            raft.initialize(members).await.unwrap();
            raft.wait(Some(Duration::from_secs(10)))
                .state(ServerState::Leader, "single member becomes leader")
                .await
                .unwrap();
            for i in 0..5 {
                raft.client_write(del(i)).await.unwrap();
            }
            raft.shutdown().await.unwrap();
        }

        // Reopen from disk — the crash gate: the committed entries are recovered.
        let mut reopened = DurableLogStore::open(dir.path()).unwrap();
        let state = reopened.get_log_state().await.unwrap();
        let last = state
            .last_log_id
            .expect("a restarted node recovers its log tail");
        assert!(
            last.index >= 5,
            "the five committed writes survived the restart (last index {})",
            last.index
        );
        let entries = reopened.try_get_log_entries(0..=last.index).await.unwrap();
        assert!(
            entries.len() >= 5,
            "the durable log replays its entries after a restart"
        );
    }

    #[tokio::test]
    async fn replay_matches_a_reference_model_for_random_op_sequences() {
        use std::collections::BTreeSet;

        use openraft::{CommittedLeaderId, EntryPayload, LogId};

        fn log_entry(index: u64) -> Entry<TypeConfig> {
            Entry {
                log_id: LogId::new(CommittedLeaderId::new(1, 1), index),
                payload: EntryPayload::Normal(del(index)),
            }
        }

        // A pseudo-random sequence of store mutations must replay (after a reopen)
        // to exactly the state produced by applying them — hardening the hand-rolled
        // crash-recovery replay against any interleaving of append / truncate /
        // purge / save-vote (ADR-0067 increment 4d). The reference model mirrors the
        // store's documented effect of each op; a deterministic xorshift drives a
        // varied-but-reproducible sequence.
        let dir = tempfile::tempdir().unwrap();
        let mut model: BTreeSet<u64> = BTreeSet::new();
        let mut model_purged: Option<u64> = None;
        let mut model_vote: Option<Vote<u64>> = None;
        let mut next = 1u64;
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        {
            let mut s = DurableLogStore::open(dir.path()).unwrap();
            for _ in 0..120 {
                match rng() % 5 {
                    0 | 1 => {
                        let n = (rng() % 3) + 1;
                        let mut batch = Vec::new();
                        for _ in 0..n {
                            batch.push(log_entry(next));
                            model.insert(next);
                            next += 1;
                        }
                        s.append_entries(batch).await.unwrap();
                    }
                    2 if next > 1 => {
                        let at = (rng() % next).max(1);
                        s.truncate(LogId::new(CommittedLeaderId::new(1, 1), at))
                            .await
                            .unwrap();
                        model.retain(|&i| i < at);
                        next = at;
                    }
                    3 => {
                        if let Some(&max) = model.iter().next_back() {
                            let upto = rng() % (max + 1);
                            s.purge(LogId::new(CommittedLeaderId::new(1, 1), upto))
                                .await
                                .unwrap();
                            model.retain(|&i| i > upto);
                            model_purged = Some(upto);
                        }
                    }
                    _ => {
                        let v = Vote::new(rng() % 10, rng() % 3);
                        s.save_vote(&v).await.unwrap();
                        model_vote = Some(v);
                    }
                }
            }
        }

        // Reopen from disk: the replayed state must equal the reference model.
        let mut s = DurableLogStore::open(dir.path()).unwrap();
        assert_eq!(
            s.read_vote().await.unwrap(),
            model_vote,
            "vote replays exactly"
        );
        let state = s.get_log_state().await.unwrap();
        assert_eq!(
            state.last_purged_log_id.map(|l| l.index),
            model_purged,
            "last-purged replays exactly"
        );
        let indices: BTreeSet<u64> = s
            .try_get_log_entries(0..=u64::MAX)
            .await
            .unwrap()
            .iter()
            .map(|e| e.get_log_id().index)
            .collect();
        assert_eq!(
            indices, model,
            "the log replays to exactly the same entries"
        );
    }
}
