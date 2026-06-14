// SPDX-License-Identifier: AGPL-3.0-only
//! `quiver admin` subcommands.
//!
//! `import` loads an export from another vector database (Qdrant, Chroma, or
//! pgvector) into a Quiver collection (ADR-0024), opening the target database the
//! same way `serve` does — an envelope key-ring from the master key, encrypted at
//! rest unless `--insecure` — so imported data is immediately serveable. See
//! `docs/migration.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use quiver_crypto::EnvelopeKeyRing;
use quiver_embed::{Database, Descriptor, DistanceMetric, Dtype, FieldType, FilterableField};
use quiver_import::{ParseOptions, Source, import_into, infer_dim, parse};

// Typed arguments for `quiver admin import`, decoupled from clap so the command
// logic is unit-testable without spawning the binary.
pub(crate) struct ImportArgs {
    pub source: Source,
    pub input: PathBuf,
    pub collection: String,
    pub data_dir: PathBuf,
    pub metric: DistanceMetric,
    pub dim: Option<usize>,
    pub filterable: Vec<FilterableField>,
    pub id_field: Option<String>,
    pub vector_field: Option<String>,
    pub vector_name: Option<String>,
    pub encryption_key: Option<String>,
    pub insecure: bool,
}

// Parse the export, then create/append the collection and bulk-load it. Returns
// the number of points imported.
pub(crate) fn import(args: ImportArgs) -> anyhow::Result<usize> {
    let text = std::fs::read_to_string(&args.input)
        .with_context(|| format!("reading export {}", args.input.display()))?;

    let mut opts = ParseOptions::new(args.source);
    if let Some(field) = args.id_field {
        opts.id_field = field;
    }
    if let Some(field) = args.vector_field {
        opts.vector_field = field;
    }
    opts.vector_name = args.vector_name;

    let points = parse(&opts, &text)?;
    if points.is_empty() {
        bail!("no points found in {}", args.input.display());
    }
    let dim = match args.dim {
        Some(d) => d,
        None => infer_dim(&points)?,
    };
    let descriptor =
        Descriptor::new(dim as u32, Dtype::F32, args.metric).with_filterable(args.filterable);

    let mut db = open_database(
        &args.data_dir,
        args.encryption_key.as_deref(),
        args.insecure,
    )?;
    Ok(import_into(&mut db, &args.collection, descriptor, &points)?)
}

// A distance metric from its CLI name.
pub(crate) fn parse_metric(name: &str) -> anyhow::Result<DistanceMetric> {
    match name.to_ascii_lowercase().as_str() {
        "l2" | "euclidean" => Ok(DistanceMetric::L2),
        "cosine" => Ok(DistanceMetric::Cosine),
        "dot" | "ip" => Ok(DistanceMetric::Dot),
        other => bail!("unknown metric '{other}' (expected l2, cosine, or dot)"),
    }
}

// Filterable fields from `path:type` specs (type = keyword | numeric).
pub(crate) fn parse_filterable(specs: &[String]) -> anyhow::Result<Vec<FilterableField>> {
    specs
        .iter()
        .map(|spec| {
            let (path, ty) = spec
                .rsplit_once(':')
                .ok_or_else(|| anyhow!("filterable '{spec}' must be path:type"))?;
            let field_type = match ty.to_ascii_lowercase().as_str() {
                "keyword" | "string" => FieldType::Keyword,
                "numeric" | "number" => FieldType::Numeric,
                other => bail!("filterable '{spec}': unknown type '{other}' (keyword|numeric)"),
            };
            Ok(FilterableField {
                path: path.to_string(),
                field_type,
            })
        })
        .collect()
}

