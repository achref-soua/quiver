// SPDX-License-Identifier: AGPL-3.0-only
//! Migration importers — load exports from other vector databases into Quiver.
//!
//! Each supported source (Qdrant, Chroma, pgvector) has a small adapter that
//! parses that tool's portable export into normalized [`ImportPoint`]s, which
//! [`import_into`] bulk-loads into an embeddable [`Database`]. The import target
//! reuses the engine directly (create collection + upsert + checkpoint), so the
//! same crash-safety, encryption, indexing, and filterable fields apply as to
//! any other write (ADR-0024).
//!
//! Inputs are *files* the user exports from the source tool — no live network
//! connection is opened — which keeps the importer dependency-light and its
//! adapters fully testable. Live-connector variants are a future enhancement.
//!
//! Formats (see `docs/migration.md`):
//!
//! - **Qdrant** — JSON Lines, one scrolled point per line:
//!   `{"id": <int|string>, "vector": [..] | {"name": [..]}, "payload": {..}}`.
//! - **pgvector** — JSON Lines, one row per line (e.g. from `row_to_json`): the
//!   id and vector columns are named (defaults `id` / `embedding`; the vector may
//!   be a JSON array or a pgvector text literal `"[1,2,3]"`), and every other
//!   column becomes a payload field.
//! - **Chroma** — a single JSON object from `collection.get(include=[...])`:
//!   `{"ids": [..], "embeddings": [[..]], "metadatas": [{..}], "documents": [..]}`.

use std::fmt;
use std::str::FromStr;

use quiver_embed::{Database, Descriptor};
use serde_json::{Map, Value};
use thiserror::Error;

/// A vector database whose export Quiver can import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Source {
    /// Qdrant — JSON Lines of scrolled points.
    Qdrant,
    /// Chroma — a single `collection.get(...)` JSON object.
    Chroma,
    /// pgvector (Postgres) — JSON Lines of rows.
    Pgvector,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Qdrant => "qdrant",
            Source::Chroma => "chroma",
            Source::Pgvector => "pgvector",
        })
    }
}

impl FromStr for Source {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "qdrant" => Ok(Source::Qdrant),
            "chroma" => Ok(Source::Chroma),
            "pgvector" | "postgres" | "pg" => Ok(Source::Pgvector),
            other => Err(format!(
                "unknown source '{other}' (expected qdrant, chroma, or pgvector)"
            )),
        }
    }
}

/// Errors raised while parsing an export or loading it into the database.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ImportError {
    /// The export text was not valid JSON / JSON Lines.
    #[error("malformed export json: {0}")]
    Json(#[from] serde_json::Error),
    /// The JSON was valid but did not match the source's expected shape.
    #[error("unexpected {0} export shape: {1}")]
    Shape(Source, String),
    /// A vector value could not be parsed into `f32` components.
    #[error("invalid vector: {0}")]
    Vector(String),
    /// The points had zero or inconsistent dimensionality.
    #[error("dimensionality error: {0}")]
    Dim(String),
    /// The database rejected the create or upsert.
    #[error(transparent)]
    Db(#[from] quiver_embed::Error),
}

/// One normalized point ready to upsert into a Quiver collection.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportPoint {
    /// External id (Qdrant/Chroma/pgvector ids are stringified).
    pub id: String,
    /// The dense vector.
    pub vector: Vec<f32>,
    /// The JSON payload (an object, or `Null` when the source carried none).
    pub payload: Value,
}

/// How to read a source export: which fields carry the id and vector.
#[derive(Debug, Clone)]
pub struct ParseOptions {
    /// The source tool.
    pub source: Source,
    /// Field/column holding the id (used by pgvector; Qdrant uses `id`).
    pub id_field: String,
    /// Field/column holding the vector (Qdrant `vector`, pgvector `embedding`).
    pub vector_field: String,
    /// For Qdrant *named* vectors, which one to import (defaults to the sole one).
    pub vector_name: Option<String>,
}

