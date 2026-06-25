// SPDX-License-Identifier: AGPL-3.0-only
//! An MCP server exposing Quiver as agent tools (ADR-0018).
//!
//! Speaks the [Model Context Protocol](https://modelcontextprotocol.io) —
//! JSON-RPC 2.0 over newline-delimited stdio — so an AI agent can create
//! collections, upsert vectors, and run filtered nearest-neighbor queries
//! against an in-process [`Database`]. The protocol dispatch ([`handle_message`])
//! is separated from the stdio loop ([`serve`]) so it is unit-testable without
//! real pipes.
//!
//! Tools: `list_collections`, `create_collection`, `collection_info`,
//! `database_stats`, `delete_collection`, `snapshot`, `upsert`, `search`,
//! `hybrid_search`, `fetch`, `get`, `delete`, the multi-vector document tools,
//! and — when an embedding provider is configured (ADR-0058) — the text tools
//! `upsert_text` / `search_text`, which embed text server-side so an agent never
//! runs an embedding model itself. Enough for an agent to *operate* the database
//! (inspect, manage, back up, clean up), not just query it.
//!
//! The text tools read their provider configuration from a Quiver TOML config
//! file (`[embedding.<collection>]` / `[rerank.<collection>]`, the same tables
//! `quiver serve` uses) passed to [`run_with_config`]; with no config they are
//! advertised but return a clear "no embedding provider configured" error.
//! The database is opened secure-by-default (encryption-at-rest on
//! unless explicitly insecure) through the same envelope key-ring as the network
//! server and `quiver admin`, so a data directory is interchangeable between them.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{Value, json};

use quiver_embed::{
    DEFAULT_RRF_K0, Database, Descriptor, DistanceMetric, Dtype, FieldType, Filter,
    FilterableField, IndexKind, IndexSpec, SearchParams, SparseVector, TEXT_KEY, VectorEncryption,
};
use quiver_providers::EmbedRegistry;

/// When reranking a text search, over-fetch this many candidates so the reranker
/// has a wide pool to reorder down to the requested `k` (mirrors the network
/// server's `RERANK_CANDIDATES`).
const RERANK_CANDIDATES: usize = 50;

/// The MCP protocol revision this server implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Open the embedded database at `data_dir`, encrypted at rest unless `insecure`.
///
/// Opens through [`quiver_crypto::open_keyring`], the same envelope key-ring the
/// network server and `quiver admin` use, so a data directory is interchangeable
/// between `quiver serve`, `quiver mcp`, and `quiver admin`.
///
/// # Errors
/// Returns an error if no encryption key is provided and `insecure` is false, if
/// the key is invalid, or if the database cannot be opened.
pub fn open(
    data_dir: &Path,
    encryption_key: Option<&str>,
    insecure: bool,
) -> anyhow::Result<Database> {
    let db = match quiver_crypto::open_keyring(data_dir, encryption_key, insecure)? {
        Some(keyring) => Database::open_with_keyring(data_dir, keyring)?,
        None => Database::open(data_dir)?,
    };
    Ok(db)
}

/// Open the database at `data_dir` (encrypted at rest unless `insecure`) and
/// serve MCP over stdin/stdout until the input stream closes.
///
/// # Errors
/// Returns an error if the database cannot be opened (see [`open`]) or on an I/O
/// failure.
pub fn run(data_dir: &Path, encryption_key: Option<&str>, insecure: bool) -> anyhow::Result<()> {
    run_with_config(data_dir, encryption_key, insecure, None)
}

/// Like [`run`], but also load embedding/rerank providers from `config_path` (a
/// Quiver TOML config), enabling the `upsert_text` / `search_text` tools for any
/// collection with an `[embedding.<collection>]` table. A `None` path — or a
/// missing file — runs with no providers (the text tools then error when used).
///
/// # Errors
/// Returns an error if the config is malformed or names an unbuildable provider
/// (e.g. a missing required API key), or if the database cannot be opened.
pub fn run_with_config(
    data_dir: &Path,
    encryption_key: Option<&str>,
    insecure: bool,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let embed = match config_path {
        Some(path) => EmbedRegistry::from_toml_path(path)
            .map_err(|e| anyhow::anyhow!("loading embedding providers from config: {e}"))?,
        None => EmbedRegistry::default(),
    };
    let mut db = open(data_dir, encryption_key, insecure)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_with_embed(&mut db, &embed, stdin.lock(), stdout.lock())?;
    Ok(())
}

/// Read newline-delimited JSON-RPC messages from `reader`, dispatch each against
/// `db`, and write any responses to `writer`. Notifications (no `id`) yield no
/// response. Returns only on input EOF or an I/O error.
///
/// # Errors
/// Propagates I/O errors from reading or writing the streams.
pub fn serve(db: &mut Database, reader: impl BufRead, writer: impl Write) -> std::io::Result<()> {
    serve_with_embed(db, &EmbedRegistry::default(), reader, writer)
}

/// Like [`serve`], but with an [`EmbedRegistry`] backing the `upsert_text` /
/// `search_text` tools. [`serve`] is this with an empty registry.
///
/// # Errors
/// Propagates I/O errors from reading or writing the streams.
pub fn serve_with_embed(
    db: &mut Database,
    embed: &EmbedRegistry,
    reader: impl BufRead,
    mut writer: impl Write,
) -> std::io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle_message_with_embed(db, embed, &msg),
            Err(e) => Some(error_response(
                &Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        if let Some(resp) = response {
            let text = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_owned());
            writeln!(writer, "{text}")?;
            writer.flush()?;
        }
    }
    Ok(())
}

/// Dispatch one JSON-RPC message, returning the response (or `None` for a
/// notification). Protocol errors (bad method/params) are JSON-RPC errors; tool
/// execution failures are returned as a normal result with `isError: true` so
/// the agent can read and react to them (the MCP convention).
#[must_use]
pub fn handle_message(db: &mut Database, msg: &Value) -> Option<Value> {
    handle_message_with_embed(db, &EmbedRegistry::default(), msg)
}

/// Like [`handle_message`], but with an [`EmbedRegistry`] so a `tools/call` for
/// `upsert_text` / `search_text` can embed text. [`handle_message`] is this with
/// an empty registry (those tools then return a clear "no provider" error).
#[must_use]
pub fn handle_message_with_embed(
    db: &mut Database,
    embed: &EmbedRegistry,
    msg: &Value,
) -> Option<Value> {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let is_notification = msg.get("id").is_none();
    let method = msg.get("method").and_then(Value::as_str);

    match method {
        Some("initialize") => Some(success(&id, initialize_result())),
        Some("ping") => Some(success(&id, json!({}))),
        Some("tools/list") => Some(success(&id, json!({ "tools": tool_definitions() }))),
        Some("tools/call") => Some(handle_tool_call(db, embed, &id, msg.get("params"))),
        // Notifications such as `notifications/initialized` need no response.
        Some(_) if is_notification => None,
        Some(other) => Some(error_response(
            &id,
            -32601,
            &format!("method not found: {other}"),
        )),
        None if is_notification => None,
        None => Some(error_response(
            &id,
            -32600,
            "invalid request: missing method",
        )),
    }
}

