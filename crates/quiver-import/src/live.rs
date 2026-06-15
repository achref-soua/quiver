// SPDX-License-Identifier: AGPL-3.0-only
//! Live migration connectors (ADR-0027): pull points directly from a running
//! source over HTTP, normalizing each through the same per-source mapper the
//! offline importers use, so live and offline import share one write path.
//!
//! Qdrant is supported (its `points/scroll` API is stable and name-addressed).
//! Chroma and pgvector stay on the offline export path for now (ADR-0027).

use serde_json::{Value, json};

use crate::{ImportError, ImportPoint, ParseOptions, Source, qdrant_point};

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
}
