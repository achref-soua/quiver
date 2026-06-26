// SPDX-License-Identifier: AGPL-3.0-only
//! Per-shard Raft for write high availability (ADR-0067), increment 4a.
//!
//! This module wires the audited [`openraft`] consensus core to Quiver's engine.
//! A committed Raft log entry is **one engine write op** ([`WalOp`]); when the
//! group commits an entry, the state-machine adapter applies it to the local
//! engine through the same [`ADR-0030`] seam a replication follower already uses
//! (`Database::apply_replicated`). Acknowledging a write only after a Raft commit
//! is what makes a leader failover lose no acknowledged write and admit no
//! split-brain — but that multi-member story is increments 4b–4d.
//!
//! **Increment 4a is the low-risk foundation:** it runs a *single-member* group
//! (a node that trivially commits to itself) to prove the adapter end to end. The
//! single-node default and non-Raft clusters are untouched — the whole module is
//! compiled only behind the off-by-default `raft` cargo feature, so a default
//! build never links `openraft`.
//!
//! Storage follows openraft 0.9's `storage-v2` split:
//!
//! - the **log store** is the reusable generic in-memory [`LogStore`] vendored
//!   from openraft's example memstore (a durable, ADR-0050-snapshot-backed store
//!   arrives in 4c — see the `log_store` submodule);
//! - the **state machine** ([`StateMachineStore`]) is ours — it tracks the Raft
//!   bookkeeping (last-applied log id, membership) and forwards each committed op
//!   to an [`ApplyOp`] that owns the engine.
//!
//! [`ADR-0030`]: ../../../docs/adr/0030-leader-follower-replication.md

mod log_store;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use openraft::error::{Infallible, InstallSnapshotError};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::storage::{RaftStateMachine, Snapshot};
use openraft::{
    BasicNode, Config, Entry, EntryPayload, LogId, RaftSnapshotBuilder, RaftTypeConfig,
    SnapshotMeta, StorageError, StorageIOError, StoredMembership,
};
use quiver_core::WalOp;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Node identifier within a Raft group (a shard member).
pub type NodeId = u64;

/// The reusable in-memory Raft log store (vendored `log_store` submodule). 4a
/// uses the volatile one; a durable, log-compacting store lands in increment 4c.
pub type LogStore = log_store::LogStore<TypeConfig>;

/// A handle to this node's Raft group.
pub type Raft = openraft::Raft<TypeConfig>;

/// A Raft-layer error from this node.
pub type RaftError<E = Infallible> = openraft::error::RaftError<NodeId, E>;

/// A Raft RPC error to a peer. Unused by a single-member group (no peers), kept
/// so the network impl below names the right types for increment 4b.
pub type RpcError<E = Infallible> = openraft::error::RPCError<NodeId, BasicNode, RaftError<E>>;

openraft::declare_raft_types!(
    /// Type configuration for a per-shard Raft group (ADR-0067): a committed log
    /// entry carries one engine write op (`WalOp`); the apply response carries no
    /// payload (a committed write is acknowledged, nothing to read back).
    pub TypeConfig:
        D = WalOp,
        R = RaftResponse,
);

/// The response produced when the state machine applies a committed entry. A
/// committed write needs no read-back value, so this is an acknowledgement marker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RaftResponse;

/// The seam the state machine drives on commit: apply one committed cluster write
/// to the local engine. This is exactly ADR-0030's `apply_replicated` contract,
/// abstracted so the audited adapter stays independent of the engine's locking
/// strategy (the server's `Service::apply_replicated` offloads the blocking
/// engine work via `write_blocking`; increment 4b wires that impl in).
pub trait ApplyOp: Send + Sync + 'static {
    /// Apply a committed op to the engine. An error here is a genuine storage
    /// fault on an already-committed entry — it is surfaced loudly (the state
    /// machine maps it to a Raft `StorageError`), never silently dropped.
    fn apply(&self, op: WalOp) -> impl std::future::Future<Output = std::io::Result<()>> + Send;
}

/// The Raft bookkeeping this node persists alongside the engine: the id of the
/// last applied log entry and the active membership. In 4a this is all the
/// snapshot carries — engine data is snapshotted via ADR-0050 in increment 4c.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StateMachineData {
    /// Log id of the most recently applied entry (`None` before the first apply).
    pub last_applied_log: Option<LogId<NodeId>>,
    /// The cluster membership last committed to the state machine.
    pub last_membership: StoredMembership<NodeId, BasicNode>,
}