fn handle_tool_call(
    db: &mut Database,
    embed: &EmbedRegistry,
    id: &Value,
    params: Option<&Value>,
) -> Value {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        return error_response(id, -32602, "tools/call requires a tool name");
    };
    let empty = json!({});
    let args = params.and_then(|p| p.get("arguments")).unwrap_or(&empty);
    match call_tool_embed(db, embed, name, args) {
        Ok(text) => success(id, tool_result(&text, false)),
        Err(message) => success(id, tool_result(&message, true)),
    }
}

/// Execute a tool with no embedding providers configured. Kept for callers and
/// tests that do not exercise the text tools; delegates to [`call_tool_embed`]
/// with an empty registry.
#[cfg(test)]
fn call_tool(db: &mut Database, name: &str, args: &Value) -> Result<String, String> {
    call_tool_embed(db, &EmbedRegistry::default(), name, args)
}

/// Execute a tool, returning its text content or an error message. `embed` backs
/// the `upsert_text` / `search_text` tools; all other tools ignore it.
fn call_tool_embed(
    db: &mut Database,
    embed: &EmbedRegistry,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    match name {
        "list_collections" => to_text(&json!({ "collections": db.collection_names() })),
        "create_collection" => {
            let collection = want_str(args, "name")?;
            let dim = want_u64(args, "dim")? as u32;
            let metric = want_metric(args)?;
            let index = want_index_spec(args)?;
            let filterable = want_filterable(args)?;
            let multivector = args
                .get("multivector")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let vector_encryption = match args.get("vector_encryption").and_then(Value::as_str) {
                Some("dcpe") => VectorEncryption::Dcpe,
                Some("client_side") => VectorEncryption::ClientSide,
                _ => VectorEncryption::None,
            };
            let descriptor = Descriptor::new(dim, Dtype::F32, metric)
                .with_index(index)
                .with_filterable(filterable)
                .with_multivector(multivector)
                .with_vector_encryption(vector_encryption);
            db.create_collection(collection, descriptor)
                .map_err(|e| e.to_string())?;
            Ok(format!("created collection '{collection}' (dim {dim})"))
        }
        "upsert" => {
            let collection = want_str(args, "collection")?;
            let point_id = want_str(args, "id")?;
            let vector = want_vector(args, "vector")?;
            let payload = args.get("payload").cloned().unwrap_or_else(|| json!({}));
            db.upsert(collection, point_id, &vector, &payload)
                .map_err(|e| e.to_string())?;
            Ok(format!("upserted '{point_id}' into '{collection}'"))
        }
        "search" => {
            let collection = want_str(args, "collection")?;
            let vector = want_vector(args, "vector")?;
            let k = args.get("k").and_then(Value::as_u64).unwrap_or(10) as usize;
            let filter = match args.get("filter") {
                Some(f) if !f.is_null() => Some(
                    serde_json::from_value::<Filter>(f.clone())
                        .map_err(|e| format!("invalid filter: {e}"))?,
                ),
                _ => None,
            };
            let params = SearchParams {
                k,
                filter,
                ..SearchParams::default()
            };
            let matches = db
                .search(collection, &vector, &params)
                .map_err(|e| e.to_string())?;
            let rendered: Vec<Value> = matches
                .iter()
                .map(|m| json!({ "id": m.id, "score": m.score, "payload": m.payload }))
                .collect();
            to_text(&json!({ "matches": rendered }))
        }
        "hybrid_search" => {
            let collection = want_str(args, "collection")?;
            let dense = match args.get("vector") {
                Some(v) if !v.is_null() => Some(want_vector(args, "vector")?),
                _ => None,
            };
            let sparse = match (args.get("sparse_indices"), args.get("sparse_values")) {
                (Some(i), Some(v)) if !i.is_null() && !v.is_null() => {
                    let indices: Vec<u32> = serde_json::from_value(i.clone())
                        .map_err(|e| format!("invalid sparse_indices: {e}"))?;
                    let values: Vec<f32> = serde_json::from_value(v.clone())
                        .map_err(|e| format!("invalid sparse_values: {e}"))?;
                    Some(SparseVector { indices, values })
                }
                (None, None) => None,
                _ => {
                    return Err(
                        "sparse_indices and sparse_values must be provided together".to_owned()
                    );
                }
            };
            let k = args.get("k").and_then(Value::as_u64).unwrap_or(10) as usize;
            let filter = match args.get("filter") {
                Some(f) if !f.is_null() => Some(
                    serde_json::from_value::<Filter>(f.clone())
                        .map_err(|e| format!("invalid filter: {e}"))?,
                ),
                _ => None,
            };
            let rrf_k0 = args
                .get("rrf_k0")
                .and_then(Value::as_f64)
                .map_or(DEFAULT_RRF_K0, |x| x as f32);
            let query_text = args
                .get("query_text")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let params = SearchParams {
                k,
                filter,
                ..SearchParams::default()
            };
            let matches = db
                .hybrid_search(
                    collection,
                    dense.as_deref(),
                    sparse.as_ref(),
                    query_text.as_deref(),
                    &params,
                    rrf_k0,
                )
                .map_err(|e| e.to_string())?;
            let rendered: Vec<Value> = matches
                .iter()
                .map(|m| json!({ "id": m.id, "score": m.score, "payload": m.payload }))
                .collect();
            to_text(&json!({ "matches": rendered }))
        }
        "fetch" => {
            let collection = want_str(args, "collection")?;
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
            let filter = match args.get("filter") {
                Some(f) if !f.is_null() => Some(
                    serde_json::from_value::<Filter>(f.clone())
                        .map_err(|e| format!("invalid filter: {e}"))?,
                ),
                _ => None,
            };
            let points = db
                .fetch(collection, filter.as_ref(), 0, limit, true, false)
                .map_err(|e| e.to_string())?;
            let rendered: Vec<Value> = points
                .iter()
                .map(|m| json!({ "id": m.id, "payload": m.payload }))
                .collect();
            to_text(&json!({ "points": rendered }))
        }
        "get" => {
            let collection = want_str(args, "collection")?;
            let point_id = want_str(args, "id")?;
            match db.get(collection, point_id).map_err(|e| e.to_string())? {
                Some(m) => {
                    to_text(&json!({ "id": m.id, "payload": m.payload, "vector": m.vector }))
                }
                None => Ok(format!("point '{point_id}' not found in '{collection}'")),
            }
        }
        "delete" => {
            let collection = want_str(args, "collection")?;
            let point_id = want_str(args, "id")?;
            let existed = db.delete(collection, point_id).map_err(|e| e.to_string())?;
            Ok(if existed {
                format!("deleted '{point_id}' from '{collection}'")
            } else {
                format!("'{point_id}' was not present in '{collection}'")
            })
        }
        "upsert_document" => {
            let collection = want_str(args, "collection")?;
            let doc_id = want_str(args, "id")?;
            let vectors = want_vectors(args, "vectors")?;
            let payload = args.get("payload").cloned().unwrap_or_else(|| json!({}));
            db.upsert_document(collection, doc_id, &vectors, &payload)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "upserted document '{doc_id}' ({} tokens) into '{collection}'",
                vectors.len()
            ))
        }
        "search_multi_vector" => {
            let collection = want_str(args, "collection")?;
            let query = want_vectors(args, "query")?;
            let k = args.get("k").and_then(Value::as_u64).unwrap_or(10) as usize;
            let filter = match args.get("filter") {
                Some(f) if !f.is_null() => Some(
                    serde_json::from_value::<Filter>(f.clone())
                        .map_err(|e| format!("invalid filter: {e}"))?,
                ),
                _ => None,
            };
            let params = SearchParams {
                k,
                filter,
                ..SearchParams::default()
            };
            let matches = db
                .search_multi_vector(collection, &query, &params)
                .map_err(|e| e.to_string())?;
            let rendered: Vec<Value> = matches
                .iter()
                .map(|m| json!({ "id": m.id, "score": m.score, "payload": m.payload }))
                .collect();
            to_text(&json!({ "matches": rendered }))
        }
        "delete_document" => {
            let collection = want_str(args, "collection")?;
            let doc_id = want_str(args, "id")?;
            let existed = db
                .delete_document(collection, doc_id)
                .map_err(|e| e.to_string())?;
            Ok(if existed {
                format!("deleted document '{doc_id}' from '{collection}'")
            } else {
                format!("document '{doc_id}' was not present in '{collection}'")
            })
        }
        "collection_info" => {
            let collection = want_str(args, "collection")?;
            to_text(&collection_summary(db, collection)?)
        }
        "delete_collection" => {
            let collection = want_str(args, "collection")?;
            let existed = db.drop_collection(collection).map_err(|e| e.to_string())?;
            to_text(&json!({ "collection": collection, "existed": existed }))
        }
        "database_stats" => {
            let names = db.collection_names();
            let mut collections = Vec::with_capacity(names.len());
            let mut total_points: u64 = 0;
            for name in &names {
                let summary = collection_summary(db, name)?;
                total_points += summary["count"].as_u64().unwrap_or(0);
                collections.push(summary);
            }
            to_text(&json!({
                "collection_count": names.len(),
                "total_points": total_points,
                // Snapshot-relevant status (ADR-0050): the catalog generation a
                // snapshot would capture and the data directory's on-disk size.
                "manifest_version": db.manifest_version(),
                "disk_bytes": db.disk_usage_bytes(),
                "collections": collections,
            }))
        }
        "snapshot" => {
            let destination = want_str(args, "destination")?;
            let info = db
                .snapshot(std::path::Path::new(destination))
                .map_err(|e| e.to_string())?;
            to_text(&json!({
                "destination": destination,
                "manifest_version": info.manifest_version,
                "files": info.files,
                "bytes": info.bytes,
            }))
        }
        "upsert_text" => {
            let collection = want_str(args, "collection")?;
            let point_id = want_str(args, "id")?;
            let text = want_str(args, "text")?;
            let embedder = embed
                .embedder(collection)
                .ok_or_else(|| no_provider_message(collection))?;
            let vector = embedder
                .embed(&[text.to_owned()])
                .map_err(|e| e.to_string())?
                .into_iter()
                .next()
                .ok_or("embedding provider returned no vector")?;
            // Co-populate the full-text key (ADR-0046) so one call feeds both the
            // dense index and BM25, without clobbering a caller-supplied text key.
            let mut payload = match args.get("payload").cloned() {
                Some(Value::Object(map)) => map,
                _ => serde_json::Map::new(),
            };
            payload
                .entry(TEXT_KEY.to_owned())
                .or_insert_with(|| Value::String(text.to_owned()));
            db.upsert(collection, point_id, &vector, &Value::Object(payload))
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "embedded and upserted '{point_id}' into '{collection}'"
            ))
        }
        "search_text" => {
            let collection = want_str(args, "collection")?;
            let text = want_str(args, "text")?;
            let k = args.get("k").and_then(Value::as_u64).unwrap_or(10) as usize;
            let filter = match args.get("filter") {
                Some(f) if !f.is_null() => Some(
                    serde_json::from_value::<Filter>(f.clone())
                        .map_err(|e| format!("invalid filter: {e}"))?,
                ),
                _ => None,
            };
            let rrf_k0 = args
                .get("rrf_k0")
                .and_then(Value::as_f64)
                .map_or(DEFAULT_RRF_K0, |x| x as f32);
            let want_rerank = args.get("rerank").and_then(Value::as_bool).unwrap_or(false);
            let embedder = embed
                .embedder(collection)
                .ok_or_else(|| no_provider_message(collection))?;
            let vector = embedder
                .embed(&[text.to_owned()])
                .map_err(|e| e.to_string())?
                .into_iter()
                .next()
                .ok_or("embedding provider returned no vector")?;
            // Rerank only when asked *and* a reranker is configured; over-fetch a
            // wide candidate set so it has something to reorder.
            let reranker = if want_rerank {
                embed.reranker(collection)
            } else {
                None
            };
            let fetch_k = if reranker.is_some() {
                k.max(RERANK_CANDIDATES)
            } else {
                k
            };
            let params = SearchParams {
                k: fetch_k,
                filter,
                ..SearchParams::default()
            };
            let mut matches = db
                .hybrid_search(
                    collection,
                    Some(vector.as_slice()),
                    None,
                    Some(text),
                    &params,
                    rrf_k0,
                )
                .map_err(|e| e.to_string())?;
            if let Some(rr) = reranker {
                let docs: Vec<String> = matches
                    .iter()
                    .map(|m| doc_text(m.payload.as_ref()))
                    .collect();
                let scores = rr.rerank(text, &docs).map_err(|e| e.to_string())?;
                let mut scored: Vec<(f32, _)> = scores.into_iter().zip(matches).collect();
                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                matches = scored
                    .into_iter()
                    .map(|(s, mut m)| {
                        m.score = s;
                        m
                    })
                    .collect();
            }
            matches.truncate(k);
            let rendered: Vec<Value> = matches
                .iter()
                .map(|m| json!({ "id": m.id, "score": m.score, "payload": m.payload }))
                .collect();
            to_text(&json!({ "matches": rendered }))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// The error an agent sees when a text tool is used on a collection with no
/// `[embedding.<collection>]` provider configured.
fn no_provider_message(collection: &str) -> String {
    format!(
        "collection '{collection}' has no embedding provider configured \
         (add an [embedding.{collection}] table to the Quiver config passed to \
         `quiver mcp --config` — ADR-0047/0058)"
    )
}

/// The text a reranker scores for a hit: the `__quiver_text__` payload field if
/// present (what `upsert_text` stores), else the whole payload stringified.
fn doc_text(payload: Option<&Value>) -> String {
    match payload {
        Some(Value::Object(map)) => map
            .get(TEXT_KEY)
            .and_then(Value::as_str)
            .map_or_else(|| Value::Object(map.clone()).to_string(), str::to_owned),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// A one-collection summary — shape, index, encryption, and live point/document
/// count — shared by the `collection_info` and `database_stats` tools so an agent
/// sees an identical view either way. Errors if the collection does not exist.
fn collection_summary(db: &Database, collection: &str) -> Result<Value, String> {
    let Some(descriptor) = db.descriptor(collection).cloned() else {
        return Err(format!("collection '{collection}' not found"));
    };
    let count = if descriptor.multivector {
        db.document_count(collection).map_err(|e| e.to_string())?
    } else {
        db.len(collection).map_err(|e| e.to_string())?
    };
    Ok(json!({
        "name": collection,
        "dim": descriptor.dim,
        "metric": serde_json::to_value(descriptor.metric).map_err(|e| e.to_string())?,
        "index": serde_json::to_value(descriptor.index).map_err(|e| e.to_string())?,
        "filterable": serde_json::to_value(&descriptor.filterable).map_err(|e| e.to_string())?,
        "multivector": descriptor.multivector,
        "vector_encryption": serde_json::to_value(descriptor.vector_encryption).map_err(|e| e.to_string())?,
        "count": count,
    }))
}

/// The advertised tool catalog, each with a JSON-Schema for its arguments.
#[must_use]
pub fn tool_definitions() -> Value {
    let collection_arg = json!({ "type": "string", "description": "Collection name" });
    let vector_arg = json!({ "type": "array", "items": { "type": "number" }, "description": "Dense f32 vector" });
    json!([
        {
            "name": "list_collections",
            "description": "List all collections in the database.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "create_collection",
            "description": "Create a collection with a vector dimensionality, distance metric, index, and optional filterable payload fields for hybrid search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": collection_arg,
                    "dim": { "type": "integer", "description": "Vector dimensionality" },
                    "metric": { "type": "string", "enum": ["l2", "cosine", "dot"], "default": "l2" },
                    "index": {
                        "type": "string",
                        "enum": ["hnsw", "vamana", "disk_vamana", "ivf", "colbert"],
                        "default": "hnsw",
                        "description": "Index structure; disk_vamana is the memory-frugal disk path (l2/cosine only); colbert is the ColBERTv2/PLAID token-pool index for multivector collections"
                    },
                    "pq_subspaces": {
                        "type": "integer",
                        "description": "Product-quantization subspaces for disk_vamana / ivf (must divide dim)"
                    },
                    "filterable": {
                        "type": "array",
                        "description": "Payload fields to index for pre-filtered (hybrid) search",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Dot-path into the payload (e.g. user.city)" },
                                "field_type": { "type": "string", "enum": ["keyword", "numeric"], "default": "keyword" }
                            },
                            "required": ["path"]
                        }
                    },
                    "multivector": {
                        "type": "boolean",
                        "default": false,
                        "description": "Create a multi-vector (late-interaction / ColBERT) collection; documents are token sets searched by MaxSim (cosine/dot only)"
                    },
                    "vector_encryption": {
                        "type": "string",
                        "enum": ["none", "dcpe", "client_side"],
                        "default": "none",
                        "description": "Client-side vector encryption (the server never holds the key). 'none' = plaintext, the server ranks. 'dcpe' = experimental property-preserving encryption (ADR-0031): the server ranks ciphertexts, L2 only, NOT semantically secure — leaks the approximate distance-comparison relation by design. 'client_side' = semantically secure opaque AEAD (ADR-0032): the server stores blobs it cannot read and does not rank, so the client fetches and ranks locally."
                    }
                },
                "required": ["name", "dim"]
            }
        },
        {
            "name": "upsert",
            "description": "Insert or replace a point (vector + optional JSON payload).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "collection": collection_arg,
                    "id": { "type": "string", "description": "External point id" },
                    "vector": vector_arg,
                    "payload": { "type": "object", "description": "Arbitrary JSON metadata" }
                },
                "required": ["collection", "id", "vector"]
            }
        },
        {
            "name": "search",
            "description": "Find the k nearest points to a query vector, with an optional payload filter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "collection": collection_arg,
                    "vector": vector_arg,
                    "k": { "type": "integer", "default": 10 },
                    "filter": { "type": "object", "description": "Quiver payload filter tree" }
                },
                "required": ["collection", "vector"]
            }
        },
        {
            "name": "hybrid_search",
            "description": "Hybrid search fused with Reciprocal Rank Fusion (ADR-0043/0045/0046). Provide a dense 'vector', a sparse query ('sparse_indices' + 'sparse_values', parallel arrays), and/or a full-text 'query_text' (tokenized and scored by BM25); at least one is required. Honours the same payload filter on every side.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "collection": collection_arg,
                    "vector": { "type": "array", "items": { "type": "number" }, "description": "Dense query vector (omit for pure-sparse/text search)" },
                    "sparse_indices": { "type": "array", "items": { "type": "integer" }, "description": "Sparse query dimension ids (parallel to sparse_values)" },
                    "sparse_values": { "type": "array", "items": { "type": "number" }, "description": "Sparse query weights (parallel to sparse_indices)" },
                    "query_text": { "type": "string", "description": "Full-text query, scored by BM25 over the inverted index (ADR-0046)" },
                    "k": { "type": "integer", "default": 10 },
                    "filter": { "type": "object", "description": "Quiver payload filter tree" },
                    "rrf_k0": { "type": "number", "description": "RRF rank-bias constant (default 60)" }
                },
                "required": ["collection"]
            }
        },
        {
            "name": "fetch",
            "description": "List points without ranking, with an optional payload filter and a limit. The retrieval path for client-side-encrypted collections (ADR-0032) that the server cannot rank — the key holder decrypts the returned payload blobs and ranks. Also a general list-points tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "collection": collection_arg,
                    "filter": { "type": "object", "description": "Quiver payload filter tree" },
                    "limit": { "type": "integer", "default": 100 }
                },
                "required": ["collection"]
            }
        },
        {
            "name": "get",
            "description": "Fetch a single point by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "collection": collection_arg, "id": { "type": "string" } },
                "required": ["collection", "id"]
            }
        },
        {
            "name": "delete",
            "description": "Delete a point by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "collection": collection_arg, "id": { "type": "string" } },
                "required": ["collection", "id"]
            }
        },
        {
            "name": "upsert_document",
            "description": "Insert or replace a multi-vector (late-interaction / ColBERT) document: its set of token vectors plus an optional JSON payload.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "collection": collection_arg,
                    "id": { "type": "string", "description": "External document id" },
                    "vectors": { "type": "array", "items": { "type": "array", "items": { "type": "number" } }, "description": "The document's token vectors" },
                    "payload": { "type": "object", "description": "Arbitrary JSON metadata" }
                },
                "required": ["collection", "id", "vectors"]
            }
        },
        {
            "name": "search_multi_vector",
            "description": "Rank documents in a multi-vector collection by MaxSim late interaction against a set of query token vectors, with an optional payload filter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "collection": collection_arg,
                    "query": { "type": "array", "items": { "type": "array", "items": { "type": "number" } }, "description": "The query's token vectors" },
                    "k": { "type": "integer", "default": 10 },
                    "filter": { "type": "object", "description": "Quiver payload filter tree" }
                },
                "required": ["collection", "query"]
            }
        },
        {
            "name": "delete_document",
            "description": "Delete a multi-vector document and all of its token rows.",
            "inputSchema": {
                "type": "object",
                "properties": { "collection": collection_arg, "id": { "type": "string" } },
                "required": ["collection", "id"]
            }
        },
        {
            "name": "collection_info",
            "description": "Inspect one collection: dimension, metric, index, declared filterable fields, multivector flag, vector-encryption mode, and live point count. Lets an agent reason about a collection's shape before upserting or searching.",
            "inputSchema": {
                "type": "object",
                "properties": { "collection": collection_arg },
                "required": ["collection"]
            }
        },
        {
            "name": "delete_collection",
            "description": "Delete an entire collection and all of its points/documents. Returns whether it existed. Irreversible — an agent managing collections uses this to clean up.",
            "inputSchema": {
                "type": "object",
                "properties": { "collection": collection_arg },
                "required": ["collection"]
            }
        },
        {
            "name": "database_stats",
            "description": "A whole-database overview for operating the instance: the number of collections, the total live point count, a per-collection summary (dimension, metric, index, multivector flag, encryption mode, count), and snapshot status (manifest_version, on-disk disk_bytes). One call to see everything, instead of collection_info per collection.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "snapshot",
            "description": "Take a consistent online snapshot (backup) of the whole database into a server-local directory, which must not already exist. Returns the manifest version captured and the file/byte counts. Restore by pointing a Quiver instance at the snapshot directory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "destination": { "type": "string", "description": "Server-local destination directory; must not already exist" }
                },
                "required": ["destination"]
            }
        },
        {
            "name": "upsert_text",
            "description": "Embed a text with the collection's configured embedding provider (ADR-0047/0058) and upsert it as a dense point, co-populating the BM25 full-text field so one call feeds both dense and keyword search. Requires an [embedding.<collection>] provider in the Quiver config passed to `quiver mcp --config`; lets an agent store documents without running an embedding model itself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "collection": collection_arg,
                    "id": { "type": "string", "description": "External point id" },
                    "text": { "type": "string", "description": "Text to embed and store" },
                    "payload": { "type": "object", "description": "Arbitrary JSON metadata (the text is also stored under the full-text key automatically)" }
                },
                "required": ["collection", "id", "text"]
            }
        },
        {
            "name": "search_text",
            "description": "Embed a query text with the collection's embedding provider and run a hybrid dense+BM25 search, optionally reranking the results with the collection's rerank provider. Requires an [embedding.<collection>] provider in the Quiver config passed to `quiver mcp --config`; lets an agent search by text without embedding the query itself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "collection": collection_arg,
                    "text": { "type": "string", "description": "Query text to embed and search with" },
                    "k": { "type": "integer", "default": 10 },
                    "filter": { "type": "object", "description": "Quiver payload filter tree" },
                    "rerank": { "type": "boolean", "default": false, "description": "Rerank results with the collection's [rerank.<collection>] provider, if configured" },
                    "rrf_k0": { "type": "number", "description": "RRF rank-bias constant (default 60)" }
                },
                "required": ["collection", "text"]
            }
        }
    ])
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "quiver-mcp", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn success(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn to_text(value: &Value) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| e.to_string())
}