impl ParseOptions {
    /// Defaults for `source`: `id` / `vector` for Qdrant, `id` / `embedding` for
    /// pgvector. Chroma uses fixed keys and ignores these.
    #[must_use]
    pub fn new(source: Source) -> Self {
        let vector_field = match source {
            Source::Pgvector => "embedding",
            _ => "vector",
        };
        Self {
            source,
            id_field: "id".to_string(),
            vector_field: vector_field.to_string(),
            vector_name: None,
        }
    }
}

/// Parse an export into normalized points.
///
/// # Errors
/// [`ImportError::Json`] for malformed JSON, [`ImportError::Shape`] when the JSON
/// does not match the source's layout, and [`ImportError::Vector`] for a vector
/// component that is not numeric.
pub fn parse(opts: &ParseOptions, input: &str) -> Result<Vec<ImportPoint>, ImportError> {
    match opts.source {
        Source::Qdrant => parse_jsonl(input, |v| qdrant_point(opts, v)),
        Source::Pgvector => parse_jsonl(input, |v| pgvector_point(opts, v)),
        Source::Chroma => parse_chroma(input),
    }
}

/// The single dimensionality shared by all `points`.
///
/// # Errors
/// [`ImportError::Dim`] if `points` is empty, holds zero-length vectors, or mixes
/// dimensionalities.
pub fn infer_dim(points: &[ImportPoint]) -> Result<usize, ImportError> {
    let mut dim: Option<usize> = None;
    for p in points {
        match dim {
            None => dim = Some(p.vector.len()),
            Some(d) if d != p.vector.len() => {
                return Err(ImportError::Dim(format!(
                    "point '{}' has dim {}, expected {d}",
                    p.id,
                    p.vector.len()
                )));
            }
            _ => {}
        }
    }
    match dim {
        Some(d) if d > 0 => Ok(d),
        _ => Err(ImportError::Dim("no vectors to import".to_string())),
    }
}

/// Create `collection` with `descriptor` (if it does not already exist), upsert
/// every point, and checkpoint. Returns the number of points loaded.
///
/// An existing collection is appended to rather than recreated, so an import can
/// resume or top up.
///
/// # Errors
/// [`ImportError::Db`] if the database rejects the create, an upsert, or the
/// checkpoint (e.g. a dimension mismatch against an existing collection).
pub fn import_into(
    db: &mut Database,
    collection: &str,
    descriptor: Descriptor,
    points: &[ImportPoint],
) -> Result<usize, ImportError> {
    if !db.collection_names().iter().any(|n| n == collection) {
        db.create_collection(collection, descriptor)?;
    }
    for p in points {
        db.upsert(collection, &p.id, &p.vector, &p.payload)?;
    }
    db.checkpoint()?;
    Ok(points.len())
}

// ----- per-source adapters -----

fn parse_jsonl(
    input: &str,
    mut to_point: impl FnMut(Value) -> Result<ImportPoint, ImportError>,
) -> Result<Vec<ImportPoint>, ImportError> {
    let mut out = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push(to_point(serde_json::from_str(line)?)?);
    }
    Ok(out)
}

fn qdrant_point(opts: &ParseOptions, value: Value) -> Result<ImportPoint, ImportError> {
    let Value::Object(mut obj) = value else {
        return Err(ImportError::Shape(
            Source::Qdrant,
            "each line must be a JSON object".to_string(),
        ));
    };
    let id = take_id(&mut obj, "id", Source::Qdrant)?;
    let vector_value = obj.remove(&opts.vector_field).ok_or_else(|| {
        ImportError::Shape(
            Source::Qdrant,
            format!("missing '{}' field", opts.vector_field),
        )
    })?;
    let vector = parse_vector(vector_value, opts.vector_name.as_deref(), Source::Qdrant)?;
    let payload = obj.remove("payload").unwrap_or(Value::Null);
    Ok(ImportPoint {
        id,
        vector,
        payload,
    })
}

