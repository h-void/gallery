use anyhow::{anyhow, Result};
use serde_json::{json, Value};

pub const MAX_CLUSTER_SCORE_VECTORS: usize = 512;

/// Compute the pairwise dot-product matrix `M @ M.T` for a list of equal-length
/// vectors. Embeddings passed here are L2-normalized, so the dot product equals
/// cosine similarity; this mirrors `numpy.vstack(vectors) @ matrix.T` exactly
/// (plain dot products, no re-normalization).
pub fn similarity_matrix(vectors: &[Vec<f32>]) -> Result<Vec<Vec<f32>>> {
    if vectors.is_empty() {
        return Ok(Vec::new());
    }
    let dim = vectors[0].len();
    if dim == 0 {
        return Err(anyhow!("vectors must be non-empty"));
    }
    let n = vectors.len();
    if n > MAX_CLUSTER_SCORE_VECTORS {
        return Err(anyhow!(
            "too many vectors for similarity matrix (max {MAX_CLUSTER_SCORE_VECTORS})"
        ));
    }
    for vector in vectors {
        if vector.len() != dim {
            return Err(anyhow!("all vectors must have the same dimension"));
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(anyhow!("vectors must contain only finite values"));
        }
    }
    let mut scores = vec![vec![0.0f32; n]; n];
    for i in 0..n {
        let vi = &vectors[i];
        // The matrix is symmetric: compute each pair once and mirror it.
        scores[i][i] = dot(vi, vi);
        for j in (i + 1)..n {
            let value = dot(vi, &vectors[j]);
            scores[i][j] = value;
            scores[j][i] = value;
        }
    }
    Ok(scores)
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

/// JSON envelope for the HTTP route.
pub fn cluster_scores_response(vectors: &[Vec<f32>]) -> Result<Value> {
    let scores = similarity_matrix(vectors)?;
    Ok(json!({ "scores": scores }))
}
