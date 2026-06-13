// SPDX-License-Identifier: AGPL-3.0-only
//! Disk-resident Vamana index — the memory-frugal serve path (ADR-0019).
//!
//! A built [`Vamana`] graph and a trained [`ProductQuantizer`] are written to a
//! single page-structured file: `[meta][codebook][ids][PQ codes][node blocks]`,
//! every page sealed with a [`PageCodec`] (so the index is encrypted at rest
//! exactly like the store). On [`DiskVamana::open`] the meta, codebook, ids, and
//! **PQ codes are read into RAM** while the node-block region is `mmap`-ed and
//! decrypted on demand — so a 10M-vector index serves from roughly its PQ-code
//! footprint plus the OS-resident working set, not the full vectors.
//!
//! Each node block co-locates a node's full-precision vector and its
//! out-neighbors, so one page read yields both. [`DiskVamana::search`] navigates
//! by the RAM-resident PQ codes (cheap, approximate), reads the visited nodes'
//! pages for neighbors and full vectors, and **re-ranks with exact distances** —
//! recovering the recall lost to PQ compression.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use quiver_core::page::{PAGE_BODY_CAP, PAGE_SIZE, PageCodec, PageType, build_page, parse_page};
use quiver_simd::Metric;

use crate::{IndexError, Neighbor, ProductQuantizer, Quantizer, Vamana};

/// On-disk format version for the disk index (independent of the product
/// SemVer); bumped only on a layout change.
const FORMAT_VERSION: u16 = 1;

/// Errors from building or querying a disk index.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DiskError {
    /// An I/O error reading or writing the index file.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A page/codec error (bad magic, CRC, version, or decrypt failure).
    #[error(transparent)]
    Core(#[from] quiver_core::CoreError),
    /// Meta/codebook (de)serialization failed.
    #[error("index serialization: {0}")]
    Serde(#[from] postcard::Error),
    /// The index file is structurally invalid.
    #[error("disk index format: {0}")]
    Format(String),
    /// An index-level error (dimension/metric mismatch).
    #[error(transparent)]
    Index(#[from] IndexError),
}

type Result<T> = std::result::Result<T, DiskError>;

// Per-node on-disk block: [vector: dim×f32][count: u32][neighbors: R×u32].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskMeta {
    format_version: u16,
    n: u64,
    dim: u32,
    r: u32,
    metric: Metric,
    medoid: u32,
    code_len: u32,
    node_stride: u32,
    nodes_per_page: u32,
    codebook_pages: u32,
    ids_pages: u32,
    codes_pages: u32,
    node_pages: u32,
}

/// Search parameters for a [`DiskVamana::search`].
#[derive(Debug, Clone, Copy)]
pub struct DiskSearchParams {
    /// Search-list width (`L`); higher trades latency for recall. Clamped up to
    /// at least `k`.
    pub l_search: usize,
}

impl Default for DiskSearchParams {
    fn default() -> Self {
        Self { l_search: 100 }
    }
}

/// Unit-normalize for cosine; pass through otherwise (matches [`Vamana`]).
fn prepare(metric: Metric, v: &[f32]) -> Vec<f32> {
    match metric {
        Metric::Cosine => {
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                v.iter().map(|x| x / norm).collect()
            } else {
                v.to_vec()
            }
        }
        Metric::L2 | Metric::Dot => v.to_vec(),
    }
}

