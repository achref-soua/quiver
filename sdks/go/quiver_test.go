package quiver

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"
)

// recorded captures what the mock server received.
type recorded struct {
	method string
	path   string
	auth   string
	body   map[string]any
}

// mock returns a server that records the request and replies with respJSON, plus
// a pointer to the last recorded request.
func mock(t *testing.T, status int, respJSON string) (*httptest.Server, *recorded) {
	t.Helper()
	rec := &recorded{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		rec.method = r.Method
		rec.path = r.URL.Path
		rec.auth = r.Header.Get("Authorization")
		if b, _ := io.ReadAll(r.Body); len(b) > 0 {
			_ = json.Unmarshal(b, &rec.body)
		}
		w.WriteHeader(status)
		_, _ = io.WriteString(w, respJSON)
	}))
	t.Cleanup(srv.Close)
	return srv, rec
}

func TestCreateCollectionSendsDefaultsAndAuth(t *testing.T) {
	srv, rec := mock(t, 200, `{"name":"kb","dim":4,"metric":"l2","count":0}`)
	c := New(srv.URL, WithAPIKey("secret"))
	info, err := c.CreateCollection(context.Background(), "kb", 4, nil)
	if err != nil {
		t.Fatal(err)
	}
	if rec.method != "POST" || rec.path != "/v1/collections" {
		t.Fatalf("got %s %s", rec.method, rec.path)
	}
	if rec.auth != "Bearer secret" {
		t.Fatalf("auth header = %q", rec.auth)
	}
	if rec.body["metric"] != "l2" || rec.body["name"] != "kb" {
		t.Fatalf("body = %v", rec.body)
	}
	if info.Name != "kb" || info.Dim != 4 {
		t.Fatalf("info = %+v", info)
	}
}

