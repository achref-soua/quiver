// SPDX-License-Identifier: AGPL-3.0-only
//! Per-shard Raft for write high availability (ADR-0067), increments 4a–4d.
//!
//! This module wires the audited [`openraft`] consensus core to Quiver's engine.
//! A committed Raft log entry is **one engine write op** ([`WalOp`]); when the
//! group commits an entry, the state-machine adapter applies it to the local
//! engine through the same [`ADR-0030`] seam a replication follower already uses
//! (`Database::apply_replicated`). Acknowledging a write only after a Raft commit
//! is what makes a leader failover lose no acknowledged write and admit no
//! split-brain.
//!
//! **The shipped production path is a real multi-member group.** [`start_member`]
//! boots a node into an `n`-node group over the gRPC Raft network
//! (`grpc::GrpcRaftNetwork`): automatic leader election and failover (4b),
//! a crash-safe durable log (`durable_log`, 4c), online voter changes (4c —
//! [`RaftShard::add_voter`]/`remove_voter`), and
//! snapshot-based catch-up + partition/rejoin hardening (4d). The whole module is
//! compiled only behind the off-by-default `raft` cargo feature, so a default
//! build never links `openraft` and the single-node default is untouched.
//!
//! [`start_single_member`] and [`NoNetwork`] are the original 4a single-member
//! vehicle — retained as unit-test scaffolding for the applier and the log store
//! (a one-member group commits to itself, so no peer RPC is exercised); they are
//! not the production boot path.
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

mod durable_log;
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

    /// Capture the engine's full state as an opaque blob (ADR-0050), for a Raft
    /// snapshot. A snapshot lets the log be compacted (ADR-0067 increment 4c) and a
    /// far-behind or newly added voter catch up by installing it instead of
    /// replaying the whole log. The default captures nothing — only an
    /// engine-backed applier needs to override it.
    fn snapshot(&self) -> impl std::future::Future<Output = std::io::Result<Vec<u8>>> + Send {
        async { Ok(Vec::new()) }
    }

    /// Replace the engine's state with a blob produced by [`snapshot`](Self::snapshot)
    /// (the receiving side of a Raft snapshot install). The default does nothing.
    fn restore(
        &self,
        data: Vec<u8>,
    ) -> impl std::future::Future<Output = std::io::Result<()>> + Send {
        let _ = data;
        async { Ok(()) }
    }
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
    /// Serialized `SnapshotPayload` at the snapshot point.
    pub data: Vec<u8>,
}

/// The serialized contents of a Raft snapshot (ADR-0067 increment 4c): the Raft
/// bookkeeping plus the engine's state captured via [`ApplyOp::snapshot`]. A voter
/// installing this restores its engine from `engine` and adopts `sm`, so it can
/// catch up from a snapshot instead of replaying a (possibly purged) log.
#[derive(Serialize, Deserialize)]
struct SnapshotPayload {
    sm: StateMachineData,
    engine: Vec<u8>,
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
        let (sm, last_applied_log, last_membership) = {
            let sm = self.state_machine.read().await;
            (sm.clone(), sm.last_applied_log, sm.last_membership.clone())
        };
        // Capture the engine alongside the bookkeeping so a far-behind or newly
        // added voter can catch up by installing this rather than replaying a log
        // that may have been compacted away (ADR-0067 increment 4c).
        let engine = self
            .applier
            .snapshot()
            .await
            .map_err(|e| StorageIOError::read_state_machine(&e))?;
        let data = serde_json::to_vec(&SnapshotPayload { sm, engine })
            .map_err(|e| StorageIOError::read_state_machine(&e))?;

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
        // Restore the engine from the snapshot, then adopt the Raft bookkeeping
        // (ADR-0067 increment 4c): a far-behind or newly added voter receives the
        // engine state it could not replay from a compacted log. `ApplyOp::restore`
        // resets the engine before replaying, so this is correct even if the voter
        // already holds divergent state.
        let data = snapshot.into_inner();
        let payload: SnapshotPayload = serde_json::from_slice(&data)
            .map_err(|e| StorageIOError::read_snapshot(Some(meta.signature()), &e))?;
        self.applier
            .restore(payload.engine)
            .await
            .map_err(|e| StorageIOError::write_snapshot(Some(meta.signature()), &e))?;
        *self.state_machine.write().await = payload.sm;
        *self.current_snapshot.write().await = Some(StoredSnapshot {
            meta: meta.clone(),
            data,
        });
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

/// A no-op Raft network for a **single-member** group: with one voter openraft
/// never opens a client or sends an RPC. Used only by [`start_single_member`] as
/// unit-test scaffolding; the production multi-member path uses the real gRPC
/// network ([`grpc::GrpcRaftNetwork`]).
#[derive(Debug, Clone, Default)]
pub struct NoNetwork;

impl RaftNetworkFactory<TypeConfig> for NoNetwork {
    type Network = NoConnection;

