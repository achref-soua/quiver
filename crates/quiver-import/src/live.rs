// SPDX-License-Identifier: AGPL-3.0-only
//! Live migration connectors (ADR-0027): pull points directly from a running
//! source over HTTP, normalizing each through the same per-source mapper the
//! offline importers use, so live and offline import share one write path.
//!
//! Qdrant (ADR-0027), Chroma, and Postgres/pgvector (ADR-0029) are supported.
//! Qdrant uses its stable `points/scroll` API and Chroma its v2 `get` API, both
//! over the blocking `ureq` HTTP client; Postgres uses the blocking `postgres`
//! driver, reading each row as `row_to_json` so it reuses the offline pgvector
//! mapper.

use serde_json::{Value, json};

use crate::{
    ImportError, ImportPoint, ParseOptions, Source, chroma_points, pgvector_point, qdrant_point,
};

/// Connection details for a live Qdrant collection.
#[derive(Debug, Clone)]
pub struct QdrantSource {
    /// Base URL of the Qdrant REST API, e.g. `http://localhost:6333` (use
    /// `https://…` to verify TLS via the bundled roots).
    pub url: String,
    /// Collection name to read.
    pub collection: String,
    /// Optional value for the `api-key` header.
    pub api_key: Option<String>,
    /// Scroll page size — how many points to pull per request.
    pub batch: usize,
}

impl QdrantSource {
    /// A source for `collection` at `url`, with a default page size and no key.
    #[must_use]
    pub fn new(url: impl Into<String>, collection: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            collection: collection.into(),
            api_key: None,
            batch: 256,
        }
    }
}

/// Pull every point from a live Qdrant collection (ADR-0027), paginating the
/// `points/scroll` endpoint and normalizing each point through the shared Qdrant
/// mapper. Points are pulled in `batch`-sized pages, so each request stays small.
///
/// # Errors
/// [`ImportError::Http`] for a transport error or non-2xx response,
/// [`ImportError::Shape`] if the response is not the expected scroll envelope,
/// and the usual per-point mapping errors ([`ImportError::Vector`], …).
pub fn fetch_qdrant(
    src: &QdrantSource,
    opts: &ParseOptions,
) -> Result<Vec<ImportPoint>, ImportError> {
    let endpoint = format!(
        "{}/collections/{}/points/scroll",
        src.url.trim_end_matches('/'),
        src.collection
    );
    let limit = src.batch.max(1);
    let mut offset = Value::Null;
    let mut out = Vec::new();
    loop {
        let mut request = ureq::post(&endpoint);
        if let Some(key) = &src.api_key {
            request = request.set("api-key", key);
        }
        let response = request
            .send_json(json!({
                "limit": limit,
                "with_payload": true,
                "with_vector": true,
                "offset": offset,
            }))
            .map_err(|e| ImportError::Http(Source::Qdrant, e.to_string()))?;
        let value: Value = response
            .into_json()
            .map_err(|e| ImportError::Http(Source::Qdrant, format!("reading response: {e}")))?;

        let result = value.get("result").ok_or_else(|| {
            ImportError::Shape(Source::Qdrant, "response missing 'result'".to_string())
        })?;
        let points = result
            .get("points")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ImportError::Shape(Source::Qdrant, "result missing 'points' array".to_string())
            })?;
        for point in points {
            out.push(qdrant_point(opts, point.clone())?);
        }
        // Qdrant returns a null `next_page_offset` once the scroll is exhausted.
        match result.get("next_page_offset") {
            None | Some(Value::Null) => break,
            Some(next) => offset = next.clone(),
        }
    }
    Ok(out)
}

/// Connection details for a live Chroma collection (ADR-0029).
#[derive(Debug, Clone)]
pub struct ChromaSource {
    /// Base URL of the Chroma server, e.g. `http://localhost:8000` (use
    /// `https://…` to verify TLS via the bundled roots).
    pub url: String,
    /// Collection name to read.
    pub collection: String,
    /// Chroma tenant (the v2 API is tenant-scoped; defaults to `default_tenant`).
    pub tenant: String,
    /// Chroma database (defaults to `default_database`).
    pub database: String,
    /// Optional value for the `x-chroma-token` header.
    pub api_key: Option<String>,
    /// Page size — how many records to pull per `get` request.
    pub batch: usize,
}

