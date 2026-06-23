// Package quiver is the Go client for the Quiver vector database, mirroring the
// REST surface (docs/api/rest-grpc.md): collections, points, search, hybrid /
// full-text (BM25) search, server-side embedding (upsert_text / search_text),
// and snapshots. It depends only on the standard library.
package quiver

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
)

// Client talks to a Quiver server over REST.
type Client struct {
	baseURL string
	apiKey  string
	http    *http.Client
}

// Option configures a Client.
type Option func(*Client)

// WithAPIKey sets the bearer token sent on every request.
func WithAPIKey(key string) Option { return func(c *Client) { c.apiKey = key } }

// WithHTTPClient overrides the underlying *http.Client (e.g. to set timeouts or TLS).
func WithHTTPClient(h *http.Client) Option { return func(c *Client) { c.http = h } }

// New returns a Client for the server at baseURL (e.g. "http://localhost:8080").
func New(baseURL string, opts ...Option) *Client {
	c := &Client{baseURL: strings.TrimRight(baseURL, "/"), http: http.DefaultClient}
	for _, o := range opts {
		o(c)
	}
	return c
}

// Bool returns a pointer to b — for the optional *bool fields (e.g. WithPayload).
func Bool(b bool) *bool { return &b }

// --- types ---

// FilterableField declares a payload field indexed for pre-filtered search.
type FilterableField struct {
	Path      string `json:"path"`
	FieldType string `json:"field_type,omitempty"` // "keyword" (default) or "numeric"
}

// CreateCollectionOptions are the optional knobs for CreateCollection.
type CreateCollectionOptions struct {
	Metric           string // "l2" (default), "cosine", "dot"
	Index            string // "hnsw" (default), "vamana", "disk_vamana", "ivf", "colbert"
	PQSubspaces      int    // 0 = unset
	Filterable       []FilterableField
	Multivector      bool
	VectorEncryption string // "none" (default), "dcpe", "client_side"
}

// CollectionInfo is a collection's metadata. Unmodeled fields are ignored.
type CollectionInfo struct {
	Name        string `json:"name"`
	Dim         int    `json:"dim"`
	Metric      string `json:"metric"`
	Count       uint64 `json:"count"`
	Multivector bool   `json:"multivector"`
}

// Point is a dense vector with an optional JSON payload.
type Point struct {
	ID      string         `json:"id"`
	Vector  []float32      `json:"vector"`
	Payload map[string]any `json:"payload,omitempty"`
}

// TextPoint is a document embedded server-side (ADR-0047).
type TextPoint struct {
	ID      string         `json:"id"`
	Text    string         `json:"text"`
	Payload map[string]any `json:"payload,omitempty"`
}

// Match is one search or fetch result.
type Match struct {
	ID      string         `json:"id"`
	Score   float32        `json:"score"`
	Payload map[string]any `json:"payload,omitempty"`
	Vector  []float32      `json:"vector,omitempty"`
}

// SparseVector is a sparse query vector for hybrid search (ADR-0043).
type SparseVector struct {
	Indices []uint32  `json:"indices"`
	Values  []float32 `json:"values"`
}

// SnapshotInfo is what a Snapshot captured (ADR-0050).
type SnapshotInfo struct {
	ManifestVersion uint64 `json:"manifest_version"`
	Files           uint64 `json:"files"`
	Bytes           uint64 `json:"bytes"`
}

// SearchOptions are the optional knobs for Search.
type SearchOptions struct {
	K           int            // default 10
	EfSearch    int            // default 64
	Filter      map[string]any // optional payload filter tree
	WithPayload *bool          // default true
	WithVector  bool           // default false
}

// HybridOptions configure HybridSearch. At least one of Vector, Sparse, or
// QueryText must be set.
type HybridOptions struct {
	Vector      []float32
	Sparse      *SparseVector
	QueryText   string
	K           int // default 10
	EfSearch    int // default 64
	RRFK0       int // default 60
	Filter      map[string]any
	WithPayload *bool // default true
	WithVector  bool
}

// SearchTextOptions configure SearchText.
type SearchTextOptions struct {
	K           int  // default 10
	EfSearch    int  // default 64
	RRFK0       int  // default 60
	Rerank      bool // default false
	Filter      map[string]any
	WithPayload *bool // default true
	WithVector  bool
}

// FetchOptions configure Fetch.
type FetchOptions struct {
	Limit       int // default 100
	Filter      map[string]any
	WithPayload *bool // default true
	WithVector  bool
}