// Open the target database the same way the server does (ADR-0010): an envelope
// key-ring derived from the master key, or plaintext only in insecure mode.
fn open_database(data_dir: &Path, key: Option<&str>, insecure: bool) -> anyhow::Result<Database> {
    match key {
        Some(key) => {
            let keyring = EnvelopeKeyRing::from_hex(key, data_dir)
                .map_err(|e| anyhow!("invalid master key: {e}"))?;
            Ok(Database::open_with_keyring(data_dir, Box::new(keyring))?)
        }
        None => {
            if !insecure {
                bail!(
                    "no encryption key set: encryption-at-rest is on by default — set QUIVER_ENCRYPTION_KEY or pass --insecure"
                );
            }
            Ok(Database::open(data_dir)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_embed::SearchParams;
    use std::io::Write;

    #[test]
    fn metric_and_filterable_parsing() {
        assert_eq!(parse_metric("Cosine").unwrap(), DistanceMetric::Cosine);
        assert_eq!(parse_metric("l2").unwrap(), DistanceMetric::L2);
        assert!(parse_metric("nope").is_err());

        let f = parse_filterable(&["user.age:numeric".to_string(), "city:keyword".to_string()])
            .unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].path, "user.age");
        assert_eq!(f[0].field_type, FieldType::Numeric);
        assert_eq!(f[1].field_type, FieldType::Keyword);
        assert!(parse_filterable(&["bad".to_string()]).is_err());
        assert!(parse_filterable(&["x:weird".to_string()]).is_err());
    }

    #[test]
    fn imports_qdrant_jsonl_into_a_local_db() {
        let dir = tempfile::tempdir().unwrap();
        let export = dir.path().join("qdrant.jsonl");
        let mut f = std::fs::File::create(&export).unwrap();
        writeln!(
            f,
            r#"{{"id": 1, "vector": [1.0, 0.0, 0.0], "payload": {{"city": "paris"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"id": 2, "vector": [0.0, 1.0, 0.0], "payload": {{"city": "rome"}}}}"#
        )
        .unwrap();
        drop(f);
        let data_dir = dir.path().join("data");

        let args = ImportArgs {
            source: Source::Qdrant,
            input: export,
            collection: "places".to_string(),
            data_dir: data_dir.clone(),
            metric: DistanceMetric::L2,
            dim: None,
            filterable: vec![FilterableField {
                path: "city".to_string(),
                field_type: FieldType::Keyword,
            }],
            id_field: None,
            vector_field: None,
            vector_name: None,
            encryption_key: None,
            insecure: true,
        };
        assert_eq!(import(args).unwrap(), 2);

        // Reopen the same data dir and confirm the points are searchable.
        let mut db = Database::open(&data_dir).unwrap();
        assert_eq!(db.len("places").unwrap(), 2);
        let res = db
            .search("places", &[1.0, 0.0, 0.0], &SearchParams::default())
            .unwrap();
        assert_eq!(res[0].id, "1");
    }

    #[test]
    fn encrypted_import_reopens_via_the_serve_path() {
        let dir = tempfile::tempdir().unwrap();
        let export = dir.path().join("q.jsonl");
        std::fs::write(
            &export,
            "{\"id\": 1, \"vector\": [1.0, 2.0], \"payload\": {}}\n",
        )
        .unwrap();
        let data_dir = dir.path().join("data");
        let key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

        let args = ImportArgs {
            source: Source::Qdrant,
            input: export,
            collection: "c".to_string(),
            data_dir: data_dir.clone(),
            metric: DistanceMetric::L2,
            dim: None,
            filterable: Vec::new(),
            id_field: None,
            vector_field: None,
            vector_name: None,
            encryption_key: Some(key.to_string()),
            insecure: false,
        };
        assert_eq!(import(args).unwrap(), 1);

        // Reopen with the same envelope key-ring `serve` uses.
        let keyring = EnvelopeKeyRing::from_hex(key, &data_dir).unwrap();
        let db = Database::open_with_keyring(&data_dir, Box::new(keyring)).unwrap();
        assert_eq!(db.len("c").unwrap(), 1);
    }

    #[test]
    fn refuses_to_import_without_a_key_unless_insecure() {
        let dir = tempfile::tempdir().unwrap();
        let export = dir.path().join("q.jsonl");
        std::fs::write(&export, "{\"id\": 1, \"vector\": [1.0], \"payload\": {}}\n").unwrap();
        let args = ImportArgs {
            source: Source::Qdrant,
            input: export,
            collection: "c".to_string(),
            data_dir: dir.path().join("data"),
            metric: DistanceMetric::L2,
            dim: None,
            filterable: Vec::new(),
            id_field: None,
            vector_field: None,
            vector_name: None,
            encryption_key: None,
            insecure: false,
        };
        assert!(import(args).is_err());
    }
}
