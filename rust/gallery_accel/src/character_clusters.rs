//! Unsupervised character clustering for the autonomous organization pipeline
//! (docs/RFC_AUTONOMOUS_ORGANIZATION_PIPELINE.md v2, milestone 1).
//!
//! Pure vector math: callers pass L2-normalized f32 feature slices (CCIP 768-dim
//! embeddings are normalized at extraction in `character_ccip::l2_normalize`);
//! this module performs no DB, filesystem, or model I/O so it is unit-testable
//! on synthetic vectors.
//!
//! Threshold provenance (RFC chapter 6 — do not adjust without re-reading it):
//! the official `ccip-caformer_b36-24` metric head computes
//! `diff = clip((1 - cos) / 2, 0, 1)`, so the official difference scale is half
//! cosine distance. Official metrics/cluster data converts to cosine similarity
//! as: same-character pair threshold 0.21323 → 0.5735, DBSCAN eps 0.14132 →
//! 0.7174, OPTICS eps 0.21196 → 0.5761.

/// Default complete-linkage merge gate as a *similarity* floor: every pair of
/// members inside one cluster must reach this cosine similarity, i.e. the
/// cluster diameter stays at or below `1 - 0.7174 = 0.2826`. Converted from the
/// official DBSCAN eps 0.14132 on the diff = (1 - cos) / 2 scale. Using the
/// eps value as a *diameter* bound is stricter than DBSCAN's neighborhood
/// chaining on purpose (over-clustering: 宁碎勿合). Empirically adjustable in
/// the 0.5761..=0.7174 window per RFC chapter 6.5.
pub const DEFAULT_MIN_SIMILARITY: f32 = 0.7174;

/// Stage-1 silent pre-filter acceptance floor. Same official provenance as
/// [`DEFAULT_MIN_SIMILARITY`]; kept as a separate constant because the two
/// roles (pre-filter acceptance vs cluster diameter) recalibrate
/// independently. Silent tagging itself stays behind an explicit enable flag
/// at the endpoint layer and runs the real-library distribution scan first
/// (RFC chapter 6.5).
pub const PREFILTER_MIN_SIMILARITY: f32 = 0.7174;

/// Stage-1 pre-filter runner-up margin, aligned with the production
/// `CHARACTER_RECOGNITION_MIN_GAP` default (0.04 in `character_ccip`).
pub const PREFILTER_MIN_GAP: f32 = 0.04;

/// Near-duplicate fold line. Deliberately identical to the production
/// `CHARACTER_IMPORT_DUPLICATE_SIMILARITY` default (0.95 in
/// `character_cleanup`) so fold groups and cleanup near-duplicates agree.
pub const FOLD_SIMILARITY: f32 = 0.95;

/// Minimum medoid-to-diverse cosine *distance*. Guarantees the two enrolled
/// prototypes sit strictly below the [`FOLD_SIMILARITY`] line, so
/// `character_cleanup` can never classify the pair as near-duplicates and
/// delete the flywheel's second prototype (RFC chapter 6.3).
pub const DIVERSE_MIN_DISTANCE: f32 = 0.05;

/// One discovered cluster, expressed as indices into the caller's input slice.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterCluster {
    /// All input indices belonging to this cluster (fold members included).
    pub members: Vec<usize>,
    /// Input index of the medoid (minimizer of the summed squared cosine
    /// distance to every other member).
    pub medoid: usize,
    /// Input index of the diversity sample: the member farthest from the
    /// medoid among those farther than [`DIVERSE_MIN_DISTANCE`]. `None` when
    /// the whole cluster folds into one near-duplicate group.
    pub diverse: Option<usize>,
    /// Mean pairwise cosine similarity over all original member pairs.
    pub mean_similarity: f32,
    /// True when the cluster consists of exactly one near-duplicate fold
    /// group (every member is within the fold line of every other member).
    pub is_all_duplicates: bool,
}

/// Clustering outcome for one batch of unorganized vectors.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterResult {
    /// Complete-linkage clusters with at least two original members, sorted
    /// by descending size (ties: ascending first member index).
    pub clusters: Vec<CharacterCluster>,
    /// Input indices that ended in no cluster (single-image groups below the
    /// merge gate — e.g. diluted multi-character shots per RFC chapter 3.3).
    pub noise: Vec<usize>,
    /// Multi-member near-duplicate fold groups (each group collapses to one
    /// representative before clustering), sorted by first member index.
    pub fold_groups: Vec<Vec<usize>>,
}