impl ChromaSource {
    /// A source for `collection` at `url`, with the default tenant/database, a
    /// default page size, and no token.
    #[must_use]
    pub fn new(url: impl Into<String>, collection: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            collection: collection.into(),
            tenant: "default_tenant".to_string(),
            database: "default_database".to_string(),
            api_key: None,
            batch: 256,
        }
    }
}

/// Pull every record from a live Chroma collection (ADR-0029) over its v2 HTTP
/// API: resolve the collection name to its id, then paginate the `get` endpoint
/// — requesting embeddings, metadatas, and documents — normalizing each page
/// through the shared Chroma mapper. Records are pulled in `batch`-sized pages.
///
/// # Errors
/// [`ImportError::Http`] for a transport error or non-2xx response,
/// [`ImportError::Shape`] if a response is not the expected v2 shape or the
/// collection is not found, and the usual per-record mapping errors
/// ([`ImportError::Vector`], …).
pub fn fetch_chroma(src: &ChromaSource) -> Result<Vec<ImportPoint>, ImportError> {
    let prefix = format!(
        "{}/api/v2/tenants/{}/databases/{}",
        src.url.trim_end_matches('/'),
        src.tenant,
        src.database
    );
    let id = resolve_collection_id(&prefix, src)?;
    let endpoint = format!("{prefix}/collections/{id}/get");
    let limit = src.batch.max(1);
    let mut offset = 0usize;
    let mut out = Vec::new();
    loop {
        let response = with_token(ureq::post(&endpoint), src)
            .send_json(json!({
                "include": ["embeddings", "metadatas", "documents"],
                "limit": limit,
                "offset": offset,
            }))
            .map_err(|e| ImportError::Http(Source::Chroma, e.to_string()))?;
        let value: Value = response
            .into_json()
            .map_err(|e| ImportError::Http(Source::Chroma, format!("reading response: {e}")))?;
        let obj = value.as_object().ok_or_else(|| {
            ImportError::Shape(Source::Chroma, "get response was not an object".to_string())
        })?;
        let page = chroma_points(obj)?;
        let got = page.len();
        out.extend(page);
        // A short page (fewer than the requested limit) ends the scroll.
        if got < limit {
            break;
        }
        offset += got;
    }
    Ok(out)
}

// Resolve a Chroma collection name to its id by listing collections and matching
// on name. The `get` path is keyed by id, and whether a collection path accepts a
// name has varied across Chroma versions, so listing is the version-robust
// resolution (ADR-0029).
fn resolve_collection_id(prefix: &str, src: &ChromaSource) -> Result<String, ImportError> {
    let limit = src.batch.max(1);
    let mut offset = 0usize;
    loop {
        let endpoint = format!("{prefix}/collections?limit={limit}&offset={offset}");
        let response = with_token(ureq::get(&endpoint), src)
            .call()
            .map_err(|e| ImportError::Http(Source::Chroma, e.to_string()))?;
        let value: Value = response
            .into_json()
            .map_err(|e| ImportError::Http(Source::Chroma, format!("reading response: {e}")))?;
        let collections = value.as_array().ok_or_else(|| {
            ImportError::Shape(
                Source::Chroma,
                "collections list was not an array".to_string(),
            )
        })?;
        for c in collections {
            if c.get("name").and_then(Value::as_str) == Some(src.collection.as_str()) {
                return c
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        ImportError::Shape(
                            Source::Chroma,
                            format!("collection '{}' has no string id", src.collection),
                        )
                    });
            }
        }
        // A short page with no match means we have seen every collection.
        if collections.len() < limit {
            return Err(ImportError::Shape(
                Source::Chroma,
                format!("collection '{}' not found", src.collection),
            ));
        }
        offset += collections.len();
    }
}

// Attach the optional Chroma auth token header.
fn with_token(request: ureq::Request, src: &ChromaSource) -> ureq::Request {
    match &src.api_key {
        Some(key) => request.set("x-chroma-token", key),
        None => request,
    }
}

/// Connection details for a live Postgres / pgvector source (ADR-0029).
#[derive(Debug, Clone)]
pub struct PgvectorSource {
    /// libpq connection URL, e.g.
    /// `postgresql://user:pass@host:5432/db?sslmode=require`.
    pub url: String,
    /// Table to read, optionally schema-qualified (`schema.table`).
    pub table: String,
}

impl PgvectorSource {
    /// A source reading `table` from the Postgres database at `url`.
    #[must_use]
    pub fn new(url: impl Into<String>, table: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            table: table.into(),
        }
    }
}