fn pgvector_point(opts: &ParseOptions, value: Value) -> Result<ImportPoint, ImportError> {
    let Value::Object(mut obj) = value else {
        return Err(ImportError::Shape(
            Source::Pgvector,
            "each line must be a JSON object".to_string(),
        ));
    };
    let id = take_id(&mut obj, &opts.id_field, Source::Pgvector)?;
    let vector_value = obj.remove(&opts.vector_field).ok_or_else(|| {
        ImportError::Shape(
            Source::Pgvector,
            format!("missing '{}' column", opts.vector_field),
        )
    })?;
    let vector = parse_vector(vector_value, None, Source::Pgvector)?;
    // Every remaining column becomes a payload field.
    Ok(ImportPoint {
        id,
        vector,
        payload: Value::Object(obj),
    })
}

fn parse_chroma(input: &str) -> Result<Vec<ImportPoint>, ImportError> {
    let Value::Object(obj) = serde_json::from_str::<Value>(input.trim())? else {
        return Err(ImportError::Shape(
            Source::Chroma,
            "expected a single JSON object from collection.get(...)".to_string(),
        ));
    };
    let ids = chroma_array(&obj, "ids")?;
    let embeddings = chroma_array(&obj, "embeddings")?;
    if ids.len() != embeddings.len() {
        return Err(ImportError::Shape(
            Source::Chroma,
            format!(
                "ids ({}) and embeddings ({}) have different lengths",
                ids.len(),
                embeddings.len()
            ),
        ));
    }
    let metadatas = obj.get("metadatas").and_then(Value::as_array);
    let documents = obj.get("documents").and_then(Value::as_array);

    let mut out = Vec::with_capacity(ids.len());
    for (i, (id_value, embedding)) in ids.iter().zip(embeddings).enumerate() {
        let id = stringify_id(id_value, Source::Chroma)?;
        let vector = parse_vector(embedding.clone(), None, Source::Chroma)?;
        let mut payload = match metadatas.and_then(|m| m.get(i)) {
            Some(Value::Object(m)) => m.clone(),
            None | Some(Value::Null) => Map::new(),
            Some(other) => {
                return Err(ImportError::Shape(
                    Source::Chroma,
                    format!("metadata {i} must be an object, got {other}"),
                ));
            }
        };
        if let Some(doc) = documents.and_then(|d| d.get(i))
            && !doc.is_null()
        {
            payload.insert("document".to_string(), doc.clone());
        }
        out.push(ImportPoint {
            id,
            vector,
            payload: Value::Object(payload),
        });
    }
    Ok(out)
}

fn chroma_array<'a>(obj: &'a Map<String, Value>, key: &str) -> Result<&'a Vec<Value>, ImportError> {
    obj.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| ImportError::Shape(Source::Chroma, format!("missing '{key}' array")))
}

// Remove and stringify the id field; ids may be JSON strings or numbers.
fn take_id(obj: &mut Map<String, Value>, key: &str, source: Source) -> Result<String, ImportError> {
    match obj.remove(key) {
        Some(value) => stringify_id(&value, source),
        None => Err(ImportError::Shape(source, format!("missing '{key}' field"))),
    }
}

fn stringify_id(value: &Value, source: Source) -> Result<String, ImportError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(ImportError::Shape(
            source,
            format!("id must be a string or number, got {other}"),
        )),
    }
}