/// Cosine similarity of two L2-normalized vectors (their dot product).
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    1.0 - dot(a, b)
}

/// Union-find over `0..n` with path compression.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            // Attach the larger root under the smaller one so components are
            // identified by their smallest member index.
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent[hi] = lo;
        }
    }
}

/// Connected components of the near-duplicate relation (pairwise similarity
/// at or above `fold_similarity`). Singletons come back as their own
/// one-element component so every input index maps to exactly one
/// representative. Components are keyed by their smallest member and returned
/// sorted by that index.
fn fold_components(vectors: &[Vec<f32>], fold_similarity: f32) -> Vec<Vec<usize>> {
    let n = vectors.len();
    let mut uf = UnionFind::new(n);
    for i in 0..n {
        for j in (i + 1)..n {
            if dot(&vectors[i], &vectors[j]) >= fold_similarity {
                uf.union(i, j);
            }
        }
    }
    let mut components: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        components[uf.find(i)].push(i);
    }
    components.retain(|component| !component.is_empty());
    components
}

/// Medoid of `indices`: the member minimizing the sum of squared cosine
/// distances to all other members (RFC chapter 3.2). Ties keep the lowest
/// index so the result is deterministic.
fn medoid_of(vectors: &[Vec<f32>], indices: &[usize]) -> usize {
    let mut best = indices[0];
    let mut best_score = f32::INFINITY;
    for &i in indices {
        let mut score = 0.0f32;
        for &j in indices {
            if i != j {
                let d = cosine_distance(&vectors[i], &vectors[j]);
                score += d * d;
            }
        }
        if score < best_score {
            best_score = score;
            best = i;
        }
    }
    best
}

/// Complete-linkage agglomeration over `reps` (indices into `vectors`):
/// repeatedly merges the two clusters with the smallest *maximum* cross
/// distance (Lance-Williams max update) while that diameter stays at or below
/// `diameter_threshold`. Returns one cluster (a `Vec` of *positions* into
/// `reps`) per final group, singletons included. The lowest pair index wins
/// distance ties, so the outcome is deterministic.
fn complete_linkage(
    vectors: &[Vec<f32>],
    reps: &[usize],
    diameter_threshold: f32,
) -> Vec<Vec<usize>> {
    let r = reps.len();
    if r == 0 {
        return Vec::new();
    }
    // Cluster-to-cluster distances start as rep-to-rep distances and grow by
    // the max update on merges.
    let mut dist = vec![vec![f32::INFINITY; r]; r];
    for i in 0..r {
        for j in (i + 1)..r {
            let d = cosine_distance(&vectors[reps[i]], &vectors[reps[j]]);
            dist[i][j] = d;
            dist[j][i] = d;
        }
    }
    let mut active = vec![true; r];
    let mut members: Vec<Vec<usize>> = (0..r).map(|i| vec![i]).collect();
    loop {
        let mut best = (f32::INFINITY, usize::MAX, usize::MAX);
        for i in 0..r {
            if !active[i] {
                continue;
            }
            for j in (i + 1)..r {
                if active[j] && dist[i][j] < best.0 {
                    best = (dist[i][j], i, j);
                }
            }
        }
        let (best_dist, i, j) = best;
        if i == usize::MAX || best_dist > diameter_threshold {
            break;
        }
        for k in 0..r {
            if active[k] && k != i && k != j {
                let merged = dist[i][k].max(dist[j][k]);
                dist[i][k] = merged;
                dist[k][i] = merged;
            }
        }
        let drained = std::mem::take(&mut members[j]);
        members[i].extend(drained);
        active[j] = false;
    }
    (0..r)
        .filter(|i| active[*i])
        .map(|i| std::mem::take(&mut members[i]))
        .collect()
}