/// Pull every row from a live Postgres table holding pgvector embeddings
/// (ADR-0029). Each row is read as `row_to_json(...)` — the same JSON shape the
/// offline pgvector path parses, with the id and vector columns named (per
/// `opts`) and every other column becoming payload — then normalized through the
/// shared pgvector mapper. TLS is negotiated according to the URL's `sslmode`.
///
/// # Errors
/// [`ImportError::Http`] if the connection or query fails, [`ImportError::Shape`]
/// for an unreadable row or an empty table name, and the usual per-row mapping
/// errors ([`ImportError::Vector`], …).
pub fn fetch_pgvector(
    src: &PgvectorSource,
    opts: &ParseOptions,
) -> Result<Vec<ImportPoint>, ImportError> {
    let tls = pgvector_tls()?;
    let mut client = postgres::Client::connect(&src.url, tls)
        .map_err(|e| ImportError::Http(Source::Pgvector, e.to_string()))?;
    let query = format!(
        "SELECT row_to_json(t) FROM (SELECT * FROM {}) t",
        quote_ident(&src.table)?
    );
    let rows = client
        .query(query.as_str(), &[])
        .map_err(|e| ImportError::Http(Source::Pgvector, e.to_string()))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let value: Value = row.try_get(0).map_err(|e| {
            ImportError::Shape(Source::Pgvector, format!("decoding row_to_json: {e}"))
        })?;
        out.push(pgvector_point(opts, value)?);
    }
    Ok(out)
}