// --- error ---

// APIError is a non-2xx response from the server.
type APIError struct {
	Status int
	Detail string
}

func (e *APIError) Error() string {
	return fmt.Sprintf("quiver: HTTP %d: %s", e.Status, e.Detail)
}

// --- collections ---

// CreateCollection creates a collection of dimensionality dim.
func (c *Client) CreateCollection(ctx context.Context, name string, dim int, opts *CreateCollectionOptions) (*CollectionInfo, error) {
	if opts == nil {
		opts = &CreateCollectionOptions{}
	}
	metric := opts.Metric
	if metric == "" {
		metric = "l2"
	}
	body := map[string]any{"name": name, "dim": dim, "metric": metric}
	if opts.Index != "" {
		body["index"] = opts.Index
	}
	if opts.PQSubspaces > 0 {
		body["pq_subspaces"] = opts.PQSubspaces
	}
	if len(opts.Filterable) > 0 {
		body["filterable"] = opts.Filterable
	}
	if opts.Multivector {
		body["multivector"] = true
	}
	if opts.VectorEncryption != "" && opts.VectorEncryption != "none" {
		body["vector_encryption"] = opts.VectorEncryption
	}
	var out CollectionInfo
	if err := c.do(ctx, http.MethodPost, "/v1/collections", body, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ListCollections returns all collections.
func (c *Client) ListCollections(ctx context.Context) ([]CollectionInfo, error) {
	var out []CollectionInfo
	if err := c.do(ctx, http.MethodGet, "/v1/collections", nil, &out); err != nil {
		return nil, err
	}
	return out, nil
}

// GetCollection fetches one collection's metadata.
func (c *Client) GetCollection(ctx context.Context, name string) (*CollectionInfo, error) {
	var out CollectionInfo
	if err := c.do(ctx, http.MethodGet, "/v1/collections/"+pathEscape(name), nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// DeleteCollection deletes a collection, returning whether it existed.
func (c *Client) DeleteCollection(ctx context.Context, name string) (bool, error) {
	var out struct {
		Existed bool `json:"existed"`
	}
	if err := c.do(ctx, http.MethodDelete, "/v1/collections/"+pathEscape(name), nil, &out); err != nil {
		return false, err
	}
	return out.Existed, nil
}

// --- points ---

// Upsert inserts or replaces points, returning the number upserted.
func (c *Client) Upsert(ctx context.Context, collection string, points []Point) (uint64, error) {
	var out struct {
		Upserted uint64 `json:"upserted"`
	}
	body := map[string]any{"points": points}
	err := c.do(ctx, http.MethodPost, "/v1/collections/"+pathEscape(collection)+"/points", body, &out)
	return out.Upserted, err
}

// DeletePoints deletes points by id, returning the number deleted.
func (c *Client) DeletePoints(ctx context.Context, collection string, ids []string) (uint64, error) {
	var out struct {
		Deleted uint64 `json:"deleted"`
	}
	body := map[string]any{"ids": ids}
	err := c.do(ctx, http.MethodDelete, "/v1/collections/"+pathEscape(collection)+"/points", body, &out)
	return out.Deleted, err
}

// GetPoint fetches one point by id, or (nil, nil) if it does not exist.
func (c *Client) GetPoint(ctx context.Context, collection, id string) (*Match, error) {
	path := "/v1/collections/" + pathEscape(collection) + "/points/" + pathEscape(id)
	var out Match
	err := c.do(ctx, http.MethodGet, path, nil, &out)
	if err != nil {
		var apiErr *APIError
		if asAPIError(err, &apiErr) && apiErr.Status == http.StatusNotFound {
			return nil, nil
		}
		return nil, err
	}
	return &out, nil
}

// --- search ---

// Search returns the k nearest points to vector, nearest first.
func (c *Client) Search(ctx context.Context, collection string, vector []float32, opts *SearchOptions) ([]Match, error) {
	if opts == nil {
		opts = &SearchOptions{}
	}
	body := map[string]any{
		"vector":       vector,
		"k":            orDefault(opts.K, 10),
		"ef_search":    orDefault(opts.EfSearch, 64),
		"with_payload": payloadDefault(opts.WithPayload),
		"with_vector":  opts.WithVector,
	}
	if opts.Filter != nil {
		body["filter"] = opts.Filter
	}
	return c.matches(ctx, "/v1/collections/"+pathEscape(collection)+"/query", body)
}

// HybridSearch fuses dense, sparse, and/or full-text (BM25) results with RRF
// (ADR-0043/0046). At least one of Vector, Sparse, or QueryText is required.
func (c *Client) HybridSearch(ctx context.Context, collection string, opts *HybridOptions) ([]Match, error) {
	if opts == nil || (opts.Vector == nil && opts.Sparse == nil && opts.QueryText == "") {
		return nil, fmt.Errorf("quiver: HybridSearch requires a dense vector, a sparse vector, or a text query")
	}
	body := map[string]any{
		"k":            orDefault(opts.K, 10),
		"ef_search":    orDefault(opts.EfSearch, 64),
		"rrf_k0":       orDefault(opts.RRFK0, 60),
		"with_payload": payloadDefault(opts.WithPayload),
		"with_vector":  opts.WithVector,
	}
	if opts.Vector != nil {
		body["vector"] = opts.Vector
	}
	if opts.QueryText != "" {
		body["query_text"] = opts.QueryText
	}
	if opts.Sparse != nil {
		body["sparse_indices"] = opts.Sparse.Indices
		body["sparse_values"] = opts.Sparse.Values
	}
	if opts.Filter != nil {
		body["filter"] = opts.Filter
	}
	return c.matches(ctx, "/v1/collections/"+pathEscape(collection)+"/query/hybrid", body)
}

// UpsertText embeds each point's text server-side and upserts it (ADR-0047);
// the text is also indexed for BM25. Requires an [embedding.<collection>]
// provider on the server. Returns the number upserted.
func (c *Client) UpsertText(ctx context.Context, collection string, points []TextPoint) (uint64, error) {
	var out struct {
		Upserted uint64 `json:"upserted"`
	}
	body := map[string]any{"points": points}
	err := c.do(ctx, http.MethodPost, "/v1/collections/"+pathEscape(collection)+"/points:text", body, &out)
	return out.Upserted, err
}

// SearchText embeds text server-side and searches dense ⊕ BM25, optionally
// reranking the candidate pool in one call (ADR-0047).
func (c *Client) SearchText(ctx context.Context, collection, text string, opts *SearchTextOptions) ([]Match, error) {
	if opts == nil {
		opts = &SearchTextOptions{}
	}
	body := map[string]any{
		"text":         text,
		"k":            orDefault(opts.K, 10),
		"ef_search":    orDefault(opts.EfSearch, 64),
		"rrf_k0":       orDefault(opts.RRFK0, 60),
		"with_payload": payloadDefault(opts.WithPayload),
		"with_vector":  opts.WithVector,
		"rerank":       opts.Rerank,
	}
	if opts.Filter != nil {
		body["filter"] = opts.Filter
	}
	return c.matches(ctx, "/v1/collections/"+pathEscape(collection)+"/query/text", body)
}

// Fetch lists points without ranking; returned matches carry score 0.
func (c *Client) Fetch(ctx context.Context, collection string, opts *FetchOptions) ([]Match, error) {
	if opts == nil {
		opts = &FetchOptions{}
	}
	body := map[string]any{
		"limit":        orDefault(opts.Limit, 100),
		"with_payload": payloadDefault(opts.WithPayload),
		"with_vector":  opts.WithVector,
	}
	if opts.Filter != nil {
		body["filter"] = opts.Filter
	}
	// The fetch endpoint returns {"points": [...]} (not {"matches": [...]} like
	// the ranked queries); each fetched point carries score 0.
	var out struct {
		Points []Match `json:"points"`
	}
	if err := c.do(ctx, http.MethodPost, "/v1/collections/"+pathEscape(collection)+"/fetch", body, &out); err != nil {
		return nil, err
	}
	return out.Points, nil
}

// ScrollOptions configures Scroll.
type ScrollOptions struct {
	Batch       int            // page size, default 500
	Filter      map[string]any // narrow the set (recommended for large collections)
	WithPayload *bool          // default true
	WithVector  bool
}

// UpsertBatch upserts a large slice of points in server-friendly batches (each no
// larger than batch, which must stay within the server's max_batch_size — ADR-0040,
// default 1000), returning the total upserted. It stops at the first error,
// including a context cancellation observed between batches. batch <= 0 defaults
// to 500. The Python upsert_iter / TypeScript upsertIter analogue.
func (c *Client) UpsertBatch(ctx context.Context, collection string, points []Point, batch int) (uint64, error) {
	if batch <= 0 {
		batch = 500
	}
	var total uint64
	for start := 0; start < len(points); start += batch {
		if err := ctx.Err(); err != nil {
			return total, err
		}
		end := start + batch
		if end > len(points) {
			end = len(points)
		}
		n, err := c.Upsert(ctx, collection, points[start:end])
		if err != nil {
			return total, err
		}
		total += n
	}
	return total, nil
}

// Scroll lists points page by page and calls fn for each, for export or
// re-embedding. The REST fetch is limit-bounded without a server cursor, so this
// returns up to opts.Batch points in one page; pass a narrowing Filter for large
// collections (a server-side scroll cursor is a follow-up). fn returning a
// non-nil error stops the scroll and that error is returned. Mirrors the Python
// async scroll generator.
func (c *Client) Scroll(ctx context.Context, collection string, opts *ScrollOptions, fn func(Match) error) error {
	if opts == nil {
		opts = &ScrollOptions{}
	}
	page, err := c.Fetch(ctx, collection, &FetchOptions{
		Limit:       opts.Batch,
		Filter:      opts.Filter,
		WithPayload: opts.WithPayload,
		WithVector:  opts.WithVector,
	})
	if err != nil {
		return err
	}
	for _, m := range page {
		if err := fn(m); err != nil {
			return err
		}
	}
	return nil
}

// DeleteByFilter deletes every point matching filter, returning the number
// deleted. It fetches matching ids in pages of batch and deletes them until none
// remain — useful for GDPR erasure and re-indexing. batch <= 0 defaults to 500.
func (c *Client) DeleteByFilter(ctx context.Context, collection string, filter map[string]any, batch int) (uint64, error) {
	if batch <= 0 {
		batch = 500
	}
	noPayload := false
	var total uint64
	for {
		page, err := c.Fetch(ctx, collection, &FetchOptions{Limit: batch, Filter: filter, WithPayload: &noPayload})
		if err != nil {
			return total, err
		}
		if len(page) == 0 {
			return total, nil
		}
		ids := make([]string, len(page))
		for i, m := range page {
			ids[i] = m.ID
		}
		n, err := c.DeletePoints(ctx, collection, ids)
		if err != nil {
			return total, err
		}
		total += n
		if len(page) < batch {
			return total, nil
		}
	}
}

// Snapshot takes a consistent online snapshot of the whole database into a
// server-local directory (ADR-0050). Admin-only.
func (c *Client) Snapshot(ctx context.Context, destination string) (*SnapshotInfo, error) {
	var out SnapshotInfo
	body := map[string]any{"destination": destination}
	if err := c.do(ctx, http.MethodPost, "/v1/snapshot", body, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// --- internals ---

func (c *Client) matches(ctx context.Context, path string, body any) ([]Match, error) {
	var out struct {
		Matches []Match `json:"matches"`
	}
	if err := c.do(ctx, http.MethodPost, path, body, &out); err != nil {
		return nil, err
	}
	return out.Matches, nil
}

func (c *Client) do(ctx context.Context, method, path string, body, out any) error {
	var reader io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		reader = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reader)
	if err != nil {
		return err
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if c.apiKey != "" {
		req.Header.Set("Authorization", "Bearer "+c.apiKey)
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(resp.Body)
	if err != nil {
		return err
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return &APIError{Status: resp.StatusCode, Detail: problemDetail(data)}
	}
	if out != nil && len(data) > 0 {
		return json.Unmarshal(data, out)
	}
	return nil
}

// problemDetail pulls `detail` out of an RFC-9457 problem+json body, falling back
// to the raw body.
func problemDetail(data []byte) string {
	var p struct {
		Detail string `json:"detail"`
	}
	if err := json.Unmarshal(data, &p); err == nil && p.Detail != "" {
		return p.Detail
	}
	return strings.TrimSpace(string(data))
}

func orDefault(v, def int) int {
	if v <= 0 {
		return def
	}
	return v
}

func payloadDefault(p *bool) bool {
	if p == nil {
		return true
	}
	return *p
}

// pathEscape escapes a single path segment (collection / id) for a URL.
func pathEscape(s string) string {
	// url.PathEscape would over-escape "/"; segments here never contain one, but
	// escape the characters that matter for a path segment.
	r := strings.NewReplacer("%", "%25", " ", "%20", "?", "%3F", "#", "%23", "/", "%2F")
	return r.Replace(s)
}

func asAPIError(err error, target **APIError) bool {
	if e, ok := err.(*APIError); ok {
		*target = e
		return true
	}
	return false
}