/// Write a built `graph` and trained `pq` to an encrypted disk index at `path`.
///
/// `pq` must have been trained for the same dimensionality and metric as the
/// graph. `codec` seals every page (use a `PlainCodec` for plaintext or the
/// `quiver-crypto` AEAD codec for encryption-at-rest).
///
/// # Errors
/// Returns [`DiskError::Index`] on a dim/metric mismatch, [`DiskError::Format`]
/// if a node block would exceed a page, or an I/O / serialization error.
pub fn write(
    path: &Path,
    graph: &Vamana,
    pq: &ProductQuantizer,
    codec: &dyn PageCodec,
) -> Result<()> {
    let n = graph.len();
    let dim = graph.dim();
    let r = graph.max_degree();
    let metric = graph.metric();
    if pq.dim() != dim || pq.metric() != metric {
        return Err(DiskError::Index(IndexError::InvalidConfig(
            "quantizer dim/metric does not match the graph",
        )));
    }

    let node_stride = dim * 4 + 4 + r * 4;
    if node_stride > PAGE_BODY_CAP {
        return Err(DiskError::Format(format!(
            "node block {node_stride} B exceeds page capacity {PAGE_BODY_CAP} B (dim too large)"
        )));
    }
    let nodes_per_page = (PAGE_BODY_CAP / node_stride).max(1);
    let code_len = pq.code_len();

    // RAM-resident regions: the codebook, the external ids, the PQ codes.
    let codebook = postcard::to_allocvec(pq)?;
    let mut ids_blob = Vec::with_capacity(n * 8);
    for &id in graph.ids() {
        ids_blob.extend_from_slice(&id.to_le_bytes());
    }
    let mut codes = vec![0u8; n * code_len];
    for i in 0..n {
        pq.encode_into(
            graph.vector(i as u32),
            &mut codes[i * code_len..(i + 1) * code_len],
        );
    }

    let codebook_pages = page_count(codebook.len());
    let ids_pages = page_count(ids_blob.len());
    let codes_pages = page_count(codes.len());
    let node_pages = n.div_ceil(nodes_per_page);

    let meta = DiskMeta {
        format_version: FORMAT_VERSION,
        n: n as u64,
        dim: dim as u32,
        r: r as u32,
        metric,
        medoid: graph.medoid(),
        code_len: code_len as u32,
        node_stride: node_stride as u32,
        nodes_per_page: nodes_per_page as u32,
        codebook_pages: codebook_pages as u32,
        ids_pages: ids_pages as u32,
        codes_pages: codes_pages as u32,
        node_pages: node_pages as u32,
    };
    let meta_blob = postcard::to_allocvec(&meta)?;
    if meta_blob.len() > PAGE_BODY_CAP {
        return Err(DiskError::Format("meta page overflow".into()));
    }

    let file = File::create(path)?;
    let mut w = BufWriter::new(file);
    let mut page_id = 0u64;
    write_page(&mut w, &mut page_id, codec, &meta_blob)?;
    write_blob_pages(&mut w, &mut page_id, codec, &codebook)?;
    write_blob_pages(&mut w, &mut page_id, codec, &ids_blob)?;
    write_blob_pages(&mut w, &mut page_id, codec, &codes)?;

    // Node blocks: pack `nodes_per_page` fixed-stride blocks per page.
    let mut node = 0usize;
    while node < n {
        let count = nodes_per_page.min(n - node);
        let mut body = vec![0u8; count * node_stride];
        for slot in 0..count {
            let i = node + slot;
            let base = slot * node_stride;
            // Full-precision vector.
            for (d, &x) in graph.vector(i as u32).iter().enumerate() {
                body[base + d * 4..base + d * 4 + 4].copy_from_slice(&x.to_le_bytes());
            }
            // Neighbor count, then up to R neighbor ids (unused slots stay zero).
            let neighbors = graph.neighbors(i as u32);
            let kept = neighbors.len().min(r);
            let cbase = base + dim * 4;
            body[cbase..cbase + 4].copy_from_slice(&(kept as u32).to_le_bytes());
            for (j, &nb) in neighbors.iter().take(r).enumerate() {
                let nbase = cbase + 4 + j * 4;
                body[nbase..nbase + 4].copy_from_slice(&nb.to_le_bytes());
            }
        }
        write_page(&mut w, &mut page_id, codec, &body)?;
        node += count;
    }

    let file = w
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)?;
    file.sync_all()?;
    Ok(())
}

