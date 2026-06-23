# Quiver Go SDK

A small, **standard-library-only** Go client for the Quiver vector database. It
mirrors the REST surface (`docs/api/rest-grpc.md`): collections, points, search,
hybrid / full-text (BM25) search, server-side embedding (`UpsertText` /
`SearchText`), fetch, snapshots, and the bulk/maintenance helpers `UpsertBatch`
(batched upload), `Scroll` (page through a collection via a callback), and
`DeleteByFilter` (paged erasure). Every call takes a `context.Context`.

```go
import quiver "github.com/achref-soua/quiver/sdks/go"
```

## Quickstart

```go
ctx := context.Background()
c := quiver.New("http://localhost:8080", quiver.WithAPIKey("secret"))

// Create a collection and add points.
c.CreateCollection(ctx, "kb", 4, nil)
c.Upsert(ctx, "kb", []quiver.Point{
    {ID: "a", Vector: []float32{1, 0, 0, 0}, Payload: map[string]any{"lang": "en"}},
    {ID: "b", Vector: []float32{0, 1, 0, 0}},
})

// Dense search.
hits, _ := c.Search(ctx, "kb", []float32{1, 0, 0, 0}, &quiver.SearchOptions{K: 5})

// Hybrid (dense ⊕ BM25, fused with RRF).
hits, _ = c.HybridSearch(ctx, "kb", &quiver.HybridOptions{
    Vector:    []float32{1, 0, 0, 0},
    QueryText: "hello world",
})

// Full-text only.
hits, _ = c.HybridSearch(ctx, "kb", &quiver.HybridOptions{QueryText: "hello"})

// Server-side embedding (requires an [embedding.kb] provider on the server).
c.UpsertText(ctx, "kb", []quiver.TextPoint{{ID: "d", Text: "the quick brown fox"}})
hits, _ = c.SearchText(ctx, "kb", "a fast fox", &quiver.SearchTextOptions{Rerank: true})

// Online backup (admin).
info, _ := c.Snapshot(ctx, "/backups/snap1")
_ = info
```

## Options & errors

- Methods take a pointer options struct (`nil` for defaults); `K` defaults to 10,
  `EfSearch` to 64, `RRFK0` to 60, payloads are included by default. To exclude
  payloads pass `WithPayload: quiver.Bool(false)`.
- Non-2xx responses return a `*quiver.APIError` carrying the HTTP `Status` and the
  RFC-9457 `Detail`. `GetPoint` returns `(nil, nil)` for a missing point (404).
- Configure timeouts/TLS with `quiver.WithHTTPClient(&http.Client{…})`.

## Tests

```sh
just test-go      # gofmt -l + go vet + go test (httptest-mocked; no server needed)
```