// Parse a vector from a JSON array, a Qdrant named-vector object, or a pgvector
// text literal like "[1,2,3]".
fn parse_vector(value: Value, name: Option<&str>, source: Source) -> Result<Vec<f32>, ImportError> {
    match value {
        Value::Array(items) => array_to_f32(items, source),
        Value::Object(map) => {
            let chosen = match name {
                Some(n) => map.get(n).cloned().ok_or_else(|| {
                    ImportError::Shape(source, format!("named vector '{n}' not found"))
                })?,
                None if map.len() == 1 => map
                    .into_values()
                    .next()
                    .ok_or_else(|| ImportError::Shape(source, "empty vector object".to_string()))?,
                None => {
                    return Err(ImportError::Shape(
                        source,
                        "multiple named vectors; specify which to import".to_string(),
                    ));
                }
            };
            parse_vector(chosen, None, source)
        }
        Value::String(s) => parse_pgvector_literal(&s),
        other => Err(ImportError::Vector(format!(
            "expected an array, named-vector object, or text literal, got {other}"
        ))),
    }
}

fn array_to_f32(items: Vec<Value>, source: Source) -> Result<Vec<f32>, ImportError> {
    items
        .into_iter()
        .map(|x| {
            x.as_f64().map(|f| f as f32).ok_or_else(|| {
                ImportError::Vector(format!("non-numeric component in {source} vector"))
            })
        })
        .collect()
}

