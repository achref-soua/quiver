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

pub mod grpc;
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

    // ----------------------------------------------------------------------
    // Multi-member consensus + failover (4b-i).
    //
    // These exercise a real multi-voter Raft group (replicas as voters) over an
    // *in-process* network: a switchboard that forwards each RPC straight to the
    // target node's `Raft` receiving handler, and models a node going down by
    // de-registering it (so peers see `Unreachable`). The consensus protocol and
    // the engine-backed state-machine adapter are the real ones — only the
    // transport is in-process (the gRPC transport is 4b-ii). This isolates the
    // scariest property — *no acknowledged write is lost across a leader
    // failover* — from the network/server plumbing, and proves it deterministically.
    //
    // Per ADR-0067 (owner-confirmed staging), the Raft log here is the volatile
    // 4a store, so these cover **leader failover among live members** (the killed
    // leader does not rejoin); a crashed voter rejoining safely needs the durable
    // store of increment 4c.
    // ----------------------------------------------------------------------

    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    use openraft::BasicNode;
    use openraft::error::{InstallSnapshotError, RPCError, RemoteError, Unreachable};
    use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
    use openraft::raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    };

    /// An in-process Raft network: a shared registry of live nodes' `Raft`
    /// handles. Sending an RPC looks up the target and calls its receiving
    /// handler directly; a de-registered (killed) target is `Unreachable`.
    #[derive(Clone, Default)]
    struct Switchboard {
        nodes: Arc<StdMutex<BTreeMap<NodeId, Raft>>>,
    }

    impl Switchboard {
        fn register(&self, id: NodeId, raft: Raft) {
            self.nodes.lock().unwrap().insert(id, raft);
        }

        /// Take a node off the network — models a crashed/partitioned node.
        fn kill(&self, id: NodeId) {
            self.nodes.lock().unwrap().remove(&id);
        }

        fn handle(&self, id: NodeId) -> Option<Raft> {
            self.nodes.lock().unwrap().get(&id).cloned()
        }
    }

    impl RaftNetworkFactory<TypeConfig> for Switchboard {
        type Network = Link;

        async fn new_client(&mut self, target: NodeId, _node: &BasicNode) -> Link {
            Link {
                target,
                board: self.clone(),
            }
        }
    }

    /// A connection to one peer over the [`Switchboard`].
    struct Link {
        target: NodeId,
        board: Switchboard,
    }

    impl Link {
        // openraft's `RPCError` is an inherently large enum that the network trait
        // returns by value; nothing to box here (test transport).
        #[allow(clippy::result_large_err)]
        fn target(&self) -> Result<Raft, RPCError<NodeId, BasicNode, RaftError>> {
            self.board.handle(self.target).ok_or_else(|| {
                RPCError::Unreachable(Unreachable::new(&std::io::Error::other("node down")))
            })
        }
    }

    impl RaftNetwork<TypeConfig> for Link {
        async fn append_entries(
            &mut self,
            rpc: AppendEntriesRequest<TypeConfig>,
            _option: RPCOption,
        ) -> Result<AppendEntriesResponse<NodeId>, RpcError> {
            self.target()?
                .append_entries(rpc)
                .await
                .map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
        }

        async fn vote(
            &mut self,
            rpc: VoteRequest<NodeId>,
            _option: RPCOption,
        ) -> Result<VoteResponse<NodeId>, RpcError> {
            self.target()?
                .vote(rpc)
                .await
                .map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
        }

        async fn install_snapshot(
            &mut self,
            _rpc: InstallSnapshotRequest<TypeConfig>,
            _option: RPCOption,
        ) -> Result<InstallSnapshotResponse<NodeId>, RpcError<InstallSnapshotError>> {
            // Never reached: these tests keep the full log (no compaction/purge),
            // so a lagging follower catches up via append_entries, never a
            // snapshot install. Snapshot transfer arrives with log compaction (4c).
            unreachable!("4b-i tests never transfer a snapshot")
        }
    }

    /// One Raft voter: its id, handle, engine, and the temp dir keeping the
    /// engine's files alive for the test's lifetime.
    struct Voter {
        id: NodeId,
        raft: Raft,
        engine: Arc<Mutex<Database>>,
        _dir: tempfile::TempDir,
    }

    // Boot an `ids`-member Raft group over one switchboard, each voter backed by
    // its own engine, and initialize the cluster on the first node.
    async fn boot_cluster(ids: &[NodeId]) -> (Switchboard, Vec<Voter>) {
        let board = Switchboard::default();
        let mut voters = Vec::new();
        for &id in ids {
            let dir = tempfile::tempdir().unwrap();
            let engine = Arc::new(Mutex::new(Database::open(dir.path()).unwrap()));
            let config = Arc::new(
                Config {
                    heartbeat_interval: 100,
                    election_timeout_min: 300,
                    election_timeout_max: 600,
                    ..Default::default()
                }
                .validate()
                .unwrap(),
            );
            let sm = Arc::new(StateMachineStore::new(EngineApplier(engine.clone())));
            let raft = openraft::Raft::new(id, config, board.clone(), LogStore::default(), sm)
                .await
                .unwrap();
            board.register(id, raft.clone());
            voters.push(Voter {
                id,
                raft,
                engine,
                _dir: dir,
            });
        }
        let members: BTreeMap<NodeId, BasicNode> =
            ids.iter().map(|&id| (id, BasicNode::default())).collect();
        voters[0].raft.initialize(members).await.unwrap();
        (board, voters)
    }

    // The id the cluster currently agrees is leader, among still-live nodes.
    fn current_leader(board: &Switchboard, voters: &[Voter]) -> Option<NodeId> {
        for v in voters {
            if board.handle(v.id).is_none() {
                continue; // killed
            }
            let leader = v.raft.metrics().borrow().current_leader;
            if let Some(leader) = leader
                && board.handle(leader).is_some()
            {
                return Some(leader);
            }
        }
        None
    }

    // Commit one op through whoever is currently leader, retrying across an
    // in-flight election. Panics if no leader accepts it within the budget.
    async fn commit(board: &Switchboard, voters: &[Voter], op: &WalOp) {
        for _ in 0..100 {
            if let Some(leader_id) = current_leader(board, voters)
                && let Some(leader) = voters.iter().find(|v| v.id == leader_id)
                && leader.raft.client_write(op.clone()).await.is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        panic!("no leader committed the op within the budget");
    }

    // Poll until `engine` serves `want_id` for `query` (followers apply async).
    async fn await_serves(engine: &Arc<Mutex<Database>>, query: &[f32], want_id: &str) {
        let params = SearchParams {
            k: 5,
            filter: None,
            ef_search: 32,
            with_payload: false,
            with_vector: false,
        };
        for _ in 0..200 {
            if let Ok(hits) = engine.lock().await.search("docs", query, &params)
                && hits.iter().any(|m| m.id == want_id)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        panic!("engine never served {want_id}");
    }

    // The WAL ops that create a 4-dim L2 collection and upsert the given points.
    fn collection_ops(points: &[(&str, [f32; 4])]) -> Vec<WalOp> {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::open(dir.path()).unwrap();
        db.create_collection("docs", Descriptor::new(4, Dtype::F32, DistanceMetric::L2))
            .unwrap();
        for (id, v) in points {
            db.upsert("docs", id, v, &serde_json::json!({})).unwrap();
        }
        db.replication_snapshot().unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn three_member_group_applies_on_every_voter() {
        let (board, voters) = boot_cluster(&[1, 2, 3]).await;
        voters[0]
            .raft
            .wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "bootstrap leader")
            .await
            .unwrap();

        let a = [1.0, 0.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0, 0.0];
        for op in collection_ops(&[("a", a), ("b", b)]) {
            commit(&board, &voters, &op).await;
        }

        // Every voter's engine — leader and followers — serves both points.
        for v in &voters {
            await_serves(&v.engine, &a, "a").await;
            await_serves(&v.engine, &b, "b").await;
        }

        for v in &voters {
            v.raft.shutdown().await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn leader_failure_preserves_acknowledged_writes() {
        let (board, voters) = boot_cluster(&[1, 2, 3]).await;
        voters[0]
            .raft
            .wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "bootstrap leader")
            .await
            .unwrap();

        // Acknowledge a batch of writes through the original leader.
        let a = [1.0, 0.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0, 0.0];
        let ops = collection_ops(&[("a", a), ("b", b)]);
        let coll_id = ops
            .iter()
            .find_map(|op| match op {
                WalOp::CreateCollection { collection_id, .. } => Some(*collection_id),
                _ => None,
            })
            .expect("create-collection op");
        for op in &ops {
            commit(&board, &voters, op).await;
        }

        // Kill the leader. A surviving voter (quorum of 2/3 remains) takes over.
        let dead = current_leader(&board, &voters).expect("a leader exists");
        board.kill(dead);
        if let Some(v) = voters.iter().find(|v| v.id == dead) {
            v.raft.shutdown().await.unwrap();
        }
        let survivors: Vec<&Voter> = voters.iter().filter(|v| v.id != dead).collect();

        // Acknowledge MORE writes after the failover (forces a new leader). The op
        // targets the same collection id the cluster already created.
        let c = [0.0f32, 0.0, 1.0, 0.0];
        let c_op = WalOp::Upsert {
            collection_id: coll_id,
            external_id: "c".to_owned(),
            vector: c.iter().flat_map(|f| f.to_le_bytes()).collect(),
            payload: b"{}".to_vec(),
        };
        commit(&board, &voters, &c_op).await;

        // No acknowledged write is lost: every survivor serves all three points,
        // the pre-failover ("a","b") and the post-failover ("c").
        for v in &survivors {
            await_serves(&v.engine, &a, "a").await;
            await_serves(&v.engine, &b, "b").await;
            await_serves(&v.engine, &c, "c").await;
        }

        // Cross-check against single-node ground truth: a lone engine fed the same
        // ops returns the same nearest neighbour for each query.
        let truth_dir = tempfile::tempdir().unwrap();
        let mut truth = Database::open(truth_dir.path()).unwrap();
        for op in collection_ops(&[("a", a), ("b", b), ("c", c)]) {
            truth.apply_replicated(op).unwrap();
        }
        let params = SearchParams {
            k: 1,
            filter: None,
            ef_search: 16,
            with_payload: false,
            with_vector: false,
        };
        for (q, want) in [(a, "a"), (b, "b"), (c, "c")] {
            let truth_top = truth.search("docs", &q, &params).unwrap()[0].id.clone();
            assert_eq!(truth_top, want);
            let survivor_top = survivors[0]
                .engine
                .lock()
                .await
                .search("docs", &q, &params)
                .unwrap()[0]
                .id
                .clone();
            assert_eq!(
                survivor_top, truth_top,
                "survivor matches single-node truth"
            );
        }

        for v in survivors {
            v.raft.shutdown().await.unwrap();
        }
    }
}
