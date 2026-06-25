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
    #[error("shard id {0} has an empty URL")]
    EmptyUrl(u64),
    /// A replica was declared for a shard id that is not in the map.
    #[error("replica declared for shard id {0}, which is not in the cluster")]
    UnknownReplicaShard(u64),
    /// A replica URL was empty.
    #[error("shard id {0} has an empty replica URL")]
    EmptyReplicaUrl(u64),
    /// Two shards share the same id (ids must be unique — they are the HRW key).
    #[error("duplicate shard id {0}")]
    DuplicateShardId(u64),
    /// A membership operation referenced a shard id that is not in the map.
    #[error("no shard with id {0}")]
    UnknownShard(u64),
}

/// One shard: a single-writer **primary** (ADR-0006) plus optional **read
/// replicas** — ordinary followers (ADR-0030) of that primary. Writes, gets and
/// deletes go to the primary; searches may be served by any of `{primary} ∪
/// replicas` to spread read load (eventually consistent — a replica lags its
/// primary by its replication delay).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Shard {
    /// Immutable shard id — **the HRW hash key** (ADR-0066). It is decoupled from the
    /// shard's position in the map and must **never change or be reused**: removing a
    /// shard drops its id and leaves every survivor's id (and therefore its data)
    /// untouched, so only the removed shard's `~1/N` slice remaps. `from_urls`
    /// assigns ids `0..N`; dynamic membership assigns the next free id at join.
    pub id: u64,
    /// Base URL of the shard's single-writer primary (e.g. `http://10.0.0.5:6333`).
    pub primary_url: String,
    /// Base URLs of the shard's read-replica followers (ADR-0030), if any. Each is
    /// an ordinary Quiver server run with `QUIVER_LEADER_URL` pointed at this
    /// shard's primary; a follower refuses writes, so a mis-route cannot corrupt.
    #[serde(default)]
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

/// An operator-declared shard map. It carries a monotonically increasing
/// **`version`** (ADR-0066): a coordinator owns the authoritative map and bumps the
/// version on every membership change, and a router refreshes the map into its
/// `ArcSwap`, ignoring any response whose version is not newer — so a membership
/// change propagates without restarting the router. A statically configured map
/// (`from_urls`) starts at version 0 and never changes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ShardMap {
    #[serde(default)]
    version: u64,
    shards: Vec<Shard>,
}