/// A captured state-machine snapshot: its Raft metadata and serialized data.
#[derive(Debug, Clone)]
pub struct StoredSnapshot {
    /// Snapshot metadata (covered log id, membership, snapshot id).
    pub meta: SnapshotMeta<NodeId, BasicNode>,
    /// Serialized [`StateMachineData`] at the snapshot point.
    pub data: Vec<u8>,
}

/// The Raft state machine: it owns the engine applier and the Raft bookkeeping,
/// and (per openraft) also retains the last snapshot. `RaftStateMachine` is
/// implemented on `Arc<StateMachineStore<A>>` so openraft can clone a cheap
/// handle; interior mutability lives behind the locks.
#[derive(Debug)]
pub struct StateMachineStore<A: ApplyOp> {
    /// Drives a committed op into the engine.
    pub applier: A,
    state_machine: RwLock<StateMachineData>,
    /// Monotonic-ish snapshot counter (uniqueness only; gaps are fine).
    snapshot_idx: AtomicU64,
    current_snapshot: RwLock<Option<StoredSnapshot>>,
}

impl<A: ApplyOp> StateMachineStore<A> {
    /// Build a fresh state machine over an engine applier.
    pub fn new(applier: A) -> Self {
        Self {
            applier,
            state_machine: RwLock::default(),
            snapshot_idx: AtomicU64::new(0),
            current_snapshot: RwLock::default(),
        }
    }
}

impl<A: ApplyOp> RaftSnapshotBuilder<TypeConfig> for Arc<StateMachineStore<A>> {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let (data, last_applied_log, last_membership) = {
            let sm = self.state_machine.read().await;
            let data =
                serde_json::to_vec(&*sm).map_err(|e| StorageIOError::read_state_machine(&e))?;
            (data, sm.last_applied_log, sm.last_membership.clone())
        };

        let snapshot_idx = self.snapshot_idx.fetch_add(1, Ordering::Relaxed) + 1;
        let snapshot_id = match last_applied_log {
            Some(last) => format!("{}-{}-{}", last.leader_id, last.index, snapshot_idx),
            None => format!("--{snapshot_idx}"),
        };