/// Runs the RFC chapter 2 stage-2/3 pipeline over one batch of L2-normalized
/// feature vectors: near-duplicate folding, complete-linkage clustering at
/// `min_similarity` (pass [`DEFAULT_MIN_SIMILARITY`] unless calibrated
/// otherwise), and medoid/diverse representative selection. Clusters with a
/// single original member come back as `noise` (RFC chapter 3.3: isolated
/// diluted shots must not become one-person clusters).
///
/// Fails when input vectors disagree in dimension or are not L2-normalized;
/// either silently corrupts every downstream similarity.
pub fn cluster_unorganized(
    vectors: &[Vec<f32>],
    min_similarity: f32,
) -> anyhow::Result<ClusterResult> {
    if let Some(first) = vectors.first() {
        let dim = first.len();
        for v in vectors {
            if v.len() != dim {
                anyhow::bail!("inconsistent embedding dimensions: {dim} vs {}", v.len());
            }
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if (norm - 1.0).abs() > 1e-3 {
                anyhow::bail!("embedding is not L2-normalized (norm = {norm})");
            }
        }
    }

    let components = fold_components(vectors, FOLD_SIMILARITY);
    let fold_groups: Vec<Vec<usize>> = components
        .iter()
        .filter(|component| component.len() > 1)
        .cloned()
        .collect();

    // One representative per fold component — its medoid — so near-duplicate
    // packs cannot weight the centroids. Complete-linkage runs over
    // representatives; the position it returns maps back through `reps`.
    let reps: Vec<usize> = components.iter().map(|g| medoid_of(vectors, g)).collect();
    let rep_clusters = complete_linkage(vectors, &reps, 1.0 - min_similarity);

    let mut clusters = Vec::new();
    let mut noise = Vec::new();
    for rep_cluster in &rep_clusters {
        // `reps` is built position-aligned over the compacted `components`, so
        // a rep position doubles as the component position.
        let mut members: Vec<usize> = rep_cluster
            .iter()
            .flat_map(|pos| components[*pos].iter().copied())
            .collect();
        members.sort_unstable();
        if members.len() < 2 {
            noise.extend(members);
            continue;
        }
        // Exactly one fold component covering the whole cluster ⇔ every pair
        // of members is within the fold line. Two separate components can
        // never qualify: any cross pair below the fold line would have merged
        // them into one component already.
        let is_all_duplicates = rep_cluster.len() == 1 && components[rep_cluster[0]].len() > 1;
        let medoid = medoid_of(vectors, &members);
        // Members are ascending, and the strict `>` keeps the first (lowest
        // index) maximum, so ties resolve deterministically.
        let mut diverse: Option<usize> = None;
        let mut best_distance = f32::NEG_INFINITY;
        for &k in &members {
            if k == medoid {
                continue;
            }
            let d = cosine_distance(&vectors[medoid], &vectors[k]);
            if d > DIVERSE_MIN_DISTANCE && d > best_distance {
                best_distance = d;
                diverse = Some(k);
            }
        }
        let mut pair_count = 0usize;
        let mut sim_sum = 0.0f32;
        for (pos, &a) in members.iter().enumerate() {
            for &b in &members[pos + 1..] {
                sim_sum += dot(&vectors[a], &vectors[b]);
                pair_count += 1;
            }
        }
        let mean_similarity = if pair_count > 0 {
            sim_sum / pair_count as f32
        } else {
            0.0
        };
        clusters.push(CharacterCluster {
            members,
            medoid,
            diverse,
            mean_similarity,
            is_all_duplicates,
        });
    }
    clusters.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.members[0].cmp(&b.members[0]))
    });
    noise.sort_unstable();

    Ok(ClusterResult {
        clusters,
        noise,
        fold_groups,
    })
}

/// Stage-1 silent pre-filter acceptance test (RFC chapter 2, stage 1): the
/// best reference match must clear [`PREFILTER_MIN_SIMILARITY`] and beat the
/// runner-up character by at least [`PREFILTER_MIN_GAP`]. A missing runner-up
/// (single-character library) leaves the gap check vacuously satisfied.
pub fn prefilter_hit(top1: f32, second: Option<f32>) -> bool {
    if top1 < PREFILTER_MIN_SIMILARITY {
        return false;
    }
    match second {
        Some(second) => top1 - second >= PREFILTER_MIN_GAP,
        None => true,
    }
}