impl ShardMap {
    /// Build from an ordered list of shard base URLs (e.g. `QUIVER_CLUSTER_SHARDS`).
    /// Shard `i` is the `i`-th URL and is assigned id `i` — the HRW key — so a given
    /// id always maps to the same URL for this map. (Dynamic membership assigns ids
    /// out of band; see [`from_shards`](Self::from_shards).)
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
                id: index as u64,
                primary_url: url.into().trim().to_owned(),
                replica_urls: Vec::new(),
            })
            .collect();
        Self::from_shards(shards)
    }

    /// Build from explicit [`Shard`]s, whose ids may be **non-contiguous** (a gap is
    /// what a removed shard leaves). Ids must be unique — they are the HRW key.
    ///
    /// # Errors
    /// [`ClusterError::NoShards`] if empty, [`ClusterError::EmptyUrl`] if a primary
    /// URL is blank, [`ClusterError::DuplicateShardId`] if two shards share an id.
    pub fn from_shards(shards: Vec<Shard>) -> Result<Self, ClusterError> {
        if shards.is_empty() {
            return Err(ClusterError::NoShards);
        }
        if let Some(s) = shards.iter().find(|s| s.primary_url.is_empty()) {
            return Err(ClusterError::EmptyUrl(s.id));
        }
        let mut seen = std::collections::HashSet::with_capacity(shards.len());
        if let Some(dup) = shards.iter().find(|s| !seen.insert(s.id)) {
            return Err(ClusterError::DuplicateShardId(dup.id));
        }
        Ok(Self { version: 0, shards })
    }

    /// The map's version. A router only adopts a refreshed map whose version is
    /// strictly greater than the one it already holds (ADR-0066).
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Add a shard with the coordinator-assigned (monotonic, never-reused) `id` and
    /// bump the version. The coordinator owns id allocation; the map only enforces
    /// that the id is unique and the primary URL non-empty.
    ///
    /// # Errors
    /// [`ClusterError::DuplicateShardId`] if `id` is already present,
    /// [`ClusterError::EmptyUrl`] if `primary_url` is blank.
    pub fn add_shard<S: Into<String>>(
        &mut self,
        id: u64,
        primary_url: S,
        replica_urls: Vec<String>,
    ) -> Result<(), ClusterError> {
        let primary_url = primary_url.into().trim().to_owned();
        if primary_url.is_empty() {
            return Err(ClusterError::EmptyUrl(id));
        }
        if self.shards.iter().any(|s| s.id == id) {
            return Err(ClusterError::DuplicateShardId(id));
        }
        self.shards.push(Shard {
            id,
            primary_url,
            replica_urls,
        });
        self.version += 1;
        Ok(())
    }

    /// Remove the shard with `id` and bump the version. Refuses to remove the **last**
    /// shard (a cluster needs at least one). A removed id is never reused — its
    /// `~1/N` slice is the only data that remaps (ADR-0066).
    ///
    /// # Errors
    /// [`ClusterError::NoShards`] if it would empty the map,
    /// [`ClusterError::UnknownShard`] if `id` is not present.
    pub fn remove_shard(&mut self, id: u64) -> Result<(), ClusterError> {
        if !self.shards.iter().any(|s| s.id == id) {
            return Err(ClusterError::UnknownShard(id));
        }
        if self.shards.len() == 1 {
            return Err(ClusterError::NoShards);
        }
        self.shards.retain(|s| s.id != id);
        self.version += 1;
        Ok(())
    }

    /// Attach a read-replica URL to the shard with id `shard_id`. The replica is an
    /// ordinary follower of that shard's primary (ADR-0030); the map only needs to
    /// know its URL so searches can fan reads to it.
    ///
    /// # Errors
    /// [`ClusterError::UnknownReplicaShard`] if no shard has `shard_id`,
    /// [`ClusterError::EmptyReplicaUrl`] if `url` is blank.
    pub fn add_replica<S: Into<String>>(
        &mut self,
        shard_id: u64,
        url: S,
    ) -> Result<(), ClusterError> {
        let url = url.into().trim().to_owned();
        if url.is_empty() {
            return Err(ClusterError::EmptyReplicaUrl(shard_id));
        }
        let shard = self
            .shards
            .iter_mut()
            .find(|s| s.id == shard_id)
            .ok_or(ClusterError::UnknownReplicaShard(shard_id))?;
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
    /// `hash(shard_id ‖ point_id)` is highest wins. Deterministic, stable across
    /// releases, and minimal-reshuffle if the shard set changes (only the keys of a
    /// removed shard remap; survivors keep their ids and so their data).
    #[must_use]
    pub fn shard_for(&self, point_id: &str) -> &Shard {
        // `from_shards` guarantees at least one shard, so `max_by_key` is `Some`; the
        // `unwrap_or` fallback (shard 0) is unreachable but keeps this total and
        // panic-free (the project bans `unwrap`/`expect`).
        self.shards
            .iter()
            .max_by_key(|s| hrw_score(s.id, point_id))
            .unwrap_or(&self.shards[0])
    }

    /// Partition `items` into per-shard groups (preserving input order within each
    /// group), returning each owning [`Shard`] with its group. Only non-empty groups
    /// are returned. `id_of` extracts the point id each item is routed by. Keyed by
    /// shard **id**, so it is correct even when ids are non-contiguous (a gap from a
    /// removed shard).
    #[must_use]
    pub fn partition<'a, T, F>(&'a self, items: &'a [T], id_of: F) -> Vec<(&'a Shard, Vec<&'a T>)>
    where
        F: Fn(&T) -> &str,
    {
        let mut by_id: std::collections::HashMap<u64, Vec<&T>> = std::collections::HashMap::new();
        for item in items {
            by_id
                .entry(self.shard_for(id_of(item)).id)
                .or_default()
                .push(item);
        }
        // Emit in shard order for a stable, deterministic result.
        self.shards
            .iter()
            .filter_map(|s| by_id.remove(&s.id).map(|g| (s, g)))
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
/// a small shard id into the hash key avalanches poorly for short ids (an early
/// low-byte difference barely moves the comparison, skewing the assignment).
fn hrw_score(shard_id: u64, point_id: &str) -> u64 {
    let mut x =
        fnv1a(point_id.as_bytes()).wrapping_add(shard_id.wrapping_mul(0x9e37_79b9_7f4a_7c15));
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
        // from_urls assigns ids 0..N.
        assert_eq!(
            map3().shards().iter().map(|s| s.id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        // A freshly built map has no replicas — every shard is primary-only.
        assert!(map3().shards().iter().all(|s| s.replica_urls.is_empty()));
    }

    #[test]
    fn from_shards_tolerates_gaps_and_rejects_duplicate_ids() {
        let shard = |id: u64, url: &str| Shard {
            id,
            primary_url: url.to_owned(),
            replica_urls: Vec::new(),
        };
        // A gap (id 1 removed) is fine — ids need not be contiguous.
        let m = ShardMap::from_shards(vec![shard(0, "http://a"), shard(2, "http://c")]).unwrap();
        assert_eq!(m.shards().iter().map(|s| s.id).collect::<Vec<_>>(), [0, 2]);
        // Duplicate ids are rejected (the id is the HRW key).
        assert_eq!(
            ShardMap::from_shards(vec![shard(0, "http://a"), shard(0, "http://b")]).unwrap_err(),
            ClusterError::DuplicateShardId(0)
        );
        assert_eq!(
            ShardMap::from_shards(vec![]).unwrap_err(),
            ClusterError::NoShards
        );
        assert_eq!(
            ShardMap::from_shards(vec![shard(5, "")]).unwrap_err(),
            ClusterError::EmptyUrl(5)
        );
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
        // An unknown shard id and an empty URL are rejected.
        assert_eq!(
            m.add_replica(3, "http://x").unwrap_err(),
            ClusterError::UnknownReplicaShard(3)
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
            let a = m.shard_for(id).id;
            let b = m.shard_for(id).id;
            assert_eq!(a, b, "deterministic");
            assert!(a < 3, "in range");
        }
    }

    #[test]
    fn distribution_is_roughly_even() {
        let m = map3();
        let mut counts = [0usize; 3];
        for i in 0..9_000 {
            counts[m.shard_for(&format!("point-{i}")).id as usize] += 1;
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
            counts[m.shard_for(&format!("p{i}")).id as usize] += 1;
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
            let before = m4.shard_for(&id).id;
            let after = m3.shard_for(&id).id;
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
    fn removing_a_middle_shard_moves_only_its_slice() {
        // The dynamic-membership case (ADR-0066): id is the HRW key, decoupled from
        // position, so removing a *middle* shard (id 1) leaves the survivors (ids 0
        // and 2) and their data untouched — only id-1's keys remap, to 0 or 2.
        let full = ShardMap::from_urls(["http://a", "http://b", "http://c"]).unwrap();
        let shard = |id: u64, url: &str| Shard {
            id,
            primary_url: url.to_owned(),
            replica_urls: Vec::new(),
        };
        let gapped =
            ShardMap::from_shards(vec![shard(0, "http://a"), shard(2, "http://c")]).unwrap();
        let mut moved_off_survivor = 0;
        let mut moved_from_removed = 0;
        for i in 0..4_000 {
            let id = format!("k{i}");
            let before = full.shard_for(&id).id;
            let after = gapped.shard_for(&id).id;
            if before == 1 {
                // The removed shard's keys must go to a survivor (0 or 2).
                assert!(after == 0 || after == 2);
                moved_from_removed += 1;
            } else if before != after {
                moved_off_survivor += 1;
            }
        }
        assert_eq!(moved_off_survivor, 0, "a survivor's keys moved");
        assert!(moved_from_removed > 0, "removed-shard keys should remap");
    }

    #[test]
    fn partition_groups_by_owning_shard() {
        let m = map3();
        let ids: Vec<String> = (0..50).map(|i| format!("id{i}")).collect();
        let groups = m.partition(&ids, |s| s.as_str());
        // Every id appears exactly once, in its owning shard's group.
        let total: usize = groups.iter().map(|(_, g)| g.len()).sum();
        assert_eq!(total, ids.len());
        for (shard, group) in &groups {
            assert!(!group.is_empty());
            for id in group {
                assert_eq!(m.shard_for(id).id, shard.id);
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
    fn add_and_remove_shard_bump_the_version() {
        let mut m = map3();
        assert_eq!(m.version(), 0);
        m.add_shard(7, "http://d:6333", vec!["http://d2:6333".into()])
            .unwrap();
        assert_eq!(m.version(), 1);
        assert_eq!(m.len(), 4);
        assert_eq!(m.shards().last().unwrap().id, 7);
        assert_eq!(m.shards().last().unwrap().replica_urls, ["http://d2:6333"]);
        // A duplicate id or empty URL is rejected and does not bump the version.
        assert_eq!(
            m.add_shard(7, "http://x", vec![]).unwrap_err(),
            ClusterError::DuplicateShardId(7)
        );
        assert_eq!(
            m.add_shard(8, "  ", vec![]).unwrap_err(),
            ClusterError::EmptyUrl(8)
        );
        assert_eq!(m.version(), 1);
        // Remove bumps the version and leaves a gap; the id is not reused by the map.
        m.remove_shard(1).unwrap();
        assert_eq!(m.version(), 2);
        assert_eq!(
            m.shards().iter().map(|s| s.id).collect::<Vec<_>>(),
            [0, 2, 7]
        );
        assert_eq!(
            m.remove_shard(1).unwrap_err(),
            ClusterError::UnknownShard(1)
        );
    }

    #[test]
    fn remove_shard_refuses_to_empty_the_map() {
        let mut m = ShardMap::from_urls(["http://only"]).unwrap();
        assert_eq!(m.remove_shard(0).unwrap_err(), ClusterError::NoShards);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn shard_map_round_trips_through_json() {
        let mut m = map3();
        m.add_shard(9, "http://d", vec!["http://d2".into()])
            .unwrap();
        let json = serde_json::to_string(&m).unwrap();
        let back: ShardMap = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version(), m.version());
        assert_eq!(back.shards(), m.shards());
        // A key routes identically through the deserialized map.
        assert_eq!(back.shard_for("user:42").id, m.shard_for("user:42").id);
    }

    #[test]
    fn merge_top_k_handles_fewer_than_k_and_empty() {
        assert!(merge_top_k(vec![Vec::<(&str, f32)>::new()], 5, |t| t.1, false).is_empty());
        let got = merge_top_k(vec![vec![("a", 1.0f32)]], 10, |t| t.1, false);
        assert_eq!(got.len(), 1);
    }
}
