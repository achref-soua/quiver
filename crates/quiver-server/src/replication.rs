// SPDX-License-Identifier: AGPL-3.0-only
//! Replication follower (ADR-0030): a background task that connects to a leader's
//! `Replicate` stream, applies each committed op to the local engine, and serves
//! reads. The leader side (the `Replicate` RPC + commit broadcast) lives in
//! `grpc.rs` / `lib.rs`.

use quiver_embed::{CollectionId, WalOp};
use quiver_proto::v1::{self, quiver_client::QuiverClient};

use crate::AppState;

/// Spawn the follower loop: connect to `leader_url` and apply its op stream to
/// `state`. On any stream error the task logs and exits — the node keeps serving
/// its (now stale) read-only state; an operator restarts it to re-bootstrap.
pub(crate) fn spawn_follower(state: AppState, leader_url: String, api_key: Option<String>) {
    tokio::spawn(async move {
        match follow(&state, &leader_url, api_key.as_deref()).await {
            Ok(()) => tracing::warn!(
                leader = %leader_url,
                "replication stream ended; follower is now serving stale read-only state (restart to re-sync)"
            ),
            Err(e) => tracing::error!(
                leader = %leader_url,
                error = %e,
                "replication follower stopped; serving stale read-only state (restart to re-sync)"
            ),
        }
    });
}

// Connect, open the stream, and apply every op until it ends or errors.
async fn follow(state: &AppState, leader_url: &str, api_key: Option<&str>) -> Result<(), String> {
    let mut client = QuiverClient::connect(leader_url.to_owned())
        .await
        .map_err(|e| format!("connecting to leader: {e}"))?;
    let mut request = tonic::Request::new(v1::ReplicateRequest {});
    if let Some(key) = api_key {
        let value = format!("Bearer {key}")
            .parse()
            .map_err(|_| "invalid leader api key".to_owned())?;
        request.metadata_mut().insert("authorization", value);
    }
    let mut stream = client
        .replicate(request)
        .await
        .map_err(|e| format!("opening replication stream: {e}"))?
        .into_inner();
    tracing::info!(leader = %leader_url, "replication follower connected");
    while let Some(op) = stream
        .message()
        .await
        .map_err(|e| format!("reading replication stream: {e}"))?
    {
        if let Some(wal_op) = proto_to_op(op) {
            state
                .apply_replicated(wal_op)
                .await
                .map_err(|e| format!("applying replicated op: {e}"))?;
        }
    }
    Ok(())
}

// The reverse of `grpc::repl_op_to_proto`: a wire op back into the engine's WalOp.
fn proto_to_op(op: v1::ReplicationOp) -> Option<WalOp> {
    use v1::replication_op::Op;
    Some(match op.op? {
        Op::CreateCollection(c) => WalOp::CreateCollection {
            collection_id: CollectionId(c.collection_id),
            name: c.name,
            descriptor: c.descriptor,
        },
        Op::DropCollection(d) => WalOp::DropCollection {
            collection_id: CollectionId(d.collection_id),
        },
        Op::Upsert(u) => WalOp::Upsert {
            collection_id: CollectionId(u.collection_id),
            external_id: u.external_id,
            vector: u.vector,
            payload: u.payload,
        },
        Op::Delete(d) => WalOp::Delete {
            collection_id: CollectionId(d.collection_id),
            external_id: d.external_id,
        },
    })
}
