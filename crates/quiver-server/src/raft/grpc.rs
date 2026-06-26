// SPDX-License-Identifier: AGPL-3.0-only
//! gRPC transport for the per-shard Raft group (ADR-0067, increment 4b).
//!
//! Carries openraft's append-entries / vote / install-snapshot RPCs between a
//! shard's members over the same tonic stack the server already runs (the
//! `quiver.v1.RaftService`). Each RPC's openraft request is **postcard**-encoded
//! — the WAL's own format, which already round-trips [`WalOp`](quiver_core::WalOp)
//! — into a `RaftEnvelope { data }`. A response envelope carries the handler's
//! `Result<Response, RaftError>`, so a Raft-layer error at the peer reaches the
//! caller as a [`RemoteError`] (mirroring openraft's reference HTTP transport);
//! a transport failure (a down or unreachable peer) becomes [`Unreachable`], so
//! openraft elects around it.
//!
//! A node serves this only while it is a Raft voter (via [`RaftRpc`]); the
//! [`GrpcRaftNetwork`] dials peers at the address openraft carries in each
//! member's [`BasicNode`], over a lazily-connected channel.

use std::io;

use openraft::BasicNode;
use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RemoteError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Response, Status};

use quiver_proto::v1::RaftEnvelope;
use quiver_proto::v1::raft_service_client::RaftServiceClient;
use quiver_proto::v1::raft_service_server::{RaftService, RaftServiceServer};

use super::{NodeId, Raft, RaftError, RpcError, TypeConfig};

// postcard is the engine's own WAL codec, so it already round-trips `WalOp` (the
// Raft application data); reusing it keeps the consensus wire compact.
fn to_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, Status> {
    postcard::to_allocvec(value).map_err(|e| Status::internal(format!("raft encode: {e}")))
}

fn from_bytes<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Status> {
    postcard::from_bytes(bytes).map_err(|e| Status::internal(format!("raft decode: {e}")))
}

/// The server side: handle a peer's Raft RPC by decoding it, invoking this node's
/// local Raft handler, and returning the (possibly error) result encoded back.
pub struct RaftRpc {
    raft: Raft,
}

impl RaftRpc {
    /// Wrap a node's Raft handle as a gRPC service.
    pub fn service(raft: Raft) -> RaftServiceServer<Self> {
        RaftServiceServer::new(Self { raft })
    }
}

#[tonic::async_trait]
impl RaftService for RaftRpc {
    async fn append_entries(
        &self,
        request: Request<RaftEnvelope>,
    ) -> Result<Response<RaftEnvelope>, Status> {
        let rpc: AppendEntriesRequest<TypeConfig> = from_bytes(&request.into_inner().data)?;
        let result = self.raft.append_entries(rpc).await;
        Ok(Response::new(RaftEnvelope {
            data: to_bytes(&result)?,
        }))
    }

    async fn vote(&self, request: Request<RaftEnvelope>) -> Result<Response<RaftEnvelope>, Status> {
        let rpc: VoteRequest<NodeId> = from_bytes(&request.into_inner().data)?;
        let result = self.raft.vote(rpc).await;
        Ok(Response::new(RaftEnvelope {
            data: to_bytes(&result)?,
        }))
    }

    async fn install_snapshot(
        &self,
        request: Request<RaftEnvelope>,
    ) -> Result<Response<RaftEnvelope>, Status> {
        let rpc: InstallSnapshotRequest<TypeConfig> = from_bytes(&request.into_inner().data)?;
        let result = self.raft.install_snapshot(rpc).await;
        Ok(Response::new(RaftEnvelope {
            data: to_bytes(&result)?,
        }))
    }
}

/// The client side: an openraft [`RaftNetworkFactory`] that dials a shard's other
/// members over the `RaftService`, addressing each by the URL openraft carries in
/// its [`BasicNode`].
#[derive(Clone, Default)]
pub struct GrpcRaftNetwork;

impl RaftNetworkFactory<TypeConfig> for GrpcRaftNetwork {
    type Network = GrpcLink;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> GrpcLink {
        // Lazy: the channel connects on first use, so an unreachable peer surfaces
        // as a transport error on the RPC (mapped to `Unreachable`) rather than
        // failing client construction (which openraft does not expect to fail).
        let channel = Endpoint::from_shared(node.addr.clone())
            .ok()
            .map(|endpoint| endpoint.connect_lazy());
        GrpcLink { target, channel }
    }
}

