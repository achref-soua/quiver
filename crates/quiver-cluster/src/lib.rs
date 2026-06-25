// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in cluster-mode primitives (ADR-0065, increments 1–2).
//!
//! A Quiver cluster shards points across N independent single-writer engines and
//! fronts them with a stateless router. This crate is the **pure, dependency-light
//! core** the router composes — it does no I/O:
//!
//! - [`Shard`] — a single-writer **primary** plus optional **read replicas**
//!   (ordinary followers, ADR-0030); writes go to the primary, searches may be
//!   served by any of [`Shard::read_order`]'s `{primary} ∪ replicas`.
//! - [`ShardMap`] — an operator-declared list of shards (each addressed by URL),
//!   with replicas attached via [`ShardMap::add_replica`].
//! - [`ShardMap::shard_for`] — which shard owns a point id, by **rendezvous (HRW)
//!   hashing**: changing the shard set only remaps ~1/N of ids, not a full
//!   reshuffle, and the mapping is stable across releases (a fixed FNV-1a hash, not
//!   the std hasher, whose output is not guaranteed stable).
//! - [`ShardMap::partition`] — split a write batch into per-shard groups so it fans
//!   out in one request per owning shard.
//! - [`merge_top_k`] — combine each shard's local top-`k` into the exact global
//!   top-`k` (the scatter-gather merge).
//!
//! Single-node Quiver does not use any of this; the cluster is composition over the
//! existing engine, not a new one.

/// An error building or using a [`ShardMap`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClusterError {
    /// A cluster needs at least one shard.
    #[error("a cluster needs at least one shard URL")]
    NoShards,
    /// A shard URL was empty.
    #[error("shard {0} has an empty URL")]
    EmptyUrl(usize),
    /// A replica was declared for a shard index that does not exist.
    #[error("replica declared for shard {0}, but the cluster has {1} shard(s)")]
    ReplicaShardOutOfRange(usize, usize),
    /// A replica URL was empty.
    #[error("shard {0} has an empty replica URL")]
    EmptyReplicaUrl(usize),
}

/// One shard: a single-writer **primary** (ADR-0006) plus optional **read
/// replicas** — ordinary followers (ADR-0030) of that primary. Writes, gets and
/// deletes go to the primary; searches may be served by any of `{primary} ∪
/// replicas` to spread read load (eventually consistent — a replica lags its
/// primary by its replication delay).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shard {
    /// Stable index in the shard map; part of the hash key, so it must not change
    /// for an existing shard (appending a shard is fine).
    pub index: usize,
    /// Base URL of the shard's single-writer primary (e.g. `http://10.0.0.5:6333`).
    pub primary_url: String,
    /// Base URLs of the shard's read-replica followers (ADR-0030), if any. Each is
    /// an ordinary Quiver server run with `QUIVER_LEADER_URL` pointed at this
    /// shard's primary; a follower refuses writes, so a mis-route cannot corrupt.
    pub replica_urls: Vec<String>,
}

impl Shard {
    /// The URL to serve a read from, chosen round-robin across `{primary} ∪
    /// replicas` by `nth` (any monotonically increasing counter). A shard with no
    /// replicas always returns the primary, so single-primary shards are
    /// unaffected. The selection is a pure function of `nth`, so it is uniform over
    /// a sweep of counters and deterministic for a fixed one — the property the
    /// unit tests pin.
    #[must_use]
    pub fn read_url(&self, nth: usize) -> &str {
        self.target(nth % (1 + self.replica_urls.len()))
    }

    /// All of the shard's read targets in preference order for counter `nth`: the
    /// round-robin pick ([`read_url`](Self::read_url)) first, then the remaining
    /// `{primary} ∪ replicas` rotated after it. The router tries them in order so a
    /// down target (a stale or stopped replica, or — for reads only — a stopped
    /// primary) falls through to the next live one; the shard's slice is
    /// unavailable only if **every** target is down. This is read availability, not
    /// write failover: writes still go to the primary alone (no HA until the Raft
    /// increment).
    #[must_use]
    pub fn read_order(&self, nth: usize) -> Vec<&str> {
        let n = 1 + self.replica_urls.len();
        let start = nth % n;
        (0..n).map(|off| self.target((start + off) % n)).collect()
    }

    // Target `i` of `{primary} ∪ replicas`: 0 is the primary, 1..=replicas the
    // followers. Callers only ever pass `i < 1 + replica_urls.len()`.
    fn target(&self, i: usize) -> &str {
        match i {
            0 => &self.primary_url,
            i => &self.replica_urls[i - 1],
        }
    }
}