fn page_count(len: usize) -> usize {
    len.div_ceil(PAGE_BODY_CAP)
}

// Build, seal, and write one page; advances `page_id`.
fn write_page(
    w: &mut impl Write,
    page_id: &mut u64,
    codec: &dyn PageCodec,
    body: &[u8],
) -> Result<()> {
    let page = build_page(PageType::IndexBlock, *page_id, 0, body)?;
    let mut block = vec![0u8; codec.block_size()];
    codec.seal(*page_id, &page, &mut block)?;
    w.write_all(&block)?;
    *page_id += 1;
    Ok(())
}

// Write a byte blob across as many full pages as needed.
fn write_blob_pages(
    w: &mut impl Write,
    page_id: &mut u64,
    codec: &dyn PageCodec,
    blob: &[u8],
) -> Result<()> {
    if blob.is_empty() {
        return Ok(());
    }
    for chunk in blob.chunks(PAGE_BODY_CAP) {
        write_page(w, page_id, codec, chunk)?;
    }
    Ok(())
}

/// A disk-resident Vamana index opened for queries.
pub struct DiskVamana {
    mmap: Mmap,
    codec: Box<dyn PageCodec>,
    meta: DiskMeta,
    pq: ProductQuantizer,
    ids: Vec<u64>,
    codes: Vec<u8>,
    node_region_page0: u64,
}

impl DiskVamana {
    /// Open the disk index at `path`, decrypting with `codec` (which must match
    /// the one used to write it). Reads the meta, codebook, ids, and PQ codes
    /// into RAM and `mmap`s the node-block region.
    ///
    /// # Errors
    /// Returns an error on I/O failure, a wrong/garbled codec (decrypt failure),
    /// or a structurally invalid file.
    pub fn open(path: &Path, codec: Box<dyn PageCodec>) -> Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the disk index is an immutable artifact — it is written once
        // by `write` and never mutated in place — so the mapped bytes do not
        // change underneath us for the lifetime of the mapping.
        let mmap = unsafe { Mmap::map(&file)? };

        let block_size = codec.block_size();
        let meta_body = read_page_body(&mmap, 0, block_size, codec.as_ref())?;
        let meta: DiskMeta = postcard::from_bytes(&meta_body)?;
        if meta.format_version != FORMAT_VERSION {
            return Err(DiskError::Format(format!(
                "unsupported disk index version {}",
                meta.format_version
            )));
        }

        let mut page = 1u64;
        let codebook = read_region(
            &mmap,
            &mut page,
            meta.codebook_pages,
            block_size,
            codec.as_ref(),
        )?;
        let pq: ProductQuantizer = postcard::from_bytes(&codebook)?;