    async fn new_client(&mut self, _target: NodeId, _node: &BasicNode) -> Self::Network {
        // Reached only when replicating to a peer — impossible with one member.
        unreachable!("single-member raft has no peers; the multi-member path uses GrpcRaftNetwork")
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
        unreachable!("single-member raft sends no RPCs; the multi-member path uses GrpcRaftNetwork")
    }

    async fn install_snapshot(
        &mut self,
        _req: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<InstallSnapshotResponse<NodeId>, RpcError<InstallSnapshotError>> {
        unreachable!("single-member raft sends no RPCs; the multi-member path uses GrpcRaftNetwork")
    }

    async fn vote(
        &mut self,
        _req: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RpcError> {
        unreachable!("single-member raft sends no RPCs; the multi-member path uses GrpcRaftNetwork")
    }
}

/// Boot a **single-member** Raft group for `node_id` over `applier`, initialized
/// so the node is the sole voter (and so becomes leader). This is the original 4a
/// proof vehicle, retained to unit-test the applier and log store in isolation:
/// `client_write` an op, it commits to the one member, and the applier drives it
/// into the engine. Production uses the multi-member [`start_member`] instead.
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

/// The production [`ApplyOp`]: drive a committed entry into the server's engine
/// through the same `apply_replicated` seam (ADR-0030) a replication follower
/// uses, offloading the synchronous engine work with `spawn_blocking`. It holds
/// the engine handle **directly** (not the whole server `AppState`), so the Raft
/// group the state machine lives in is never referenced back — no `Arc` cycle.
pub struct EngineApplier {
    db: Arc<std::sync::RwLock<quiver_embed::Database>>,
}

impl EngineApplier {
    /// Build an applier over the server's shared engine handle.
    pub fn new(db: Arc<std::sync::RwLock<quiver_embed::Database>>) -> Self {
        Self { db }
    }
}

impl ApplyOp for EngineApplier {
    async fn apply(&self, op: WalOp) -> std::io::Result<()> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let mut guard = db
                .write()
                .map_err(|_| std::io::Error::other("database lock poisoned"))?;
            guard
                .apply_replicated(op)
                .map_err(|e| std::io::Error::other(e.to_string()))
        })
        .await
        .map_err(|e| std::io::Error::other(format!("blocking apply task failed: {e}")))?
    }

    // Capture the engine as the WalOps that recreate it (ADR-0050's replication
    // snapshot — the same op stream a fresh follower bootstraps from), postcard-
    // encoded. A read lock suffices; the blocking work is offloaded.
    async fn snapshot(&self) -> std::io::Result<Vec<u8>> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let guard = db
                .read()
                .map_err(|_| std::io::Error::other("database lock poisoned"))?;
            let ops = guard
                .replication_snapshot()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            postcard::to_allocvec(&ops).map_err(|e| std::io::Error::other(e.to_string()))
        })
        .await
        .map_err(|e| std::io::Error::other(format!("blocking snapshot task failed: {e}")))?
    }

    // Replace the engine with a snapshot: reset (drop every collection) then replay
    // the captured WalOps. Reset-then-replay (rather than merge) makes install
    // idempotent and correct even on a voter that already holds divergent state.
    async fn restore(&self, data: Vec<u8>) -> std::io::Result<()> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let ops: Vec<WalOp> =
                postcard::from_bytes(&data).map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut guard = db
                .write()
                .map_err(|_| std::io::Error::other("database lock poisoned"))?;
            for name in guard.collection_names() {
                guard
                    .drop_collection(&name)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
            for op in ops {
                guard
                    .apply_replicated(op)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| std::io::Error::other(format!("blocking restore task failed: {e}")))?
    }
}