        let meta = SnapshotMeta {
            last_log_id: last_applied_log,
            last_membership,
            snapshot_id,
        };
        *self.current_snapshot.write().await = Some(StoredSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        });

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl<A: ApplyOp> RaftStateMachine<TypeConfig> for Arc<StateMachineStore<A>> {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, BasicNode>), StorageError<NodeId>>
    {
        let sm = self.state_machine.read().await;
        Ok((sm.last_applied_log, sm.last_membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<RaftResponse>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
    {
        let mut responses = Vec::new();
        let mut sm = self.state_machine.write().await;
        for entry in entries {
            sm.last_applied_log = Some(entry.log_id);
            match entry.payload {
                // Leader no-op heartbeat / membership entries carry no engine op.
                EntryPayload::Blank => {}
                EntryPayload::Normal(ref op) => {
                    // The entry is already committed; an apply failure is a real
                    // storage fault, mapped to a Raft StorageError (not swallowed).
                    self.applier
                        .apply(op.clone())
                        .await
                        .map_err(|e| StorageIOError::apply(entry.log_id, &e))?;
                }
                EntryPayload::Membership(ref mem) => {
                    sm.last_membership = StoredMembership::new(Some(entry.log_id), mem.clone());
                }
            }
            responses.push(RaftResponse);
        }
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<<TypeConfig as RaftTypeConfig>::SnapshotData>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, BasicNode>,
        snapshot: Box<<TypeConfig as RaftTypeConfig>::SnapshotData>,
    ) -> Result<(), StorageError<NodeId>> {
        // ponytail: 4a carries only Raft metadata in the snapshot, so this keeps
        // the Raft layer consistent but does NOT transfer engine data. A
        // single-member group never installs a snapshot, so it is never exercised
        // here; increment 4c integrates ADR-0050 so a lagging/new voter receives
        // engine state. Until then a real install would under-restore — which is
        // why multi-member voting is gated to 4b+.
        let stored = StoredSnapshot {
            meta: meta.clone(),
            data: snapshot.into_inner(),
        };
        let restored: StateMachineData = serde_json::from_slice(&stored.data)
            .map_err(|e| StorageIOError::read_snapshot(Some(stored.meta.signature()), &e))?;
        *self.state_machine.write().await = restored;
        *self.current_snapshot.write().await = Some(stored);
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        Ok(self
            .current_snapshot
            .read()
            .await
            .as_ref()
            .map(|s| Snapshot {
                meta: s.meta.clone(),
                snapshot: Box::new(Cursor::new(s.data.clone())),
            }))
    }
}

/// A no-op Raft network. A single-member group (4a) has no peers, so openraft
/// never opens a client or sends an RPC; the real HTTP network arrives in 4b.
#[derive(Debug, Clone, Default)]
pub struct NoNetwork;

impl RaftNetworkFactory<TypeConfig> for NoNetwork {
    type Network = NoConnection;

    async fn new_client(&mut self, _target: NodeId, _node: &BasicNode) -> Self::Network {
        // Reached only when replicating to a peer — impossible with one member.
        unreachable!("single-member raft (4a) has no peers; real RPC arrives in 4b")
    }
}

/// A peer connection that never exists in a single-member group.
#[derive(Debug, Clone)]
pub struct NoConnection;

impl RaftNetwork<TypeConfig> for NoConnection {
    async fn append_entries(
        &mut self,
        _req: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RpcError> {
        unreachable!("single-member raft (4a) sends no RPCs; real network arrives in 4b")
    }

    async fn install_snapshot(
        &mut self,
        _req: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<InstallSnapshotResponse<NodeId>, RpcError<InstallSnapshotError>> {
        unreachable!("single-member raft (4a) sends no RPCs; real network arrives in 4b")
    }

    async fn vote(
        &mut self,
        _req: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RpcError> {
        unreachable!("single-member raft (4a) sends no RPCs; real network arrives in 4b")
    }
}

/// Boot a **single-member** Raft group for `node_id` over `applier`, initialized
/// so the node is the sole voter (and so becomes leader). This is the 4a proof
/// vehicle: `client_write` an op, it commits to the one member, and the applier
/// drives it into the engine.
///
/// # Errors
/// Propagates openraft configuration, construction, or initialization errors.
pub async fn start_single_member<A: ApplyOp>(
    node_id: NodeId,
    applier: A,
) -> Result<Raft, Box<dyn std::error::Error + Send + Sync>> {
    let config = Arc::new(
        Config {
            heartbeat_interval: 250,
            election_timeout_min: 500,
            election_timeout_max: 1000,
            ..Default::default()
        }
        .validate()?,
    );
    let log_store = LogStore::default();
    let state_machine = Arc::new(StateMachineStore::new(applier));
    let raft = openraft::Raft::new(node_id, config, NoNetwork, log_store, state_machine).await?;

    let mut members = BTreeMap::new();
    members.insert(node_id, BasicNode::default());
    raft.initialize(members).await?;
    Ok(raft)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    use openraft::ServerState;
    use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, SearchParams};
    use tokio::sync::Mutex;

    use super::*;

    /// An applier that records nothing — for exercising the state-machine
    /// bookkeeping/snapshot paths in isolation from any engine.
    struct NoopApplier;

    impl ApplyOp for NoopApplier {
        async fn apply(&self, _op: WalOp) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An [`ApplyOp`] over a real engine — drives `Database::apply_replicated`,
    /// the ADR-0030 seam. The in-process lock is fine for the small test corpus;
    /// production wiring (4b) offloads the blocking engine work via the server's
    /// `write_blocking`.
    struct EngineApplier(Arc<Mutex<Database>>);

    impl ApplyOp for EngineApplier {
        async fn apply(&self, op: WalOp) -> std::io::Result<()> {
            self.0
                .lock()
                .await
                .apply_replicated(op)
                .map_err(|e| std::io::Error::other(e.to_string()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn single_member_group_applies_committed_ops_to_engine() {
        // A source engine populated the ordinary way; capture the WAL ops that
        // recreate it (the same bootstrap a replication follower replays).
        let src_dir = tempfile::tempdir().unwrap();
        let mut src = Database::open(src_dir.path()).unwrap();
        src.create_collection("docs", Descriptor::new(4, Dtype::F32, DistanceMetric::L2))
            .unwrap();
        src.upsert(
            "docs",
            "a",
            &[1.0, 0.0, 0.0, 0.0],
            &serde_json::json!({"t": "a"}),
        )
        .unwrap();
        src.upsert(
            "docs",
            "b",
            &[0.0, 1.0, 0.0, 0.0],
            &serde_json::json!({"t": "b"}),
        )
        .unwrap();
        let ops = src.replication_snapshot().unwrap();
        assert!(ops.len() >= 3, "create-collection + two upserts");

        // An empty target engine, fed every op through a single-member group.
        let tgt_dir = tempfile::tempdir().unwrap();
        let target = Arc::new(Mutex::new(Database::open(tgt_dir.path()).unwrap()));
        let raft = start_single_member(1, EngineApplier(target.clone()))
            .await
            .unwrap();
        raft.wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "single member becomes leader")
            .await
            .unwrap();

        for op in ops {
            // Commit each op via consensus; a single member commits to itself.
            raft.client_write(op).await.unwrap();
        }

        // Proof the apply seam fired: the target engine now serves the points.
        let params = SearchParams {
            k: 2,
            ef_search: 16,
            with_payload: false,
            with_vector: false,
            filter: None,
        };
        let hits = target
            .lock()
            .await
            .search("docs", &[1.0, 0.0, 0.0, 0.0], &params)
            .unwrap();
        assert_eq!(hits.first().map(|m| m.id.as_str()), Some("a"));
        let ids: HashSet<_> = hits.iter().map(|m| m.id.clone()).collect();
        assert!(ids.contains("a") && ids.contains("b"), "both points served");

        raft.shutdown().await.unwrap();
    }

    /// An applier whose engine apply always fails — to prove the adapter does not
    /// silently swallow a fault on an already-committed entry.
    struct FailingApplier;

    impl ApplyOp for FailingApplier {
        async fn apply(&self, _op: WalOp) -> std::io::Result<()> {
            Err(std::io::Error::other("simulated engine apply fault"))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_failure_surfaces_not_swallowed() {
        let raft = start_single_member(1, FailingApplier).await.unwrap();
        raft.wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "leader")
            .await
            .unwrap();

        // The entry commits, then apply fails: the adapter maps that to a Raft
        // StorageError, so client_write resolves to an error — not a phantom
        // success that would lose the fault.
        let op = WalOp::Delete {
            collection_id: quiver_core::CollectionId(1),
            external_id: "x".to_owned(),
        };
        assert!(
            raft.client_write(op).await.is_err(),
            "an engine apply fault must surface, not be swallowed"
        );
    }

    #[tokio::test]
    async fn state_machine_snapshot_roundtrip() {
        // Exercise the RaftStateMachine adapter methods directly (the snapshot
        // surface a single-member group never drives on its own): build a
        // snapshot, read it back, then install one and observe the restored state.
        use std::io::Cursor;

        use openraft::SnapshotMeta;

        let sm = Arc::new(StateMachineStore::new(NoopApplier));

        // A fresh machine has applied nothing.
        let (applied, _membership) = sm.clone().applied_state().await.unwrap();
        assert!(applied.is_none());
        assert!(sm.clone().get_current_snapshot().await.unwrap().is_none());

        // Build a snapshot; it is captured as the current snapshot and readable.
        let mut builder = sm.clone().get_snapshot_builder().await;
        let built = builder.build_snapshot().await.unwrap();
        assert!(!built.meta.snapshot_id.is_empty());
        assert!(built.meta.last_log_id.is_none());
        assert!(sm.clone().get_current_snapshot().await.unwrap().is_some());

        // Once something has been applied, the snapshot encodes the last log id.
        {
            let mut data = sm.state_machine.write().await;
            data.last_applied_log = Some(openraft::LogId::new(
                openraft::CommittedLeaderId::new(1, 1),
                7,
            ));
        }
        let applied_snapshot = sm
            .clone()
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        assert!(applied_snapshot.meta.last_log_id.is_some());

        // Receive and install a distinct snapshot (begin_receiving_snapshot +
        // install_snapshot); the machine adopts it as its current snapshot.
        let bytes = serde_json::to_vec(&StateMachineData::default()).unwrap();
        let mut receiver = sm.clone();
        let mut cursor = receiver.begin_receiving_snapshot().await.unwrap();
        *cursor = Cursor::new(bytes);
        let meta = SnapshotMeta {
            last_log_id: None,
            last_membership: StoredMembership::default(),
            snapshot_id: "installed".to_owned(),
        };
        receiver.install_snapshot(&meta, cursor).await.unwrap();

        let current = sm.clone().get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(current.meta.snapshot_id, "installed", "install replaced it");
    }
}