fn parse_pgvector_literal(s: &str) -> Result<Vec<f32>, ImportError> {
    let inner = s.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|t| {
            t.trim().parse::<f32>().map_err(|e| {
                ImportError::Vector(format!("bad pgvector component '{}': {e}", t.trim()))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_embed::{Database, DistanceMetric, Dtype, SearchParams};
    use serde_json::json;

    #[test]
    fn source_parses_aliases() {
        assert_eq!("Qdrant".parse::<Source>().unwrap(), Source::Qdrant);
        assert_eq!("pg".parse::<Source>().unwrap(), Source::Pgvector);
        assert!("milvus".parse::<Source>().is_err());
        assert_eq!(Source::Chroma.to_string(), "chroma");
    }

    #[test]
    fn parses_qdrant_jsonl() {
        let input = concat!(
            r#"{"id": 1, "vector": [1.0, 2.0, 3.0], "payload": {"city": "paris"}}"#,
            "\n\n",
            r#"{"id": "abc", "vector": [4.0, 5.0, 6.0], "payload": {"city": "rome"}}"#,
        );
        let pts = parse(&ParseOptions::new(Source::Qdrant), input).unwrap();
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].id, "1");
        assert_eq!(pts[0].vector, vec![1.0, 2.0, 3.0]);
        assert_eq!(pts[0].payload, json!({"city": "paris"}));
        assert_eq!(pts[1].id, "abc");
    }

    #[test]
    fn qdrant_named_vector_picks_the_sole_one() {
        let input = r#"{"id": 1, "vector": {"dense": [1.0, 2.0]}, "payload": {}}"#;
        let pts = parse(&ParseOptions::new(Source::Qdrant), input).unwrap();
        assert_eq!(pts[0].vector, vec![1.0, 2.0]);
    }

    #[test]
    fn qdrant_named_vector_requires_a_choice_when_ambiguous() {
        let input = r#"{"id": 1, "vector": {"a": [1.0], "b": [2.0]}, "payload": {}}"#;
        let err = parse(&ParseOptions::new(Source::Qdrant), input).unwrap_err();
        assert!(matches!(err, ImportError::Shape(Source::Qdrant, _)));
        let mut opts = ParseOptions::new(Source::Qdrant);
        opts.vector_name = Some("b".to_string());
        let pts = parse(&opts, input).unwrap();
        assert_eq!(pts[0].vector, vec![2.0]);
    }

    #[test]
    fn parses_pgvector_rows_with_text_literal_and_payload_columns() {
        let input = concat!(
            r#"{"id": 7, "embedding": "[1.5, 2.5]", "title": "a", "score": 9}"#,
            "\n",
            r#"{"id": 8, "embedding": [3.5, 4.5], "title": "b", "score": 4}"#,
        );
        let pts = parse(&ParseOptions::new(Source::Pgvector), input).unwrap();
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].id, "7");
        assert_eq!(pts[0].vector, vec![1.5, 2.5]);
        // id and embedding columns are consumed; the rest is payload.
        assert_eq!(pts[0].payload, json!({"title": "a", "score": 9}));
        assert_eq!(pts[1].vector, vec![3.5, 4.5]);
    }

    #[test]
    fn parses_chroma_get_object() {
        let input = r#"{
            "ids": ["x", "y"],
            "embeddings": [[1.0, 0.0], [0.0, 1.0]],
            "metadatas": [{"k": 1}, null],
            "documents": ["hello", null]
        }"#;
        let pts = parse(&ParseOptions::new(Source::Chroma), input).unwrap();
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].id, "x");
        assert_eq!(pts[0].vector, vec![1.0, 0.0]);
        assert_eq!(pts[0].payload, json!({"k": 1, "document": "hello"}));
        // y has null metadata and null document → empty payload object.
        assert_eq!(pts[1].payload, json!({}));
    }

    #[test]
    fn chroma_length_mismatch_is_rejected() {
        let input = r#"{"ids": ["x", "y"], "embeddings": [[1.0]]}"#;
        assert!(matches!(
            parse(&ParseOptions::new(Source::Chroma), input),
            Err(ImportError::Shape(Source::Chroma, _))
        ));
    }

    #[test]
    fn non_numeric_vector_component_is_rejected() {
        let input = r#"{"id": 1, "vector": [1.0, "nope"], "payload": {}}"#;
        assert!(matches!(
            parse(&ParseOptions::new(Source::Qdrant), input),
            Err(ImportError::Vector(_))
        ));
    }

    #[test]
    fn infer_dim_detects_mismatch_and_emptiness() {
        let ok = vec![
            ImportPoint {
                id: "a".into(),
                vector: vec![1.0, 2.0],
                payload: Value::Null,
            },
            ImportPoint {
                id: "b".into(),
                vector: vec![3.0, 4.0],
                payload: Value::Null,
            },
        ];
        assert_eq!(infer_dim(&ok).unwrap(), 2);
        let mixed = vec![
            ImportPoint {
                id: "a".into(),
                vector: vec![1.0],
                payload: Value::Null,
            },
            ImportPoint {
                id: "b".into(),
                vector: vec![1.0, 2.0],
                payload: Value::Null,
            },
        ];
        assert!(matches!(infer_dim(&mixed), Err(ImportError::Dim(_))));
        assert!(matches!(infer_dim(&[]), Err(ImportError::Dim(_))));
    }

    #[test]
    fn import_into_creates_loads_and_is_searchable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Database::open(tmp.path()).unwrap();
        let input = concat!(
            r#"{"id": "a", "vector": [1.0, 0.0, 0.0], "payload": {"tag": "x"}}"#,
            "\n",
            r#"{"id": "b", "vector": [0.0, 1.0, 0.0], "payload": {"tag": "y"}}"#,
        );
        let pts = parse(&ParseOptions::new(Source::Qdrant), input).unwrap();
        let dim = infer_dim(&pts).unwrap() as u32;
        let descriptor = Descriptor::new(dim, Dtype::F32, DistanceMetric::L2);
        let n = import_into(&mut db, "imported", descriptor, &pts).unwrap();
        assert_eq!(n, 2);
        assert_eq!(db.len("imported").unwrap(), 2);

        let res = db
            .search("imported", &[1.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(res[0].id, "a");

        // A second import appends rather than failing on the existing collection.
        let more = parse(
            &ParseOptions::new(Source::Qdrant),
            r#"{"id": "c", "vector": [0.0, 0.0, 1.0], "payload": {}}"#,
        )
        .unwrap();
        import_into(
            &mut db,
            "imported",
            Descriptor::new(3, Dtype::F32, DistanceMetric::L2),
            &more,
        )
        .unwrap();
        assert_eq!(db.len("imported").unwrap(), 3);
    }
}