        let ids_blob = read_region(&mmap, &mut page, meta.ids_pages, block_size, codec.as_ref())?;
        let n = meta.n as usize;
        if ids_blob.len() < n * 8 {
            return Err(DiskError::Format("ids region too short".into()));
        }
        let ids: Vec<u64> = ids_blob[..n * 8]
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap_or_default()))
            .collect();

        let codes_blob = read_region(
            &mmap,
            &mut page,
            meta.codes_pages,
            block_size,
            codec.as_ref(),
        )?;
        let codes_len = n * meta.code_len as usize;
        if codes_blob.len() < codes_len {
            return Err(DiskError::Format("codes region too short".into()));
        }
        let codes = codes_blob[..codes_len].to_vec();

        Ok(Self {
            mmap,
            codec,
            meta,
            pq,
            ids,
            codes,
            node_region_page0: page,
        })
    }

    /// Number of vectors in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.meta.n as usize
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.meta.n == 0
    }

    /// Search for the `k` nearest neighbors to `query`, closest first.
    ///
    /// # Errors
    /// Returns [`DiskError::Index`] on a dimensionality mismatch, or an I/O /
    /// decrypt error reading a node page.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        params: &DiskSearchParams,
    ) -> Result<Vec<Neighbor>> {
        let dim = self.meta.dim as usize;
        if query.len() != dim {
            return Err(DiskError::Index(IndexError::DimensionMismatch {
                expected: dim,
                got: query.len(),
            }));
        }
        if self.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let metric = self.meta.metric;
        let prepared = prepare(metric, query);
        let scorer = self.pq.scorer(&prepared);
        let code_len = self.meta.code_len as usize;
        let approx = |node: u32| -> f32 {
            let start = node as usize * code_len;
            scorer.distance(&self.codes[start..start + code_len])
        };
        let l = params.l_search.max(k);

        // PQ-navigated greedy beam search; expanded nodes' full vectors are kept
        // for the exact re-rank.
        let medoid = self.meta.medoid;
        let mut working: Vec<(f32, u32)> = vec![(approx(medoid), medoid)];
        let mut in_working: HashSet<u32> = HashSet::from([medoid]);
        let mut visited: HashSet<u32> = HashSet::new();
        let mut expanded: Vec<(u32, Vec<f32>)> = Vec::new();

        while let Some(&(_, node)) = working
            .iter()
            .filter(|(_, nd)| !visited.contains(nd))
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
        {
            visited.insert(node);
            let (vector, neighbors) = self.read_node(node)?;
            expanded.push((node, vector));
            for nb in neighbors {
                if in_working.insert(nb) {
                    working.push((approx(nb), nb));
                }
            }
            working.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            if working.len() > l {
                for (_, nd) in working.drain(l..) {
                    in_working.remove(&nd);
                }
            }
        }

        // Exact re-rank over the full-precision vectors we read from disk.
        let mut scored: Vec<(f32, u32, f32)> = expanded
            .iter()
            .map(|(nd, v)| {
                (
                    rank_distance(metric, &prepared, v),
                    *nd,
                    report_distance(metric, &prepared, v),
                )
            })
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        Ok(scored
            .into_iter()
            .take(k)
            .map(|(_, nd, report)| Neighbor {
                id: self.ids[nd as usize],
                distance: report,
            })
            .collect())
    }

    // Read (and decrypt) a node's page, returning its full vector and neighbors.
    fn read_node(&self, node: u32) -> Result<(Vec<f32>, Vec<u32>)> {
        let dim = self.meta.dim as usize;
        let nodes_per_page = self.meta.nodes_per_page as usize;
        let node_stride = self.meta.node_stride as usize;
        let region_page = node as usize / nodes_per_page;
        let file_page = self.node_region_page0 + region_page as u64;
        let body = read_page_body(
            &self.mmap,
            file_page,
            self.codec.block_size(),
            self.codec.as_ref(),
        )?;
        let base = (node as usize % nodes_per_page) * node_stride;

        let vec_bytes = &body[base..base + dim * 4];
        let vector: Vec<f32> = vec_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap_or_default()))
            .collect();
        let cbase = base + dim * 4;
        let count =
            u32::from_le_bytes(body[cbase..cbase + 4].try_into().unwrap_or_default()) as usize;
        let r = self.meta.r as usize;
        let count = count.min(r);
        let mut neighbors = Vec::with_capacity(count);
        for j in 0..count {
            let nbase = cbase + 4 + j * 4;
            neighbors.push(u32::from_le_bytes(
                body[nbase..nbase + 4].try_into().unwrap_or_default(),
            ));
        }
        Ok((vector, neighbors))
    }
}

// Exact "smaller is closer" distance for re-rank ordering (vectors are prepared).
fn rank_distance(metric: Metric, q: &[f32], v: &[f32]) -> f32 {
    match metric {
        Metric::L2 => quiver_simd::l2_sq_f32(q, v),
        Metric::Cosine => -quiver_simd::cosine_f32(q, v),
        Metric::Dot => -quiver_simd::dot_f32(q, v),
    }
}