/// A connection to one peer's `RaftService`.
pub struct GrpcLink {
    target: NodeId,
    channel: Option<Channel>,
}

impl GrpcLink {
    fn channel(&self) -> Result<Channel, Unreachable> {
        self.channel
            .clone()
            .ok_or_else(|| Unreachable::new(&io::Error::other("no channel to peer")))
    }
}

// A failed RPC over the wire means the peer is (currently) unreachable; openraft
// backs off and elects around it.
fn peer_unreachable<E>(status: tonic::Status) -> RPCError<NodeId, BasicNode, RaftError<E>>
where
    E: std::error::Error,
{
    RPCError::Unreachable(Unreachable::new(&status))
}

fn network_err<E, S>(e: &S) -> RPCError<NodeId, BasicNode, RaftError<E>>
where
    E: std::error::Error,
    S: std::error::Error + 'static,
{
    RPCError::Network(NetworkError::new(e))
}

impl RaftNetwork<TypeConfig> for GrpcLink {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RpcError> {
        let channel = self.channel().map_err(RPCError::Unreachable)?;
        let data = postcard::to_allocvec(&rpc).map_err(|e| network_err(&e))?;
        let resp = RaftServiceClient::new(channel)
            .append_entries(RaftEnvelope { data })
            .await
            .map_err(peer_unreachable)?;
        let result: Result<AppendEntriesResponse<NodeId>, RaftError> =
            postcard::from_bytes(&resp.into_inner().data).map_err(|e| network_err(&e))?;
        result.map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RpcError> {
        let channel = self.channel().map_err(RPCError::Unreachable)?;
        let data = postcard::to_allocvec(&rpc).map_err(|e| network_err(&e))?;
        let resp = RaftServiceClient::new(channel)
            .vote(RaftEnvelope { data })
            .await
            .map_err(peer_unreachable)?;
        let result: Result<VoteResponse<NodeId>, RaftError> =
            postcard::from_bytes(&resp.into_inner().data).map_err(|e| network_err(&e))?;
        result.map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<InstallSnapshotResponse<NodeId>, RpcError<InstallSnapshotError>> {
        let channel = self.channel().map_err(RPCError::Unreachable)?;
        let data = postcard::to_allocvec(&rpc).map_err(|e| network_err(&e))?;
        let resp = RaftServiceClient::new(channel)
            .install_snapshot(RaftEnvelope { data })
            .await
            .map_err(peer_unreachable)?;
        let result: Result<InstallSnapshotResponse<NodeId>, RaftError<InstallSnapshotError>> =
            postcard::from_bytes(&resp.into_inner().data).map_err(|e| network_err(&e))?;
        result.map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use openraft::{Config, ServerState};
    use quiver_core::WalOp;
    use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, SearchParams};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;
    use tokio_stream::wrappers::TcpListenerStream;

    use super::super::{ApplyOp, LogStore, StateMachineStore};
    use super::*;

    /// Drives committed ops into a real engine (the ADR-0030 apply seam).
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

    /// One Raft voter running a real `RaftService` over loopback gRPC.
    struct Node {
        id: NodeId,
        raft: Raft,
        engine: Arc<Mutex<Database>>,
        server: JoinHandle<()>,
        _dir: tempfile::TempDir,
    }

    // Boot an `ids`-member Raft group, each node serving the gRPC `RaftService`
    // on a loopback port and dialing peers via `GrpcRaftNetwork`.
    async fn boot(ids: &[NodeId]) -> Vec<Node> {
        // Bind every node's listener first, so the member set (addresses) is known
        // before any node is constructed.
        let mut listeners = Vec::new();
        let mut members = BTreeMap::new();
        for &id in ids {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            members.insert(id, BasicNode::new(format!("http://{addr}")));
            listeners.push((id, listener));
        }

        let mut nodes = Vec::new();
        for (id, listener) in listeners {
            let dir = tempfile::tempdir().unwrap();
            let engine = Arc::new(Mutex::new(Database::open(dir.path()).unwrap()));
            let config = Arc::new(
                Config {
                    heartbeat_interval: 150,
                    election_timeout_min: 400,
                    election_timeout_max: 800,
                    ..Default::default()
                }
                .validate()
                .unwrap(),
            );
            let sm = Arc::new(StateMachineStore::new(EngineApplier(engine.clone())));
            let raft = openraft::Raft::new(id, config, GrpcRaftNetwork, LogStore::default(), sm)
                .await
                .unwrap();
            let service = RaftRpc::service(raft.clone());
            let server = tokio::spawn(async move {
                let _ = tonic::transport::Server::builder()
                    .add_service(service)
                    .serve_with_incoming(TcpListenerStream::new(listener))
                    .await;
            });
            nodes.push(Node {
                id,
                raft,
                engine,
                server,
                _dir: dir,
            });
        }
        nodes[0].raft.initialize(members).await.unwrap();
        nodes
    }

    fn leader_of(nodes: &[&Node]) -> Option<NodeId> {
        for n in nodes {
            let leader = n.raft.metrics().borrow().current_leader;
            if let Some(leader) = leader
                && nodes.iter().any(|n| n.id == leader)
            {
                return Some(leader);
            }
        }
        None
    }

    async fn commit(nodes: &[&Node], op: &WalOp) {
        for _ in 0..150 {
            if let Some(leader_id) = leader_of(nodes)
                && let Some(leader) = nodes.iter().find(|n| n.id == leader_id)
                && leader.raft.client_write(op.clone()).await.is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        panic!("no leader committed the op within the budget");
    }

    async fn await_serves(engine: &Arc<Mutex<Database>>, query: &[f32], want_id: &str) {
        let params = SearchParams {
            k: 5,
            filter: None,
            ef_search: 32,
            with_payload: false,
            with_vector: false,
        };
        for _ in 0..250 {
            if let Ok(hits) = engine.lock().await.search("docs", query, &params)
                && hits.iter().any(|m| m.id == want_id)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        panic!("engine never served {want_id}");
    }

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
    async fn grpc_group_replicates_and_survives_leader_loss() {
        let nodes = boot(&[1, 2, 3]).await;
        let all: Vec<&Node> = nodes.iter().collect();
        nodes[0]
            .raft
            .wait(Some(Duration::from_secs(15)))
            .state(ServerState::Leader, "bootstrap leader over grpc")
            .await
            .unwrap();

        // Commit a batch through the leader; every voter applies it over the wire.
        let a = [1.0f32, 0.0, 0.0, 0.0];
        let b = [0.0f32, 1.0, 0.0, 0.0];
        let ops = collection_ops(&[("a", a), ("b", b)]);
        let coll_id = ops
            .iter()
            .find_map(|op| match op {
                WalOp::CreateCollection { collection_id, .. } => Some(*collection_id),
                _ => None,
            })
            .expect("create-collection op");
        for op in &ops {
            commit(&all, op).await;
        }
        for n in &nodes {
            await_serves(&n.engine, &a, "a").await;
        }

        // Kill the leader's process: stop its gRPC server and shut down its Raft.
        // Peers see the connection drop (Unreachable) and elect a new leader.
        let dead = leader_of(&all).expect("a leader exists");
        let dead_node = nodes.iter().find(|n| n.id == dead).unwrap();
        dead_node.server.abort();
        dead_node.raft.shutdown().await.unwrap();
        let survivors: Vec<&Node> = nodes.iter().filter(|n| n.id != dead).collect();

        // A post-failover write is acknowledged and applied on the survivors.
        let c = [0.0f32, 0.0, 1.0, 0.0];
        let c_op = WalOp::Upsert {
            collection_id: coll_id,
            external_id: "c".to_owned(),
            vector: c.iter().flat_map(|f| f.to_le_bytes()).collect(),
            payload: b"{}".to_vec(),
        };
        commit(&survivors, &c_op).await;
        for n in &survivors {
            await_serves(&n.engine, &a, "a").await;
            await_serves(&n.engine, &c, "c").await;
        }

        for n in survivors {
            n.raft.shutdown().await.unwrap();
            n.server.abort();
        }
    }
}