fn want_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or non-string argument '{key}'"))
}

fn want_u64(args: &Value, key: &str) -> Result<u64, String> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing or non-integer argument '{key}'"))
}

fn want_vector(args: &Value, key: &str) -> Result<Vec<f32>, String> {
    let arr = args
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing array argument '{key}'"))?;
    arr.iter()
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| format!("'{key}' must be an array of numbers"))
        })
        .collect()
}

// Parse an array-of-arrays argument into the token set of a multi-vector document
// or query (ADR-0028).
fn want_vectors(args: &Value, key: &str) -> Result<Vec<Vec<f32>>, String> {
    let outer = args
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing array-of-arrays argument '{key}'"))?;
    outer
        .iter()
        .map(|row| {
            row.as_array()
                .ok_or_else(|| format!("'{key}' must be an array of vectors"))?
                .iter()
                .map(|v| {
                    v.as_f64()
                        .map(|f| f as f32)
                        .ok_or_else(|| format!("'{key}' vectors must contain numbers"))
                })
                .collect()
        })
        .collect()
}

fn want_metric(args: &Value) -> Result<DistanceMetric, String> {
    match args.get("metric").and_then(Value::as_str).unwrap_or("l2") {
        "l2" | "L2" => Ok(DistanceMetric::L2),
        "cosine" | "Cosine" => Ok(DistanceMetric::Cosine),
        "dot" | "Dot" => Ok(DistanceMetric::Dot),
        other => Err(format!("unknown metric '{other}' (use l2, cosine, or dot)")),
    }
}