/// A node's per-shard Raft group plus the handles the server's write path needs
/// (ADR-0067, increment 4b). Held behind an `Arc` in `AppState` because the
/// `create_lock` is not `Clone`.
pub struct RaftShard {
    /// This node's Raft handle.
    pub raft: Raft,
    /// This node's member id within the group.
    pub node_id: NodeId,
    /// The group's members: id → gRPC base URL, for resolving a leader hint to a
    /// URL in the "not the leader" redirect. Behind a lock because the voter set is
    /// **dynamic** (ADR-0067 increment 4c): [`add_voter`](Self::add_voter) /
    /// [`remove_voter`](Self::remove_voter) change it at runtime.
    pub members: std::sync::RwLock<BTreeMap<NodeId, String>>,
    /// Serializes create-collection proposals so two concurrent creates cannot
    /// claim the same next collection id — the leader assigns it at prepare time
    /// (ADR-0067, owner-locked decision). Upserts/deletes target an existing
    /// collection and take no lock.
    pub create_lock: tokio::sync::Mutex<()>,
}

impl RaftShard {
    /// Resolve a member id to its gRPC base URL (for the "not the leader" redirect).
    pub fn member_url(&self, id: NodeId) -> Option<String> {
        self.members.read().ok()?.get(&id).cloned()
    }

    /// Add a voter to this shard's Raft group at runtime (ADR-0067 increment 4c):
    /// first add it as a **learner** so it catches up (replaying the log, or
    /// installing a snapshot if the log was compacted — increment 4c), blocking
    /// until it is current, then **promote** it to a voting member via a joint
    /// consensus change. Must be called on the leader.
    ///
    /// # Errors
    /// Propagates openraft learner/membership-change errors (e.g. not the leader).
    pub async fn add_voter(
        &self,
        id: NodeId,
        url: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.raft
            .add_learner(id, BasicNode::new(url.clone()), true)
            .await?;
        self.raft
            .change_membership(
                openraft::ChangeMembers::AddVoterIds([id].into_iter().collect()),
                true,
            )
            .await?;
        if let Ok(mut m) = self.members.write() {
            m.insert(id, url);
        }
        Ok(())
    }

    /// Remove a voter from this shard's Raft group at runtime (ADR-0067 increment
    /// 4c) via a joint consensus change. Must be called on the leader.
    ///
    /// # Errors
    /// Propagates openraft membership-change errors (e.g. not the leader).
    pub async fn remove_voter(
        &self,
        id: NodeId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.raft
            .change_membership(
                openraft::ChangeMembers::RemoveVoters([id].into_iter().collect()),
                false,
            )
            .await?;
        if let Ok(mut m) = self.members.write() {
            m.remove(&id);
        }
        Ok(())
    }
}

