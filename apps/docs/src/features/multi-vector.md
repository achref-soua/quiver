# Multi-vector / late interaction (ColBERT)

Create a collection `multivector` and each **document** is stored as a *set* of
token vectors and ranked by **MaxSim** late interaction: for each query token, take
its best-matching document token, and sum those across the query. This is the
ColBERT retrieval model.

## How it works

Quiver models a document as a group of ordinary rows over the same row-addressed
store, so there is **no on-disk format change and the `kill -9` crash gate is
untouched**. The token pool is the set the ANN index serves (candidate generation);
candidates are then re-ranked by exact MaxSim with an optional payload filter. A
ColBERT corpus — a large pool of low-dimensional vectors — is exactly what the
IVF+PQ and disk paths were built to compress, so late interaction showcases the
[memory-frugality wedge](indexing.md).

Reachable from the embeddable database, [REST + gRPC](../api/rest-grpc.md), the
[MCP server](../api/mcp.md), and the [SDKs](../api/sdks.md):
`upsert_document` / `search_multi_vector` / `delete_document`.

## ColBERTv2 / PLAID compression

For multi-vector collections you can opt into a `colbert` index: coarse `kmeans`
centroids plus per-token *(centroid id, quantized residual code)* held in RAM, with
the exact token vectors on the encrypted store for the re-rank. Candidate generation
prunes by scoring centroids first (PLAID). It is derived and rebuilt from the store
on open, so the crash gate stays untouched. Create a multi-vector collection with
the `colbert` index over any transport or SDK.

## Maintenance

Document upsert/delete maintain the token-pool index **incrementally** (no full
rebuild), so a document write is size-independent.

The full design, including the deferred native variable-stride document-row storage
(gated on a reference-hardware locality measurement), is in
[ADR-0028](https://github.com/achref-soua/quiver/blob/main/docs/adr/0028-multi-vector-late-interaction.md)
and
[ADR-0034](https://github.com/achref-soua/quiver/blob/main/docs/adr/0034-multivector-followups.md).