// The metric value reported to the caller (un-negated for similarities).
fn report_distance(metric: Metric, q: &[f32], v: &[f32]) -> f32 {
    match metric {
        Metric::L2 => quiver_simd::l2_sq_f32(q, v),
        Metric::Cosine => quiver_simd::cosine_f32(q, v),
        Metric::Dot => quiver_simd::dot_f32(q, v),
    }
}

// Decrypt and validate one file page, returning a copy of its live body.
fn read_page_body(
    mmap: &Mmap,
    file_page: u64,
    block_size: usize,
    codec: &dyn PageCodec,
) -> Result<Vec<u8>> {
    let off = file_page as usize * block_size;
    let block = mmap
        .get(off..off + block_size)
        .ok_or_else(|| DiskError::Format(format!("page {file_page} out of range")))?;
    let mut page = [0u8; PAGE_SIZE];
    codec.open(file_page, block, &mut page)?;
    let (_, body) = parse_page(&page, PageType::IndexBlock)?;
    Ok(body.to_vec())
}

// Read `count` consecutive pages starting at `*page`, concatenating their bodies;
// advances `*page` past the region.
fn read_region(
    mmap: &Mmap,
    page: &mut u64,
    count: u32,
    block_size: usize,
    codec: &dyn PageCodec,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for _ in 0..count {
        out.extend_from_slice(&read_page_body(mmap, *page, block_size, codec)?);
        *page += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VamanaConfig;
    use crate::rng::SplitMix64;
    use quiver_core::page::PlainCodec;
    use std::collections::HashSet as Set;

    fn rand_vec(rng: &mut SplitMix64, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| (rng.next_f64() as f32) * 2.0 - 1.0)
            .collect()
    }

    // A reversible non-identity codec, to prove pages are actually sealed and the
    // disk index round-trips through a transform other than the identity.
    #[derive(Debug)]
    struct XorCodec(u8);
    impl PageCodec for XorCodec {
        fn block_size(&self) -> usize {
            PAGE_SIZE
        }
        fn seal(
            &self,
            _id: u64,
            plaintext: &[u8; PAGE_SIZE],
            out: &mut [u8],
        ) -> std::result::Result<(), quiver_core::CoreError> {
            for (o, &p) in out.iter_mut().zip(plaintext.iter()) {
                *o = p ^ self.0;
            }
            Ok(())
        }
        fn open(
            &self,
            _id: u64,
            block: &[u8],
            out: &mut [u8; PAGE_SIZE],
        ) -> std::result::Result<(), quiver_core::CoreError> {
            for (o, &b) in out.iter_mut().zip(block.iter()) {
                *o = b ^ self.0;
            }
            Ok(())
        }
        fn clone_box(&self) -> Box<dyn PageCodec> {
            Box::new(XorCodec(self.0))
        }
    }

    fn build_disk(
        dir: &std::path::Path,
        n: usize,
        dim: usize,
        metric: Metric,
        codec: &dyn PageCodec,
    ) -> (Vec<Vec<f32>>, std::path::PathBuf) {
        let mut rng = SplitMix64::new(0xD15C ^ n as u64);
        let data: Vec<Vec<f32>> = (0..n).map(|_| rand_vec(&mut rng, dim)).collect();
        let flat: Vec<f32> = data.iter().flatten().copied().collect();
        let ids: Vec<u64> = (0..n as u64).collect();
        let graph = Vamana::build(&ids, &flat, dim, metric, VamanaConfig::default()).unwrap();
        let pq = ProductQuantizer::train(&flat, n, dim, dim / 4, metric, 7).unwrap();
        let path = dir.join("index.qvx");
        write(&path, &graph, &pq, codec).unwrap();
        (data, path)
    }

    fn brute_force(data: &[Vec<f32>], q: &[f32], k: usize, metric: Metric) -> Set<usize> {
        let mut scored: Vec<(f32, usize)> = data
            .iter()
            .enumerate()
            .map(|(i, v)| {
                (
                    rank_distance(metric, &prepare(metric, q), &prepare(metric, v)),
                    i,
                )
            })
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scored.into_iter().take(k).map(|(_, i)| i).collect()
    }

    #[test]
    fn disk_index_recall_matches_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let (dim, n, queries, k) = (32, 1000, 50, 10);
        let (data, path) = build_disk(tmp.path(), n, dim, Metric::L2, &PlainCodec);
        let idx = DiskVamana::open(&path, Box::new(PlainCodec)).unwrap();
        assert_eq!(idx.len(), n);

        let mut rng = SplitMix64::new(0xA11CE);
        let mut hits = 0usize;
        for _ in 0..queries {
            let q = rand_vec(&mut rng, dim);
            let truth = brute_force(&data, &q, k, Metric::L2);
            let got = idx.search(&q, k, &DiskSearchParams::default()).unwrap();
            hits += got
                .iter()
                .filter(|nbr| truth.contains(&(nbr.id as usize)))
                .count();
        }
        let recall = hits as f64 / (queries * k) as f64;
        // PQ navigation loses a little; exact re-rank recovers most of it.
        assert!(recall >= 0.90, "disk recall@10 was {recall:.3}");
    }

    #[test]
    fn cosine_disk_index_searches() {
        let tmp = tempfile::tempdir().unwrap();
        let (dim, n, k) = (24, 600, 10);
        let (data, path) = build_disk(tmp.path(), n, dim, Metric::Cosine, &PlainCodec);
        let idx = DiskVamana::open(&path, Box::new(PlainCodec)).unwrap();
        let mut rng = SplitMix64::new(0xC05);
        let mut hits = 0usize;
        for _ in 0..30 {
            let q = rand_vec(&mut rng, dim);
            let truth = brute_force(&data, &q, k, Metric::Cosine);
            let got = idx.search(&q, k, &DiskSearchParams::default()).unwrap();
            hits += got
                .iter()
                .filter(|nbr| truth.contains(&(nbr.id as usize)))
                .count();
        }
        assert!(hits as f64 / 300.0 >= 0.85, "cosine disk recall too low");
    }

    #[test]
    fn pages_are_sealed_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let dim = 16;
        // A recognizable payload pattern; under a non-identity codec it must not
        // appear verbatim on disk.
        let (_data, path) = build_disk(tmp.path(), 200, dim, Metric::L2, &XorCodec(0x5A));
        let bytes = std::fs::read(&path).unwrap();
        // The XOR'd file decrypts and searches correctly with the right codec.
        let idx = DiskVamana::open(&path, Box::new(XorCodec(0x5A))).unwrap();
        assert_eq!(idx.len(), 200);
        assert!(
            !idx.search(&vec![0.1; dim], 5, &DiskSearchParams::default())
                .unwrap()
                .is_empty()
        );
        // The page magic "QVPG" never appears in the sealed bytes.
        let magic = b"QVPG";
        assert!(
            !bytes.windows(4).any(|w| w == magic),
            "page magic leaked through the codec — pages not sealed"
        );
    }

    #[test]
    fn wrong_codec_fails_to_open() {
        let tmp = tempfile::tempdir().unwrap();
        let (_data, path) = build_disk(tmp.path(), 100, 8, Metric::L2, &XorCodec(0x11));
        // Opening XOR-sealed pages as plaintext must fail page validation.
        assert!(DiskVamana::open(&path, Box::new(PlainCodec)).is_err());
    }

    #[test]
    fn finds_exact_vector() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, path) = build_disk(tmp.path(), 300, 16, Metric::L2, &PlainCodec);
        let idx = DiskVamana::open(&path, Box::new(PlainCodec)).unwrap();
        let got = idx
            .search(&data[42], 1, &DiskSearchParams { l_search: 100 })
            .unwrap();
        assert_eq!(got[0].id, 42);
    }
}