func TestUpsertReturnsCount(t *testing.T) {
	srv, rec := mock(t, 200, `{"upserted":2}`)
	c := New(srv.URL)
	n, err := c.Upsert(context.Background(), "kb", []Point{
		{ID: "a", Vector: []float32{1, 0, 0, 0}, Payload: map[string]any{"n": 1}},
		{ID: "b", Vector: []float32{0, 1, 0, 0}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if n != 2 {
		t.Fatalf("upserted = %d", n)
	}
	if rec.path != "/v1/collections/kb/points" {
		t.Fatalf("path = %s", rec.path)
	}
	pts := rec.body["points"].([]any)
	if len(pts) != 2 {
		t.Fatalf("points = %v", pts)
	}
}

func TestSearchAppliesDefaultsAndParsesMatches(t *testing.T) {
	srv, rec := mock(t, 200, `{"matches":[{"id":"a","score":0.5,"payload":{"n":1}},{"id":"b","score":0.9}]}`)
	c := New(srv.URL)
	got, err := c.Search(context.Background(), "kb", []float32{1, 0, 0, 0}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 2 || got[0].ID != "a" || got[0].Score != 0.5 {
		t.Fatalf("matches = %+v", got)
	}
	// Defaults: k=10, ef_search=64, with_payload=true, with_vector=false.
	if rec.body["k"].(float64) != 10 || rec.body["ef_search"].(float64) != 64 {
		t.Fatalf("defaults not applied: %v", rec.body)
	}
	if rec.body["with_payload"] != true || rec.body["with_vector"] != false {
		t.Fatalf("payload/vector defaults: %v", rec.body)
	}
}

func TestHybridSearchRequiresAQuery(t *testing.T) {
	c := New("http://unused")
	if _, err := c.HybridSearch(context.Background(), "kb", nil); err == nil {
		t.Fatal("expected an error for an empty hybrid query")
	}
	if _, err := c.HybridSearch(context.Background(), "kb", &HybridOptions{}); err == nil {
		t.Fatal("expected an error when no query side is set")
	}
}

func TestHybridSearchSendsAllSides(t *testing.T) {
	srv, rec := mock(t, 200, `{"matches":[]}`)
	c := New(srv.URL)
	_, err := c.HybridSearch(context.Background(), "kb", &HybridOptions{
		Vector:    []float32{1, 0},
		Sparse:    &SparseVector{Indices: []uint32{1, 2}, Values: []float32{0.5, 0.5}},
		QueryText: "hello",
	})
	if err != nil {
		t.Fatal(err)
	}
	if rec.path != "/v1/collections/kb/query/hybrid" {
		t.Fatalf("path = %s", rec.path)
	}
	if rec.body["query_text"] != "hello" {
		t.Fatalf("query_text missing: %v", rec.body)
	}
	if _, ok := rec.body["sparse_indices"]; !ok {
		t.Fatalf("sparse_indices missing: %v", rec.body)
	}
	if rec.body["rrf_k0"].(float64) != 60 {
		t.Fatalf("rrf_k0 default: %v", rec.body)
	}
}

func TestSearchTextSendsRerankFlag(t *testing.T) {
	srv, rec := mock(t, 200, `{"matches":[]}`)
	c := New(srv.URL)
	_, err := c.SearchText(context.Background(), "kb", "a query", &SearchTextOptions{Rerank: true, K: 5})
	if err != nil {
		t.Fatal(err)
	}
	if rec.path != "/v1/collections/kb/query/text" {
		t.Fatalf("path = %s", rec.path)
	}
	if rec.body["rerank"] != true || rec.body["text"] != "a query" || rec.body["k"].(float64) != 5 {
		t.Fatalf("body = %v", rec.body)
	}
}

func TestUpsertTextHitsTextEndpoint(t *testing.T) {
	srv, rec := mock(t, 200, `{"upserted":1}`)
	c := New(srv.URL)
	n, err := c.UpsertText(context.Background(), "kb", []TextPoint{{ID: "a", Text: "hello world"}})
	if err != nil {
		t.Fatal(err)
	}
	if n != 1 || rec.path != "/v1/collections/kb/points:text" {
		t.Fatalf("n=%d path=%s", n, rec.path)
	}
}

func TestSnapshotParsesInfo(t *testing.T) {
	srv, rec := mock(t, 200, `{"manifest_version":3,"files":12,"bytes":4096}`)
	c := New(srv.URL)
	info, err := c.Snapshot(context.Background(), "/backups/snap1")
	if err != nil {
		t.Fatal(err)
	}
	if info.Files != 12 || info.Bytes != 4096 || info.ManifestVersion != 3 {
		t.Fatalf("info = %+v", info)
	}
	if rec.body["destination"] != "/backups/snap1" {
		t.Fatalf("destination = %v", rec.body)
	}
}

func TestGetPointReturnsNilOn404(t *testing.T) {
	srv, _ := mock(t, 404, `{"detail":"not found"}`)
	c := New(srv.URL)
	m, err := c.GetPoint(context.Background(), "kb", "missing")
	if err != nil {
		t.Fatalf("404 should be (nil,nil), got err %v", err)
	}
	if m != nil {
		t.Fatalf("expected nil match, got %+v", m)
	}
}

func TestDeleteCollectionReportsExistence(t *testing.T) {
	srv, _ := mock(t, 200, `{"existed":true}`)
	c := New(srv.URL)
	existed, err := c.DeleteCollection(context.Background(), "kb")
	if err != nil || !existed {
		t.Fatalf("existed=%v err=%v", existed, err)
	}
}

func TestAPIErrorSurfacesStatusAndDetail(t *testing.T) {
	srv, _ := mock(t, 409, `{"detail":"already exists"}`)
	c := New(srv.URL)
	_, err := c.Snapshot(context.Background(), "/dup")
	apiErr, ok := err.(*APIError)
	if !ok {
		t.Fatalf("expected *APIError, got %T", err)
	}
	if apiErr.Status != 409 || apiErr.Detail != "already exists" {
		t.Fatalf("apiErr = %+v", apiErr)
	}
}

func TestWithPayloadFalseIsSent(t *testing.T) {
	srv, rec := mock(t, 200, `{"matches":[]}`)
	c := New(srv.URL)
	_, err := c.Search(context.Background(), "kb", []float32{1}, &SearchOptions{WithPayload: Bool(false)})
	if err != nil {
		t.Fatal(err)
	}
	if rec.body["with_payload"] != false {
		t.Fatalf("with_payload should be false: %v", rec.body)
	}
}