/// A static, operator-declared shard map (ADR-0065 increment 1). Online resharding
/// is a later increment; here the set is fixed at startup.
#[derive(Clone, Debug)]
pub struct ShardMap {
    shards: Vec<Shard>,
}

impl ShardMap {
    /// Build from an ordered list of shard base URLs (e.g. `QUIVER_CLUSTER_SHARDS`).
    /// Shard `i` is the `i`-th URL; the index is part of the hash key, so order is
    /// significant and stable.
    ///
    /// # Errors
    /// [`ClusterError::NoShards`] if the list is empty, [`ClusterError::EmptyUrl`]
    /// if any URL is blank.
    pub fn from_urls<I, S>(urls: I) -> Result<Self, ClusterError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let shards: Vec<Shard> = urls
            .into_iter()
            .enumerate()
            .map(|(index, url)| Shard {
                index,
                primary_url: url.into().trim().to_owned(),
                replica_urls: Vec::new(),
            })
            .collect();
        if shards.is_empty() {
            return Err(ClusterError::NoShards);
        }
        if let Some(s) = shards.iter().find(|s| s.primary_url.is_empty()) {
            return Err(ClusterError::EmptyUrl(s.index));
        }
        Ok(Self { shards })
    }

    /// Attach a read-replica URL to the shard at `shard_index`. The replica is an
    /// ordinary follower of that shard's primary (ADR-0030); the map only needs to
    /// know its URL so searches can fan reads to it.
    ///
    /// # Errors
    /// [`ClusterError::ReplicaShardOutOfRange`] if `shard_index` is not a shard,
    /// [`ClusterError::EmptyReplicaUrl`] if `url` is blank.
    pub fn add_replica<S: Into<String>>(
        &mut self,
        shard_index: usize,
        url: S,
    ) -> Result<(), ClusterError> {
        let url = url.into().trim().to_owned();
        if url.is_empty() {
            return Err(ClusterError::EmptyReplicaUrl(shard_index));
        }
        let count = self.shards.len();
        let shard = self
            .shards
            .get_mut(shard_index)
            .ok_or(ClusterError::ReplicaShardOutOfRange(shard_index, count))?;
        shard.replica_urls.push(url);
        Ok(())
    }

    /// Number of shards.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shards.len()
    }

    /// Whether the map has no shards (never true once built — `from_urls` rejects
    /// an empty list — but provided for completeness).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    /// All shards, in index order.
    #[must_use]
    pub fn shards(&self) -> &[Shard] {
        &self.shards
    }

    /// The shard that owns `point_id`, by rendezvous (HRW) hashing: the shard whose
    /// `hash(index ‖ id)` is highest wins. Deterministic, stable across releases,
    /// and minimal-reshuffle if the shard set changes.
    #[must_use]
    pub fn shard_for(&self, point_id: &str) -> &Shard {
        // `from_urls` guarantees at least one shard, so `max_by_key` is `Some`; the
        // `unwrap_or` fallback (shard 0) is unreachable but keeps this total and
        // panic-free (the project bans `unwrap`/`expect`).
        self.shards
            .iter()
            .max_by_key(|s| hrw_score(s.index, point_id))
            .unwrap_or(&self.shards[0])
    }

    /// Partition `items` into per-shard groups (preserving input order within each
    /// group), keyed by the owning shard's index. Only non-empty groups are
    /// returned. `id_of` extracts the point id each item is routed by.
    #[must_use]
    pub fn partition<'a, T, F>(&self, items: &'a [T], id_of: F) -> Vec<(usize, Vec<&'a T>)>
    where
        F: Fn(&T) -> &str,
    {
        let mut groups: Vec<Vec<&T>> = vec![Vec::new(); self.shards.len()];
        for item in items {
            let shard = self.shard_for(id_of(item));
            groups[shard.index].push(item);
        }
        groups
            .into_iter()
            .enumerate()
            .filter(|(_, g)| !g.is_empty())
            .collect()
    }
}

