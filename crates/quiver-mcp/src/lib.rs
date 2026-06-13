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
//! unless explicitly insecure), the same posture as the network server.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{Value, json};

use quiver_core::page::{PageCodec, PlainCodec};
use quiver_crypto::AeadCodec;
use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, Filter, SearchParams};

/// The MCP protocol revision this server implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Open the database at `data_dir` (encrypted at rest unless `insecure`) and
/// serve MCP over stdin/stdout until the input stream closes.
///
/// # Errors
/// Returns an error if no encryption key is provided and `insecure` is false, if
/// the key is invalid, if the database cannot be opened, or on an I/O failure.
pub fn run(data_dir: &Path, encryption_key: Option<&str>, insecure: bool) -> anyhow::Result<()> {
    let codec: Box<dyn PageCodec> = match encryption_key {
        Some(key) => Box::new(
            AeadCodec::from_hex(key).map_err(|e| anyhow::anyhow!("invalid encryption key: {e}"))?,
        ),
        None => {
            anyhow::ensure!(
                insecure,
                "no encryption key set: encryption-at-rest is on by default — set QUIVER_ENCRYPTION_KEY or pass --insecure"
            );
            Box::new(PlainCodec)
        }
    };
    let mut db = Database::open_with_codec(data_dir, codec)?;
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
            db.create_collection(collection, Descriptor::new(dim, Dtype::F32, metric))
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
            "description": "Create a collection with a vector dimensionality and distance metric.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": collection_arg,
                    "dim": { "type": "integer", "description": "Vector dimensionality" },
                    "metric": { "type": "string", "enum": ["l2", "cosine", "dot"], "default": "l2" }
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

fn want_metric(args: &Value) -> Result<DistanceMetric, String> {
    match args.get("metric").and_then(Value::as_str).unwrap_or("l2") {
        "l2" | "L2" => Ok(DistanceMetric::L2),
        "cosine" | "Cosine" => Ok(DistanceMetric::Cosine),
        "dot" | "Dot" => Ok(DistanceMetric::Dot),
        other => Err(format!("unknown metric '{other}' (use l2, cosine, or dot)")),
    }
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
}