/// Boot this node's per-shard Raft group as a member of `members` (id → gRPC base
/// URL), applying committed entries through `applier` over the gRPC transport.
///
/// The lowest-id member bootstraps the group by initializing the membership
/// (idempotent — an already-initialized node is left as-is); the other members
/// join when the initializer replicates the membership entry to them. Returns
/// once the local group is running; leader election and replication proceed in
/// the background, so a member whose peers are not up yet does not block startup.
///
/// # Errors
/// Propagates openraft configuration/construction errors.
pub async fn start_member(
    node_id: NodeId,
    members: BTreeMap<NodeId, String>,
    applier: EngineApplier,
    log_dir: &std::path::Path,
) -> Result<RaftShard, Box<dyn std::error::Error + Send + Sync>> {
    let config = Arc::new(
        Config {
            heartbeat_interval: 250,
            election_timeout_min: 500,
            election_timeout_max: 1000,
            ..Default::default()
        }
        .validate()?,
    );
    // The durable, crash-safe log store (ADR-0067 increment 4c): a restarted voter
    // recovers its log + vote and rejoins safely. The volatile `LogStore` stays for
    // the in-process consensus tests, which never restart.
    let log_store = durable_log::DurableLogStore::open(log_dir)?;
    let state_machine = Arc::new(StateMachineStore::new(applier));
    let raft = openraft::Raft::new(
        node_id,
        config,
        grpc::GrpcRaftNetwork,
        log_store,
        state_machine,
    )
    .await?;

    // The lowest-id member bootstraps the group. The 4b log is volatile, so a
    // fresh boot is always uninitialized; the `is_initialized` guard keeps this
    // correct for the durable store of 4c without changing 4b behaviour.
    if members.keys().next() == Some(&node_id) && !raft.is_initialized().await? {
        let nodes: BTreeMap<NodeId, BasicNode> = members
            .iter()
            .map(|(id, url)| (*id, BasicNode::new(url.clone())))
            .collect();
        if let Err(e) = raft.initialize(nodes).await {
            // An already-initialized race is benign; anything else is logged but
            // not fatal — the node still serves and can be re-bootstrapped.
            tracing::debug!(error = %e, "raft initialize (already bootstrapped?)");
        }
    }

    Ok(RaftShard {
        raft,
        node_id,
        members: std::sync::RwLock::new(members),
        create_lock: tokio::sync::Mutex::new(()),
    })
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

        async fn snapshot(&self) -> std::io::Result<Vec<u8>> {
            let ops = self
                .0
                .lock()
                .await
                .replication_snapshot()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            postcard::to_allocvec(&ops).map_err(|e| std::io::Error::other(e.to_string()))
        }

        async fn restore(&self, data: Vec<u8>) -> std::io::Result<()> {
            let ops: Vec<WalOp> =
                postcard::from_bytes(&data).map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut db = self.0.lock().await;
            for name in db.collection_names() {
                db.drop_collection(&name)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
            for op in ops {
                db.apply_replicated(op)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
            Ok(())
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
        let bytes = serde_json::to_vec(&SnapshotPayload {
            sm: StateMachineData::default(),
            engine: Vec::new(),
        })
        .unwrap();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_transfers_engine_state_to_a_fresh_voter() {
        // A source engine holding data, captured into a state-machine snapshot
        // (ADR-0067 increment 4c: the snapshot carries engine state, not just Raft
        // metadata, so a far-behind / newly added voter can catch up from it).
        let src_dir = tempfile::tempdir().unwrap();
        let src = Arc::new(Mutex::new(Database::open(src_dir.path()).unwrap()));
        {
            let mut db = src.lock().await;
            db.create_collection("docs", Descriptor::new(4, Dtype::F32, DistanceMetric::L2))
                .unwrap();
            db.upsert("docs", "a", &[1.0, 0.0, 0.0, 0.0], &serde_json::json!({}))
                .unwrap();
            db.upsert("docs", "b", &[0.0, 1.0, 0.0, 0.0], &serde_json::json!({}))
                .unwrap();
        }
        let sm_src = Arc::new(StateMachineStore::new(EngineApplier(src.clone())));
        let snap = sm_src
            .clone()
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        let bytes = snap.snapshot.into_inner();

        // A fresh, empty target installs the snapshot — its engine is restored.
        let tgt_dir = tempfile::tempdir().unwrap();
        let tgt = Arc::new(Mutex::new(Database::open(tgt_dir.path()).unwrap()));
        let mut receiver = Arc::new(StateMachineStore::new(EngineApplier(tgt.clone())));
        receiver
            .install_snapshot(&snap.meta, Box::new(std::io::Cursor::new(bytes)))
            .await
            .unwrap();

        // The target now serves both points the source held.
        let params = SearchParams {
            k: 2,
            ef_search: 16,
            with_payload: false,
            with_vector: false,
            filter: None,
        };
        let hits = tgt
            .lock()
            .await
            .search("docs", &[1.0, 0.0, 0.0, 0.0], &params)
            .unwrap();
        let ids: std::collections::HashSet<_> = hits.iter().map(|m| m.id.clone()).collect();
        assert!(
            ids.contains("a") && ids.contains("b"),
            "the snapshot transferred the engine state to a fresh voter"
        );
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
            rpc: InstallSnapshotRequest<TypeConfig>,
            _option: RPCOption,
        ) -> Result<InstallSnapshotResponse<NodeId>, RpcError<InstallSnapshotError>> {
            // Forward to the target's snapshot-receive handler (4c): a voter that
            // is behind a compacted log catches up by installing a snapshot.
            let target = self.board.handle(self.target).ok_or_else(|| {
                RPCError::Unreachable(Unreachable::new(&std::io::Error::other("node down")))
            })?;
            target
                .install_snapshot(rpc)
                .await
                .map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_minority_cannot_commit_a_write() {
        // No split-brain (ADR-0067, 4b-iv): once a minority of voters is reachable,
        // the survivor cannot reach a quorum, so it cannot commit a write — while
        // still serving the data committed while the quorum was whole. The
        // Switchboard truly isolates a killed voter (a de-registered node is
        // `Unreachable`), so a minority here is a real partition — unlike a
        // whole-process kill, which cannot stop openraft's background core.
        let (board, voters) = boot_cluster(&[1, 2, 3]).await;
        voters[0]
            .raft
            .wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "bootstrap leader")
            .await
            .unwrap();

        // Commit a batch while all three voters form a quorum.
        let a = [1.0f32, 0.0, 0.0, 0.0];
        let ops = collection_ops(&[("a", a)]);
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
        for v in &voters {
            await_serves(&v.engine, &a, "a").await;
        }

        // Isolate a minority of one: kill voters 2 and 3. The survivor (voter 1)
        // cannot reach a quorum, which needs two.
        for v in &voters[1..] {
            board.kill(v.id);
            v.raft.shutdown().await.unwrap();
        }
        let survivor = &voters[0];

        // A new write cannot commit on the minority: `client_write` either errors
        // (no leader / forward-to-leader) or never resolves (a leader stepping down
        // after losing quorum), bounded by a timeout — but never succeeds.
        let op = WalOp::Upsert {
            collection_id: coll_id,
            external_id: "b".to_owned(),
            vector: [0.0f32, 1.0, 0.0, 0.0]
                .iter()
                .flat_map(|f| f.to_le_bytes())
                .collect(),
            payload: b"{}".to_vec(),
        };
        let committed =
            tokio::time::timeout(Duration::from_secs(3), survivor.raft.client_write(op)).await;
        assert!(
            matches!(committed, Err(_) | Ok(Err(_))),
            "a minority of one committed a write — split-brain"
        );

        // Safety, not just denial of liveness: the survivor still serves the data
        // committed before the partition.
        await_serves(&survivor.engine, &a, "a").await;
        survivor.raft.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_new_voter_catches_up_via_snapshot_after_compaction() {
        // The headline of log compaction (ADR-0067 increment 4c): a leader commits
        // data, snapshots, and PURGES its log — then a fresh voter added as a learner
        // can only catch up by INSTALLING the snapshot (the early log is gone), which
        // proves the snapshot carries engine state end to end and the transport
        // delivers it.
        let board = Switchboard::default();
        let cfg = Arc::new(
            Config {
                heartbeat_interval: 100,
                election_timeout_min: 300,
                election_timeout_max: 600,
                // Keep no post-snapshot log, so the purge leaves a fresh voter no
                // entries to replay — it must install the snapshot to catch up.
                max_in_snapshot_log_to_keep: 0,
                purge_batch_size: 1,
                ..Default::default()
            }
            .validate()
            .unwrap(),
        );

        let dir1 = tempfile::tempdir().unwrap();
        let e1 = Arc::new(Mutex::new(Database::open(dir1.path()).unwrap()));
        let r1 = openraft::Raft::new(
            1,
            cfg.clone(),
            board.clone(),
            LogStore::default(),
            Arc::new(StateMachineStore::new(EngineApplier(e1.clone()))),
        )
        .await
        .unwrap();
        board.register(1, r1.clone());
        let mut members = BTreeMap::new();
        members.insert(1, BasicNode::default());
        r1.initialize(members).await.unwrap();
        r1.wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "leader")
            .await
            .unwrap();

        let a = [1.0f32, 0.0, 0.0, 0.0];
        let b = [0.0f32, 1.0, 0.0, 0.0];
        for op in collection_ops(&[("a", a), ("b", b)]) {
            r1.client_write(op).await.unwrap();
        }

        // Snapshot, then purge the log up to the snapshot point.
        r1.trigger().snapshot().await.unwrap();
        let snap_index = loop {
            if let Some(s) = r1.metrics().borrow().snapshot {
                break s.index;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        r1.trigger().purge_log(snap_index).await.unwrap();

        // Add a fresh voter; with the log compacted it catches up via the snapshot.
        let dir2 = tempfile::tempdir().unwrap();
        let e2 = Arc::new(Mutex::new(Database::open(dir2.path()).unwrap()));
        let r2 = openraft::Raft::new(
            2,
            cfg.clone(),
            board.clone(),
            LogStore::default(),
            Arc::new(StateMachineStore::new(EngineApplier(e2.clone()))),
        )
        .await
        .unwrap();
        board.register(2, r2.clone());
        r1.add_learner(2, BasicNode::default(), true).await.unwrap();

        // The new voter now serves the snapshotted data it never saw in the log.
        await_serves(&e2, &a, "a").await;
        await_serves(&e2, &b, "b").await;

        r1.shutdown().await.unwrap();
        r2.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_partitioned_voter_rejoins_and_catches_up() {
        // Partition / rejoin (ADR-0067 increment 4d): a voter cut off from the group
        // misses the writes the surviving majority commits, then on healing rejoins
        // and catches up to the full state — no acknowledged write is lost, and the
        // group reconverges.
        let (board, voters) = boot_cluster(&[1, 2, 3]).await;
        voters[0]
            .raft
            .wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "bootstrap leader")
            .await
            .unwrap();

        let a = [1.0f32, 0.0, 0.0, 0.0];
        let b = [0.0f32, 1.0, 0.0, 0.0];
        let ops = collection_ops(&[("a", a)]);
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
        for v in &voters {
            await_serves(&v.engine, &a, "a").await;
        }

        // Partition voter 3 away — peers can no longer reach it, so it misses what
        // the majority commits next.
        let isolated = 3;
        board.kill(isolated);

        // The majority {1,2} still has a quorum and keeps committing.
        let b_op = WalOp::Upsert {
            collection_id: coll_id,
            external_id: "b".to_owned(),
            vector: b.iter().flat_map(|f| f.to_le_bytes()).collect(),
            payload: b"{}".to_vec(),
        };
        commit(&board, &voters, &b_op).await;
        for v in voters.iter().filter(|v| v.id != isolated) {
            await_serves(&v.engine, &b, "b").await;
        }

        // Heal the partition: voter 3 reconnects and catches up to the write it
        // missed, reconverging with the group.
        board.register(isolated, voters[2].raft.clone());
        await_serves(&voters[2].engine, &a, "a").await;
        await_serves(&voters[2].engine, &b, "b").await;

        for v in &voters {
            v.raft.shutdown().await.unwrap();
        }
    }
}