fn want_index_spec(args: &Value) -> Result<IndexSpec, String> {
    let kind = match args.get("index").and_then(Value::as_str).unwrap_or("hnsw") {
        "hnsw" => IndexKind::Hnsw,
        "vamana" => IndexKind::Vamana,
        "disk_vamana" | "disk" => IndexKind::DiskVamana,
        "ivf" => IndexKind::Ivf,
        "colbert" => IndexKind::Colbert,
        other => {
            return Err(format!(
                "unknown index '{other}' (use hnsw, vamana, disk_vamana, ivf, or colbert)"
            ));
        }
    };
    let pq_subspaces = args
        .get("pq_subspaces")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    Ok(IndexSpec { kind, pq_subspaces })
}

// Parse the optional `filterable` argument: an array of {path, field_type}
// objects declaring which payload fields to index for hybrid search.
fn want_filterable(args: &Value) -> Result<Vec<FilterableField>, String> {
    let Some(value) = args.get("filterable") else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or("filterable must be an array of {path, field_type}")?;
    let mut fields = Vec::with_capacity(array.len());
    for field in array {
        let path = field
            .get("path")
            .and_then(Value::as_str)
            .ok_or("each filterable field needs a string 'path'")?;
        let field_type = match field
            .get("field_type")
            .and_then(Value::as_str)
            .unwrap_or("keyword")
        {
            "keyword" => FieldType::Keyword,
            "numeric" => FieldType::Numeric,
            other => {
                return Err(format!(
                    "unknown field_type '{other}' (use keyword or numeric)"
                ));
            }
        };
        fields.push(FilterableField {
            path: path.to_owned(),
            field_type,
        });
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn db() -> (tempfile::TempDir, Database) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(tmp.path()).unwrap();
        (tmp, db)
    }

    fn call(db: &mut Database, tool: &str, args: Value) -> Value {
        let msg = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":args}});
        handle_message(db, &msg).unwrap()
    }

    fn result_text(resp: &Value) -> String {
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let (_t, mut db) = db();
        let resp = handle_message(
            &mut db,
            &json!({"jsonrpc":"2.0","id":0,"method":"initialize"}),
        )
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "quiver-mcp");
    }

    #[test]
    fn tools_list_has_the_core_tools() {
        let (_t, mut db) = db();
        let resp = handle_message(
            &mut db,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "list_collections",
            "create_collection",
            "upsert",
            "search",
            "hybrid_search",
            "get",
            "delete",
            "collection_info",
            "delete_collection",
            "database_stats",
            "snapshot",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    #[test]
    fn collection_info_reports_shape_and_count() {
        let (_t, mut db) = db();
        call(
            &mut db,
            "create_collection",
            json!({
                "name": "kb", "dim": 3, "metric": "cosine", "index": "hnsw",
                "filterable": [{"path": "lang", "field_type": "keyword"}]
            }),
        );
        call(
            &mut db,
            "upsert",
            json!({"collection":"kb","id":"a","vector":[1.0,0.0,0.0]}),
        );
        let resp = call(&mut db, "collection_info", json!({"collection":"kb"}));
        assert_eq!(resp["result"]["isError"], false);
        let info: Value = serde_json::from_str(&result_text(&resp)).unwrap();
        assert_eq!(info["name"], "kb");
        assert_eq!(info["dim"], 3);
        assert_eq!(info["count"], 1);
        assert_eq!(info["multivector"], false);
        assert_eq!(info["filterable"][0]["path"], "lang");
        // index + vector_encryption serialise to their snake_case strings.
        assert_eq!(info["index"]["kind"], "hnsw");
        assert_eq!(info["vector_encryption"], "none");
        // An unknown collection is reported as an error result, not a panic.
        let missing = call(&mut db, "collection_info", json!({"collection":"nope"}));
        assert_eq!(missing["result"]["isError"], true);
    }

    #[test]
    fn delete_collection_drops_and_reports_existence() {
        let (_t, mut db) = db();
        call(&mut db, "create_collection", json!({"name":"tmp","dim":2}));
        let dropped = call(&mut db, "delete_collection", json!({"collection":"tmp"}));
        assert_eq!(dropped["result"]["isError"], false);
        let info: Value = serde_json::from_str(&result_text(&dropped)).unwrap();
        assert_eq!(info["existed"], true);
        // It is gone now, and a second delete reports it never existed.
        assert!(!db.collection_names().contains(&"tmp".to_owned()));
        let again = call(&mut db, "delete_collection", json!({"collection":"tmp"}));
        let info: Value = serde_json::from_str(&result_text(&again)).unwrap();
        assert_eq!(info["existed"], false);
    }

    #[test]
    fn database_stats_summarises_every_collection() {
        let (_t, mut db) = db();
        call(&mut db, "create_collection", json!({"name":"a","dim":2}));
        call(&mut db, "create_collection", json!({"name":"b","dim":3}));
        call(
            &mut db,
            "upsert",
            json!({"collection":"a","id":"1","vector":[1.0,0.0]}),
        );
        call(
            &mut db,
            "upsert",
            json!({"collection":"a","id":"2","vector":[0.0,1.0]}),
        );
        call(
            &mut db,
            "upsert",
            json!({"collection":"b","id":"1","vector":[1.0,0.0,0.0]}),
        );
        let resp = call(&mut db, "database_stats", json!({}));
        assert_eq!(resp["result"]["isError"], false);
        let stats: Value = serde_json::from_str(&result_text(&resp)).unwrap();
        assert_eq!(stats["collection_count"], 2);
        assert_eq!(stats["total_points"], 3);
        // Each collection's summary matches its collection_info shape.
        let by_name: std::collections::HashMap<String, &Value> = stats["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["name"].as_str().unwrap().to_owned(), c))
            .collect();
        assert_eq!(by_name["a"]["count"], 2);
        assert_eq!(by_name["a"]["dim"], 2);
        assert_eq!(by_name["b"]["count"], 1);
        // Snapshot status is present and sane.
        assert!(stats["manifest_version"].is_u64());
        assert!(stats["disk_bytes"].as_u64().unwrap() > 0);
    }

    #[test]
    fn snapshot_writes_a_restorable_copy() {
        let (_t, mut db) = db();
        call(&mut db, "create_collection", json!({"name":"kb","dim":2}));
        call(
            &mut db,
            "upsert",
            json!({"collection":"kb","id":"a","vector":[1.0,0.0]}),
        );
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("snap");
        let resp = call(
            &mut db,
            "snapshot",
            json!({ "destination": dest.to_str().unwrap() }),
        );
        assert_eq!(resp["result"]["isError"], false);
        let info: Value = serde_json::from_str(&result_text(&resp)).unwrap();
        assert!(info["files"].as_u64().unwrap() > 0);

        // The snapshot opens as an identical database.
        let restored = Database::open(&dest).unwrap();
        assert_eq!(restored.len("kb").unwrap(), 1);

        // Snapshotting onto the existing directory is reported as an error.
        let again = call(
            &mut db,
            "snapshot",
            json!({ "destination": dest.to_str().unwrap() }),
        );
        assert_eq!(again["result"]["isError"], true);
    }

    #[test]
    fn create_upsert_search_round_trip() {
        let (_t, mut db) = db();
        let r = call(
            &mut db,
            "create_collection",
            json!({"name":"items","dim":4,"metric":"l2"}),
        );
        assert_eq!(r["result"]["isError"], false);

        for (id, v, color) in [
            ("a", [0.0, 0.0, 0.0, 0.0], "red"),
            ("b", [1.0, 0.0, 0.0, 0.0], "blue"),
        ] {
            let r = call(
                &mut db,
                "upsert",
                json!({"collection":"items","id":id,"vector":v,"payload":{"color":color}}),
            );
            assert_eq!(r["result"]["isError"], false, "upsert {id}");
        }

        let r = call(
            &mut db,
            "search",
            json!({"collection":"items","vector":[0.1,0.0,0.0,0.0],"k":2}),
        );
        let text = result_text(&r);
        let parsed: Value = serde_json::from_str(&text).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        assert_eq!(matches[0]["id"], "a");
        assert_eq!(matches.len(), 2);

        // Filtered search returns only matching payloads.
        let r = call(
            &mut db,
            "search",
            json!({"collection":"items","vector":[0.0,0.0,0.0,0.0],"k":5,"filter":{"eq":{"field":"color","value":"blue"}}}),
        );
        let parsed: Value = serde_json::from_str(&result_text(&r)).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        assert!(matches.iter().all(|m| m["payload"]["color"] == "blue"));
    }

    #[test]
    fn hybrid_search_tool_fuses_dense_and_sparse() {
        let (_t, mut db) = db();
        call(
            &mut db,
            "create_collection",
            json!({"name":"kb","dim":4,"metric":"l2"}),
        );
        // "a" is the dense nearest neighbour; "b" matches the sparse query.
        call(
            &mut db,
            "upsert",
            json!({"collection":"kb","id":"a","vector":[1.0,0.0,0.0,0.0],"payload":{"__quiver_sparse__":{"indices":[100],"values":[0.1]}}}),
        );
        call(
            &mut db,
            "upsert",
            json!({"collection":"kb","id":"b","vector":[0.0,1.0,0.0,0.0],"payload":{"__quiver_sparse__":{"indices":[1,2],"values":[5.0,5.0]}}}),
        );

        // Dense + sparse: both "a" and "b" come back.
        let r = call(
            &mut db,
            "hybrid_search",
            json!({"collection":"kb","vector":[1.0,0.0,0.0,0.0],"sparse_indices":[1,2],"sparse_values":[1.0,1.0],"k":2}),
        );
        assert_eq!(r["result"]["isError"], false);
        let parsed: Value = serde_json::from_str(&result_text(&r)).unwrap();
        let ids: Vec<&str> = parsed["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"a") && ids.contains(&"b"), "got {ids:?}");

        // Pure sparse: only "b" shares the query's terms.
        let r = call(
            &mut db,
            "hybrid_search",
            json!({"collection":"kb","sparse_indices":[1,2],"sparse_values":[1.0,1.0],"k":2}),
        );
        let parsed: Value = serde_json::from_str(&result_text(&r)).unwrap();
        assert_eq!(parsed["matches"][0]["id"], "b");

        // Neither query is a tool error.
        let r = call(&mut db, "hybrid_search", json!({"collection":"kb","k":2}));
        assert_eq!(r["result"]["isError"], true);

        // Mismatched sparse arrays (only one provided) is a tool error.
        let r = call(
            &mut db,
            "hybrid_search",
            json!({"collection":"kb","sparse_indices":[1],"k":2}),
        );
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn hybrid_search_tool_supports_full_text_query() {
        let (_t, mut db) = db();
        call(
            &mut db,
            "create_collection",
            json!({"name":"docs","dim":4,"metric":"l2"}),
        );
        call(
            &mut db,
            "upsert",
            json!({"collection":"docs","id":"cat","vector":[0.0,0.0,0.0,0.0],"payload":{"__quiver_text__":"the quick brown cat"}}),
        );
        call(
            &mut db,
            "upsert",
            json!({"collection":"docs","id":"dog","vector":[0.0,0.0,0.0,0.0],"payload":{"__quiver_text__":"a lazy dog sleeps"}}),
        );
        let r = call(
            &mut db,
            "hybrid_search",
            json!({"collection":"docs","query_text":"cats","k":5}),
        );
        assert_eq!(r["result"]["isError"], false);
        let parsed: Value = serde_json::from_str(&result_text(&r)).unwrap();
        let ids: Vec<&str> = parsed["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["cat"], "BM25 over MCP ranks the cat doc");
    }

    const ENC_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    // Open the same directory through the `serve`/admin path (an envelope
    // key-ring from the master key), independent of MCP's `open` helper.
    fn open_via_serve(dir: &std::path::Path) -> Database {
        let keyring = quiver_crypto::open_keyring(dir, Some(ENC_KEY), false)
            .unwrap()
            .unwrap();
        Database::open_with_keyring(dir, keyring).unwrap()
    }

    // A directory written through MCP's encrypted `open` must be readable through
    // the `serve`/admin opener, and vice versa — they share one on-disk crypto
    // format. Before unifying on `quiver_crypto::open_keyring`, MCP wrote
    // single-key `AeadCodec` pages the envelope key-ring could not open.
    #[test]
    fn a_directory_written_by_mcp_opens_under_serve() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open(tmp.path(), Some(ENC_KEY), false).unwrap();
            call_tool(&mut db, "create_collection", &json!({"name":"c","dim":2})).unwrap();
            call_tool(
                &mut db,
                "upsert",
                &json!({"collection":"c","id":"1","vector":[1.0,2.0]}),
            )
            .unwrap();
        }
        let db = open_via_serve(tmp.path());
        assert_eq!(db.len("c").unwrap(), 1);
        assert!(db.get("c", "1").unwrap().is_some());
    }

    #[test]
    fn a_directory_written_by_serve_opens_under_mcp() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut db = open_via_serve(tmp.path());
            call_tool(&mut db, "create_collection", &json!({"name":"c","dim":2})).unwrap();
            call_tool(
                &mut db,
                "upsert",
                &json!({"collection":"c","id":"9","vector":[3.0,4.0]}),
            )
            .unwrap();
        }
        let db = open(tmp.path(), Some(ENC_KEY), false).unwrap();
        assert_eq!(db.len("c").unwrap(), 1);
        assert!(db.get("c", "9").unwrap().is_some());
    }

    #[test]
    fn tool_failure_is_reported_as_iserror_result() {
        let (_t, mut db) = db();
        let r = call(
            &mut db,
            "search",
            json!({"collection":"missing","vector":[0.0,0.0]}),
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(result_text(&r).contains("missing"));
    }

    #[test]
    fn invalid_arguments_are_iserror() {
        let (_t, mut db) = db();
        // Missing required `dim`.
        let r = call(&mut db, "create_collection", json!({"name":"x"}));
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn unknown_method_is_a_protocol_error() {
        let (_t, mut db) = db();
        let resp = handle_message(
            &mut db,
            &json!({"jsonrpc":"2.0","id":7,"method":"frobnicate"}),
        )
        .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn notifications_get_no_response() {
        let (_t, mut db) = db();
        assert!(
            handle_message(
                &mut db,
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .is_none()
        );
    }

    #[test]
    fn serve_processes_newline_delimited_messages() {
        let (_t, mut db) = db();
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let mut out = Vec::new();
        serve(&mut db, Cursor::new(input), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["id"], 1);
        assert_eq!(first["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn agent_can_create_a_disk_index_collection_and_query_it() {
        let (_t, mut db) = db();
        let r = call(
            &mut db,
            "create_collection",
            json!({"name":"d","dim":4,"metric":"l2","index":"disk_vamana"}),
        );
        assert_eq!(r["result"]["isError"], false, "{}", result_text(&r));
        for i in 0..30u32 {
            let r = call(
                &mut db,
                "upsert",
                json!({"collection":"d","id":format!("p{i}"),"vector":[i as f32,0.0,0.0,0.0]}),
            );
            assert_eq!(r["result"]["isError"], false);
        }
        let r = call(
            &mut db,
            "search",
            json!({"collection":"d","vector":[7.0,0.0,0.0,0.0],"k":1}),
        );
        let parsed: Value = serde_json::from_str(&result_text(&r)).unwrap();
        assert_eq!(parsed["matches"][0]["id"], "p7");
    }

    #[test]
    fn unknown_index_kind_is_an_iserror_result() {
        let (_t, mut db) = db();
        let r = call(
            &mut db,
            "create_collection",
            json!({"name":"x","dim":4,"index":"bogus"}),
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(result_text(&r).contains("unknown index"));
    }

    #[test]
    fn agent_can_create_a_colbert_multivector_collection() {
        let (_t, mut db) = db();
        // colbert is selectable for a multi-vector collection (ADR-0034).
        let r = call(
            &mut db,
            "create_collection",
            json!({"name":"c","dim":4,"metric":"cosine","multivector":true,"index":"colbert"}),
        );
        assert_eq!(r["result"]["isError"], false, "{}", result_text(&r));
        // colbert on a single-vector collection is rejected by the engine.
        let r = call(
            &mut db,
            "create_collection",
            json!({"name":"bad","dim":4,"metric":"cosine","index":"colbert"}),
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(result_text(&r).contains("multi-vector"));
    }

    #[test]
    fn agent_can_declare_filterable_fields_and_run_a_hybrid_search() {
        let (_t, mut db) = db();
        let r = call(
            &mut db,
            "create_collection",
            json!({
                "name": "people", "dim": 4, "metric": "l2",
                "filterable": [{"path": "city", "field_type": "keyword"}]
            }),
        );
        assert_eq!(r["result"]["isError"], false, "{}", result_text(&r));
        for (id, x, city) in [
            ("p", 0.0, "paris"),
            ("l", 1.0, "lyon"),
            ("p2", 2.0, "paris"),
        ] {
            let r = call(
                &mut db,
                "upsert",
                json!({"collection":"people","id":id,"vector":[x,0.0,0.0,0.0],"payload":{"city":city}}),
            );
            assert_eq!(r["result"]["isError"], false, "upsert {id}");
        }
        let r = call(
            &mut db,
            "search",
            json!({
                "collection":"people","vector":[0.0,0.0,0.0,0.0],"k":5,
                "filter":{"eq":{"field":"city","value":"paris"}}
            }),
        );
        let parsed: Value = serde_json::from_str(&result_text(&r)).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        assert_eq!(matches[0]["id"], "p"); // nearest paris, via the pre-filter
        for m in matches {
            assert_eq!(m["payload"]["city"], "paris"); // lyon excluded
        }
    }

    #[test]
    fn unknown_field_type_is_an_iserror_result() {
        let (_t, mut db) = db();
        let r = call(
            &mut db,
            "create_collection",
            json!({
                "name": "x", "dim": 4,
                "filterable": [{"path": "city", "field_type": "bogus"}]
            }),
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(result_text(&r).contains("unknown field_type"));
    }

    #[test]
    fn create_collection_supports_dcpe_and_enforces_l2() {
        let (_t, mut db) = db();
        // A DCPE + L2 collection is created (encrypted vectors are ordinary L2
        // vectors to the engine).
        call_tool(
            &mut db,
            "create_collection",
            &json!({"name": "enc", "dim": 3, "metric": "l2", "vector_encryption": "dcpe"}),
        )
        .unwrap();
        // A DCPE collection with a non-L2 metric is rejected (ADR-0031).
        let r = call(
            &mut db,
            "create_collection",
            json!({"name": "bad", "dim": 3, "metric": "cosine", "vector_encryption": "dcpe"}),
        );
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn fetch_tool_lists_points_and_client_side_rejects_search() {
        let (_t, mut db) = db();
        call_tool(
            &mut db,
            "create_collection",
            &json!({"name": "vault", "dim": 2, "metric": "l2", "vector_encryption": "client_side"}),
        )
        .unwrap();
        for i in 0..3 {
            call_tool(
                &mut db,
                "upsert",
                &json!({
                    "collection": "vault",
                    "id": format!("p{i}"),
                    "vector": [0.0, 0.0],
                    "payload": {"__quiver_vec__": "ciphertext", "n": i}
                }),
            )
            .unwrap();
        }
        // fetch lists the points without ranking.
        let out = call_tool(&mut db, "fetch", &json!({"collection": "vault"})).unwrap();
        assert!(out.contains("\"p0\"") && out.contains("\"p2\""));
        // A ranked search is rejected — the server cannot rank opaque vectors.
        let r = call(
            &mut db,
            "search",
            json!({"collection": "vault", "vector": [0.0, 0.0]}),
        );
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn multivector_tools_create_upsert_search_and_delete() {
        let (_t, mut db) = db();
        call_tool(
            &mut db,
            "create_collection",
            &json!({"name": "docs", "dim": 3, "metric": "cosine", "multivector": true}),
        )
        .unwrap();
        call_tool(
            &mut db,
            "upsert_document",
            &json!({"collection": "docs", "id": "a",
                    "vectors": [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], "payload": {"lang": "en"}}),
        )
        .unwrap();
        call_tool(
            &mut db,
            "upsert_document",
            &json!({"collection": "docs", "id": "b",
                    "vectors": [[0.0, 0.0, 1.0]], "payload": {"lang": "fr"}}),
        )
        .unwrap();

        let out = call_tool(
            &mut db,
            "search_multi_vector",
            &json!({"collection": "docs", "query": [[0.0, 0.0, 1.0]], "k": 2}),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["matches"][0]["id"], "b");

        let msg = call_tool(
            &mut db,
            "delete_document",
            &json!({"collection": "docs", "id": "b"}),
        )
        .unwrap();
        assert!(msg.contains("deleted document 'b'"));
    }

    use quiver_providers::{EmbeddingConfig, ProviderKind, RerankConfig};

    /// A registry with a deterministic, network-free `fake` embedder (and
    /// optionally a `fake` reranker) for `collection`, so the text tools are
    /// exercised end-to-end without a real model.
    fn fake_registry(collection: &str, dim: u32, with_reranker: bool) -> EmbedRegistry {
        let mut embedding = std::collections::HashMap::new();
        embedding.insert(
            collection.to_owned(),
            EmbeddingConfig {
                provider: ProviderKind::Fake,
                model: String::new(),
                endpoint: String::new(),
                dim,
                api_key_env: String::new(),
            },
        );
        let mut rerank = std::collections::HashMap::new();
        if with_reranker {
            rerank.insert(
                collection.to_owned(),
                RerankConfig {
                    provider: ProviderKind::Fake,
                    model: String::new(),
                    endpoint: String::new(),
                    api_key_env: String::new(),
                },
            );
        }
        EmbedRegistry::from_config(&embedding, &rerank).unwrap()
    }

    #[test]
    fn upsert_text_and_search_text_round_trip() {
        let (_t, mut db) = db();
        let reg = fake_registry("docs", 16, false);
        call_tool(
            &mut db,
            "create_collection",
            &json!({"name":"docs","dim":16,"metric":"cosine"}),
        )
        .unwrap();
        for (id, text) in [("cat", "the quick brown cat"), ("dog", "a lazy dog sleeps")] {
            let msg = call_tool_embed(
                &mut db,
                &reg,
                "upsert_text",
                &json!({"collection":"docs","id":id,"text":text}),
            )
            .unwrap();
            assert!(msg.contains(id));
        }
        // The text was co-stored under the full-text key, so BM25 ranks the cat
        // document for the stemmed query "cats".
        let out = call_tool_embed(
            &mut db,
            &reg,
            "search_text",
            &json!({"collection":"docs","text":"cats","k":5}),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["matches"][0]["id"], "cat");
        // The stored payload carries the text key, so a plain `get` sees it too.
        let got = call_tool(&mut db, "get", &json!({"collection":"docs","id":"cat"})).unwrap();
        assert!(got.contains("quick brown cat"));
    }

    #[test]
    fn search_text_reranks_when_requested() {
        let (_t, mut db) = db();
        let reg = fake_registry("docs", 16, true);
        call_tool(
            &mut db,
            "create_collection",
            &json!({"name":"docs","dim":16,"metric":"cosine"}),
        )
        .unwrap();
        for (id, text) in [("cat", "the quick brown cat"), ("dog", "a lazy dog sleeps")] {
            call_tool_embed(
                &mut db,
                &reg,
                "upsert_text",
                &json!({"collection":"docs","id":id,"text":text}),
            )
            .unwrap();
        }
        // The fake reranker scores by lexical overlap, so the query "lazy dog"
        // reorders the dog document to the top.
        let out = call_tool_embed(
            &mut db,
            &reg,
            "search_text",
            &json!({"collection":"docs","text":"lazy dog","k":2,"rerank":true}),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["matches"][0]["id"], "dog");
    }

    #[test]
    fn text_tools_are_advertised_and_error_without_a_provider() {
        let (_t, mut db) = db();
        // Both tools are in the advertised catalog regardless of configuration.
        let resp = handle_message(
            &mut db,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"upsert_text"), "upsert_text advertised");
        assert!(names.contains(&"search_text"), "search_text advertised");
        // With no provider configured (the default no-embed path), using one is a
        // clear tool error, not a panic.
        call_tool(
            &mut db,
            "create_collection",
            &json!({"name":"docs","dim":4}),
        )
        .unwrap();
        let r = call(
            &mut db,
            "upsert_text",
            json!({"collection":"docs","id":"a","text":"hi"}),
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(result_text(&r).contains("no embedding provider"));
    }

    #[test]
    fn handle_message_with_embed_routes_text_tools() {
        let (_t, mut db) = db();
        let reg = fake_registry("docs", 8, false);
        call_tool(
            &mut db,
            "create_collection",
            &json!({"name":"docs","dim":8}),
        )
        .unwrap();
        let msg = json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"upsert_text","arguments":{"collection":"docs","id":"x","text":"hello world"}}});
        let resp = handle_message_with_embed(&mut db, &reg, &msg).unwrap();
        assert_eq!(resp["result"]["isError"], false, "{}", result_text(&resp));
    }
}
