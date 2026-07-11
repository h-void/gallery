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
        return Err(anyhow!("too many vectors for similarity matrix (max {MAX_CLUSTER_SCORE_VECTORS})"));
    }
    let mut scores = vec![vec![0.0f32; n]; n];
    for i in 0..n {
        let vi = &vectors[i];
        if vi.len() != dim {
            return Err(anyhow!("all vectors must have the same dimension"));
        }
        for j in 0..n {
            scores[i][j] = dot(vi, &vectors[j]);
        }
    }
    Ok(scores)
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    for (a, b) in left.iter().zip(right.iter()) {
        sum += a * b;
    }
    sum
}

/// JSON envelope for the HTTP route.
pub fn cluster_scores_response(vectors: &[Vec<f32>]) -> Result<Value> {
    let scores = similarity_matrix(vectors)?;
    Ok(json!({ "scores": scores }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_matrix_matches_numpy_dot_product() {
        // Orthogonal unit-ish vectors; M @ M.T expects:
        //   row0 = [1, 0, 1]
        //   row1 = [0, 1, 0]
        //   row2 = [1, 0, 1]
        let vectors = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![1.0, 0.0, 0.0],
        ];
        let scores = similarity_matrix(&vectors).unwrap();
        assert_eq!(scores[0], vec![1.0, 0.0, 1.0]);
        assert_eq!(scores[1], vec![0.0, 1.0, 0.0]);
        assert_eq!(scores[2], vec![1.0, 0.0, 1.0]);
    }

    #[test]
    fn similarity_matrix_rejects_ragged_vectors() {
        let vectors = vec![vec![1.0, 0.0], vec![0.0]];
        assert!(similarity_matrix(&vectors).is_err());
    }

    #[test]
    fn similarity_matrix_rejects_oversized_input_before_allocating_matrix() {
        assert!(similarity_matrix(&vec![vec![1.0]; 513]).is_err());
    }
}