// Build a rustls TLS connector for Postgres on the same `ring` provider and
// Mozilla roots the rest of Quiver uses (no OpenSSL, no aws-lc-rs). Whether TLS
// is actually used is governed by `sslmode` in the connection URL.
fn pgvector_tls() -> Result<tokio_postgres_rustls::MakeRustlsConnect, ImportError> {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ImportError::Http(Source::Pgvector, format!("tls setup: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

// Quote a possibly schema-qualified SQL identifier (`schema.table`) so the table
// name is interpolated safely: each dot-separated part is double-quoted with any
// embedded quote doubled.
fn quote_ident(ident: &str) -> Result<String, ImportError> {
    if ident.trim().is_empty() {
        return Err(ImportError::Shape(
            Source::Pgvector,
            "empty table name".to_string(),
        ));
    }
    let quoted = ident
        .split('.')
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(".");
    Ok(quoted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    // A throwaway HTTP/1.1 server that replies with each canned body in order,
    // one connection per reply (`Connection: close`), then stops. Returns the
    // bound base URL. Hermetic — no external Qdrant required.
    fn serve(bodies: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                read_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        format!("http://{addr}")
    }

    // Drain one HTTP request: headers up to the blank line, then Content-Length
    // bytes of body, so the client's write completes before we reply.
    fn read_request(stream: &mut std::net::TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header_end = end + 4;
                let content_length = String::from_utf8_lossy(&buf[..header_end])
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if buf.len() - header_end >= content_length {
                    return;
                }
            }
        }
    }

    #[test]
    fn fetch_qdrant_paginates_and_normalizes() {
        // Two scroll pages: the first carries a next_page_offset, the second
        // nulls it — exercising pagination end-to-end.
        let page1 = r#"{"result":{"points":[
            {"id":1,"vector":[1.0,2.0],"payload":{"k":"a"}},
            {"id":2,"vector":[3.0,4.0],"payload":{"k":"b"}}
        ],"next_page_offset":2},"status":"ok"}"#;
        let page2 = r#"{"result":{"points":[
            {"id":3,"vector":[5.0,6.0],"payload":{"k":"c"}}
        ],"next_page_offset":null},"status":"ok"}"#;
        let base = serve(vec![page1.to_string(), page2.to_string()]);

        let src = QdrantSource {
            batch: 2,
            ..QdrantSource::new(base, "c")
        };
        let points = fetch_qdrant(&src, &ParseOptions::new(Source::Qdrant)).unwrap();

        assert_eq!(points.len(), 3, "both pages were pulled");
        assert_eq!(points[0].id, "1");
        assert_eq!(points[0].vector, vec![1.0, 2.0]);
        assert_eq!(points[2].id, "3");
        assert_eq!(points[2].vector, vec![5.0, 6.0]);
        assert_eq!(points[1].payload["k"], serde_json::json!("b"));
    }

    #[test]
    fn fetch_qdrant_surfaces_a_bad_shape() {
        let base = serve(vec![r#"{"status":"ok"}"#.to_string()]);
        let src = QdrantSource::new(base, "c");
        assert!(matches!(
            fetch_qdrant(&src, &ParseOptions::new(Source::Qdrant)),
            Err(ImportError::Shape(Source::Qdrant, _))
        ));
    }

    #[test]
    fn fetch_chroma_resolves_paginates_and_normalizes() {
        // The list resolves the collection name to an id; then two `get` pages,
        // the second short, exercising pagination end-to-end.
        let list = r#"[{"id":"col-123","name":"docs"}]"#;
        let page1 = r#"{"ids":["a","b"],
            "embeddings":[[1.0,2.0],[3.0,4.0]],
            "metadatas":[{"k":"x"},null],
            "documents":["d1",null]}"#;
        let page2 = r#"{"ids":["c"],
            "embeddings":[[5.0,6.0]],
            "metadatas":[null],
            "documents":[null]}"#;
        let base = serve(vec![list.to_string(), page1.to_string(), page2.to_string()]);

        let src = ChromaSource {
            batch: 2,
            ..ChromaSource::new(base, "docs")
        };
        let points = fetch_chroma(&src).unwrap();

        assert_eq!(points.len(), 3, "both pages were pulled");
        assert_eq!(points[0].id, "a");
        assert_eq!(points[0].vector, vec![1.0, 2.0]);
        assert_eq!(points[0].payload["k"], serde_json::json!("x"));
        assert_eq!(points[0].payload["document"], serde_json::json!("d1"));
        // id "b" has null metadata and a null document → an empty payload object.
        assert_eq!(points[1].id, "b");
        assert_eq!(points[1].payload, serde_json::json!({}));
        assert_eq!(points[2].id, "c");
        assert_eq!(points[2].vector, vec![5.0, 6.0]);
    }

    #[test]
    fn fetch_chroma_reports_a_missing_collection() {
        let list = r#"[{"id":"other-id","name":"other"}]"#;
        let base = serve(vec![list.to_string()]);
        let src = ChromaSource::new(base, "docs");
        assert!(matches!(
            fetch_chroma(&src),
            Err(ImportError::Shape(Source::Chroma, _))
        ));
    }

    #[test]
    fn quote_ident_quotes_and_rejects_empty() {
        assert_eq!(quote_ident("items").unwrap(), "\"items\"");
        assert_eq!(quote_ident("public.items").unwrap(), "\"public\".\"items\"");
        // An embedded quote is doubled, so a crafted table name cannot break out.
        assert_eq!(quote_ident("a\"b").unwrap(), "\"a\"\"b\"");
        assert!(quote_ident("   ").is_err());
    }

    #[test]
    fn fetch_pgvector_reports_a_connection_error() {
        use std::net::TcpListener;
        // Bind then drop to get a port nothing is listening on, so the connect is
        // refused promptly — exercising TLS setup and the connection-error path
        // without a real server.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("postgresql://u@127.0.0.1:{port}/db?connect_timeout=2&sslmode=disable");
        let src = PgvectorSource::new(url, "items");
        assert!(matches!(
            fetch_pgvector(&src, &ParseOptions::new(Source::Pgvector)),
            Err(ImportError::Http(Source::Pgvector, _))
        ));
    }

    // A real-server check: point QUIVER_PG_TEST_URL (and optionally
    // QUIVER_PG_TEST_TABLE) at a running Postgres with a pgvector table, then run
    // `cargo test -p quiver-import -- --ignored`. Hermetic CI cannot fake the
    // Postgres wire protocol, so this is an operator step (ADR-0029).
    #[test]
    #[ignore = "needs a real Postgres+pgvector at QUIVER_PG_TEST_URL"]
    fn live_pgvector_import_against_a_real_server() {
        let url = std::env::var("QUIVER_PG_TEST_URL").expect("QUIVER_PG_TEST_URL must be set");
        let table = std::env::var("QUIVER_PG_TEST_TABLE").unwrap_or_else(|_| "items".to_string());
        let src = PgvectorSource::new(url, table);
        let points = fetch_pgvector(&src, &ParseOptions::new(Source::Pgvector)).unwrap();
        assert!(!points.is_empty(), "expected at least one row");
    }
}