/// FNV-1a (64-bit) — a small, fast, **stable** hash. The std hasher's output is not
/// guaranteed stable across Rust versions, which would silently move a shard's data
/// on a toolchain bump; a fixed algorithm is required for a sharding key.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Rendezvous weight of a shard for a point id. The id is hashed once (FNV-1a),
/// combined with a golden-ratio-scaled shard seed, then run through a `splitmix64`
/// finalizer so the weight is well-distributed in *both* arguments — concatenating
/// a small shard index into the hash key avalanches poorly for short ids (an early
/// low-byte difference barely moves the comparison, skewing the assignment).
fn hrw_score(shard_index: usize, point_id: &str) -> u64 {
    let mut x = fnv1a(point_id.as_bytes())
        .wrapping_add((shard_index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    // splitmix64 finalizer.
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Merge each shard's local top-`k` ranked results into the exact global top-`k`
/// (the scatter-gather merge). `score_of` reads an item's score and
/// `higher_is_better` is the metric's ordering (`false` for L2 distance, `true` for
/// cosine/dot similarity). Exact when every shard returns its own top-`k`: a point
/// in the global top-`k` has at most `k-1` better points overall, hence at most
/// `k-1` better in its own shard, so it is in that shard's top-`k`.
#[must_use]
pub fn merge_top_k<T, F>(
    per_shard: Vec<Vec<T>>,
    k: usize,
    score_of: F,
    higher_is_better: bool,
) -> Vec<T>
where
    F: Fn(&T) -> f32,
{
    let mut all: Vec<T> = per_shard.into_iter().flatten().collect();
    all.sort_by(|a, b| {
        let (sa, sb) = (score_of(a), score_of(b));
        if higher_is_better {
            sb.total_cmp(&sa)
        } else {
            sa.total_cmp(&sb)
        }
    });
    all.truncate(k);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map3() -> ShardMap {
        ShardMap::from_urls(["http://a:6333", "http://b:6333", "http://c:6333"]).unwrap()
    }

    #[test]
    fn from_urls_validates() {
        assert_eq!(
            ShardMap::from_urls(Vec::<String>::new()).unwrap_err(),
            ClusterError::NoShards
        );
        assert_eq!(
            ShardMap::from_urls(["http://a", "  "]).unwrap_err(),
            ClusterError::EmptyUrl(1)
        );
        assert_eq!(map3().len(), 3);
        // A freshly built map has no replicas — every shard is primary-only.
        assert!(map3().shards().iter().all(|s| s.replica_urls.is_empty()));
    }

    #[test]
    fn add_replica_attaches_and_validates() {
        let mut m = map3();
        m.add_replica(1, "http://b2:6333").unwrap();
        m.add_replica(1, " http://b3:6333 ").unwrap(); // trimmed
        assert_eq!(m.shards()[0].replica_urls, Vec::<String>::new());
        assert_eq!(
            m.shards()[1].replica_urls,
            vec!["http://b2:6333".to_owned(), "http://b3:6333".to_owned()]
        );
        // Out-of-range shard index and empty URL are rejected.
        assert_eq!(
            m.add_replica(3, "http://x").unwrap_err(),
            ClusterError::ReplicaShardOutOfRange(3, 3)
        );
        assert_eq!(
            m.add_replica(0, "   ").unwrap_err(),
            ClusterError::EmptyReplicaUrl(0)
        );
    }

    #[test]
    fn read_url_falls_back_to_primary_with_no_replicas() {
        // A primary-only shard serves every read from the primary, for any counter.
        let m = map3();
        let s = &m.shards()[0];
        let p = s.primary_url.clone();
        for nth in 0..10 {
            assert_eq!(s.read_url(nth), p);
        }
    }

    #[test]
    fn read_url_round_robins_primary_then_replicas() {
        let mut m = map3();
        m.add_replica(0, "http://a2").unwrap();
        m.add_replica(0, "http://a3").unwrap();
        let s = &m.shards()[0];
        // Cycle is primary, replica0, replica1, primary, … — uniform over a sweep.
        let seq: Vec<&str> = (0..6).map(|n| s.read_url(n)).collect();
        assert_eq!(
            seq,
            [
                "http://a:6333",
                "http://a2",
                "http://a3",
                "http://a:6333",
                "http://a2",
                "http://a3"
            ]
        );
        // Every target is hit equally over a full number of cycles (uniform).
        let mut counts = std::collections::HashMap::new();
        for n in 0..3_000 {
            *counts.entry(s.read_url(n)).or_insert(0) += 1;
        }
        assert_eq!(counts.len(), 3);
        assert!(counts.values().all(|&c| c == 1_000));
    }

    #[test]
    fn read_order_is_the_pick_then_the_rest() {
        let mut m = map3();
        m.add_replica(0, "http://a2").unwrap();
        m.add_replica(0, "http://a3").unwrap();
        let s = &m.shards()[0];
        // The first element is always the round-robin pick; the rest rotate after it
        // so a failed pick falls through to the other live targets.
        assert_eq!(s.read_order(0), ["http://a:6333", "http://a2", "http://a3"]);
        assert_eq!(s.read_order(1), ["http://a2", "http://a3", "http://a:6333"]);
        assert_eq!(s.read_order(2), ["http://a3", "http://a:6333", "http://a2"]);
        // Element 0 always equals read_url for the same counter.
        for n in 0..6 {
            assert_eq!(s.read_order(n)[0], s.read_url(n));
        }
        // A primary-only shard has exactly one target: the primary.
        assert_eq!(
            m.shards()[1].read_order(7),
            [m.shards()[1].primary_url.as_str()]
        );
    }

    #[test]
    fn fnv1a_is_the_known_fixed_value() {
        // Pin the algorithm: a toolchain bump must never change these (data would move).
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"hello"), 0xa430_d846_80aa_bd0b);
    }

    #[test]
    fn shard_for_is_deterministic_and_total() {
        let m = map3();
        for id in ["p0", "user:42", "🦀", ""] {
            let a = m.shard_for(id).index;
            let b = m.shard_for(id).index;
            assert_eq!(a, b, "deterministic");
            assert!(a < 3, "in range");
        }
    }

    #[test]
    fn distribution_is_roughly_even() {
        let m = map3();
        let mut counts = [0usize; 3];
        for i in 0..9_000 {
            counts[m.shard_for(&format!("point-{i}")).index] += 1;
        }
        // Even split is 3000 each; allow ±20% for hash variance.
        for c in counts {
            assert!((2_400..=3_600).contains(&c), "uneven: {counts:?}");
        }
    }

    #[test]
    fn distribution_is_even_for_two_shards_and_short_ids() {
        // The case a naive HRW skewed: 2 shards and short, sequential ids (`p0`…).
        let m = ShardMap::from_urls(["http://a", "http://b"]).unwrap();
        let mut counts = [0usize; 2];
        for i in 0..2_000 {
            counts[m.shard_for(&format!("p{i}")).index] += 1;
        }
        for c in counts {
            assert!((800..=1_200).contains(&c), "two-shard skew: {counts:?}");
        }
    }

    #[test]
    fn rendezvous_minimises_reshuffle_when_a_shard_is_removed() {
        // HRW property: dropping a shard only remaps the ids that lived on it.
        let m4 = ShardMap::from_urls(["a", "b", "c", "d"]).unwrap();
        let m3 = ShardMap::from_urls(["a", "b", "c"]).unwrap(); // shard 3 removed
        let mut moved = 0;
        let mut moved_off_survivor = 0;
        for i in 0..3_000 {
            let id = format!("k{i}");
            let before = m4.shard_for(&id).index;
            let after = m3.shard_for(&id).index;
            if before != after {
                moved += 1;
                if before != 3 {
                    moved_off_survivor += 1;
                }
            }
        }
        // Every key that moved must have been on the removed shard 3 — survivors stay put.
        assert_eq!(moved_off_survivor, 0, "a survivor's keys moved");
        assert!(moved > 0, "removed-shard keys should remap");
    }

    #[test]
    fn partition_groups_by_owning_shard() {
        let m = map3();
        let ids: Vec<String> = (0..50).map(|i| format!("id{i}")).collect();
        let groups = m.partition(&ids, |s| s.as_str());
        // Every id appears exactly once, in its owning shard's group.
        let total: usize = groups.iter().map(|(_, g)| g.len()).sum();
        assert_eq!(total, ids.len());
        for (shard_idx, group) in &groups {
            assert!(!group.is_empty());
            for id in group {
                assert_eq!(m.shard_for(id).index, *shard_idx);
            }
        }
    }

    #[test]
    fn merge_top_k_is_exact_for_both_orderings() {
        // L2: lower is better. Three shards each returned their local top-2.
        let per_shard = vec![
            vec![("a", 0.1f32), ("b", 0.9)],
            vec![("c", 0.2), ("d", 0.5)],
            vec![("e", 0.05), ("f", 0.7)],
        ];
        let got = merge_top_k(per_shard, 3, |t| t.1, false);
        assert_eq!(got.iter().map(|t| t.0).collect::<Vec<_>>(), ["e", "a", "c"]);

        // Cosine/dot: higher is better.
        let per_shard = vec![vec![("x", 0.9f32)], vec![("y", 0.95), ("z", 0.3)]];
        let got = merge_top_k(per_shard, 2, |t| t.1, true);
        assert_eq!(got.iter().map(|t| t.0).collect::<Vec<_>>(), ["y", "x"]);
    }

    #[test]
    fn merge_top_k_handles_fewer_than_k_and_empty() {
        assert!(merge_top_k(vec![Vec::<(&str, f32)>::new()], 5, |t| t.1, false).is_empty());
        let got = merge_top_k(vec![vec![("a", 1.0f32)]], 10, |t| t.1, false);
        assert_eq!(got.len(), 1);
    }
}
