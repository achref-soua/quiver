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
//! Tools: `list_collections`, `create_collection`, `upsert`, `search`, `get`,
//! `delete`. The database is opened secure-by-default (encryption-at-rest on
//! unless explicitly insecure) through the same envelope key-ring as the network
//! server and `quiver admin`, so a data directory is interchangeable between them.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{Value, json};

use quiver_embed::{
    Database, Descriptor, DistanceMetric, Dtype, FieldType, Filter, FilterableField, IndexKind,
    IndexSpec, SearchParams, VectorEncryption,
};

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
    let mut db = open(data_dir, encryption_key, insecure)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(&mut db, stdin.lock(), stdout.lock())?;
    Ok(())
}

/// Read newline-delimited JSON-RPC messages from `reader`, dispatch each against
/// `db`, and write any responses to `writer`. Notifications (no `id`) yield no
/// response. Returns only on input EOF or an I/O error.
///
/// # Errors
/// Propagates I/O errors from reading or writing the streams.
pub fn serve(
    db: &mut Database,
    reader: impl BufRead,
    mut writer: impl Write,
) -> std::io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle_message(db, &msg),
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
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let is_notification = msg.get("id").is_none();
    let method = msg.get("method").and_then(Value::as_str);

    match method {
        Some("initialize") => Some(success(&id, initialize_result())),
        Some("ping") => Some(success(&id, json!({}))),
        Some("tools/list") => Some(success(&id, json!({ "tools": tool_definitions() }))),
        Some("tools/call") => Some(handle_tool_call(db, &id, msg.get("params"))),
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

fn handle_tool_call(db: &mut Database, id: &Value, params: Option<&Value>) -> Value {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        return error_response(id, -32602, "tools/call requires a tool name");
    };
    let empty = json!({});
    let args = params.and_then(|p| p.get("arguments")).unwrap_or(&empty);
    match call_tool(db, name, args) {
        Ok(text) => success(id, tool_result(&text, false)),
        Err(message) => success(id, tool_result(&message, true)),
    }
}

/// Execute a tool, returning its text content or an error message.
fn call_tool(db: &mut Database, name: &str, args: &Value) -> Result<String, String> {
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
        other => Err(format!("unknown tool: {other}")),
    }
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
                        "enum": ["hnsw", "vamana", "disk_vamana", "ivf"],
                        "default": "hnsw",
                        "description": "Index structure; disk_vamana is the memory-frugal disk path (l2/cosine only)"
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
        other => {
            return Err(format!(
                "unknown index '{other}' (use hnsw, vamana, disk_vamana, or ivf)"
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
            "get",
            "delete",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
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
}
