use std::cmp::Ordering;
use std::collections::HashMap;

use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Serialize, Debug)]
struct ArtistReferenceScore {
    artist_id: i64,
    artist_name: String,
    dino_score: f64,
    wd14_score: f64,
    fused_score: f64,
    matched_ref_id: i64,
    matched_ref_item_id: Option<i64>,
}

#[derive(Clone, Debug)]
struct ArtistReferenceEmbeddingRow {
    reference_id: i64,
    artist_id: i64,
    artist_name: String,
    reference_item_id: Option<i64>,
    dino_embedding: Vec<u8>,
    dino_embedding_dim: i64,
    wd14_embedding: Vec<u8>,
    wd14_embedding_dim: i64,
}

pub fn artist_reference_scores_response(
    conn: &Connection,
    query_dino: &[f32],
    query_wd14: &[f32],
    dino_weight: f64,
    wd14_weight: f64,
    limit: Option<i64>,
) -> Result<Value> {
    let mut best_by_artist: HashMap<i64, ArtistReferenceScore> = HashMap::new();
    let mut stmt = conn.prepare(
        "
        SELECT ar.id AS reference_id, ar.artist_id, a.name AS artist_name,
               ar.item_id AS reference_item_id,
               ar.dino_embedding, ar.dino_embedding_dim,
               ar.wd14_embedding, ar.wd14_embedding_dim
        FROM artist_references ar
        JOIN artists a ON a.id = ar.artist_id
        WHERE ar.dino_embedding IS NOT NULL
          AND ar.wd14_embedding IS NOT NULL
        ORDER BY a.name COLLATE NOCASE, ar.id
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ArtistReferenceEmbeddingRow {
            reference_id: row.get("reference_id")?,
            artist_id: row.get("artist_id")?,
            artist_name: row.get("artist_name")?,
            reference_item_id: row.get("reference_item_id")?,
            dino_embedding: row.get("dino_embedding")?,
            dino_embedding_dim: row.get("dino_embedding_dim")?,
            wd14_embedding: row.get("wd14_embedding")?,
            wd14_embedding_dim: row.get("wd14_embedding_dim")?,
        })
    })?;

    for row in rows {
        let row = row?;
        if row.dino_embedding_dim as usize != query_dino.len()
            || row.wd14_embedding_dim as usize != query_wd14.len()
        {
            continue;
        }
        let ref_dino = f32_blob_to_vec(&row.dino_embedding)?;
        let ref_wd14 = f32_blob_to_vec(&row.wd14_embedding)?;
        if ref_dino.len() != query_dino.len() || ref_wd14.len() != query_wd14.len() {
            continue;
        }
        let dino_score = dot_f32(query_dino, &ref_dino) as f64;
        let wd14_score = dot_f32(query_wd14, &ref_wd14) as f64;
        let fused_score = (dino_weight * dino_score) + (wd14_weight * wd14_score);
        let score = ArtistReferenceScore {
            artist_id: row.artist_id,
            artist_name: row.artist_name,
            dino_score,
            wd14_score,
            fused_score,
            matched_ref_id: row.reference_id,
            matched_ref_item_id: row.reference_item_id,
        };
        match best_by_artist.get(&score.artist_id) {
            Some(current) if current.fused_score >= score.fused_score => {}
            _ => {
                best_by_artist.insert(score.artist_id, score);
            }
        }
    }

    let mut scores: Vec<_> = best_by_artist.into_values().collect();
    scores.sort_by(|left, right| {
        right
            .fused_score
            .partial_cmp(&left.fused_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                left.artist_name
                    .to_lowercase()
                    .cmp(&right.artist_name.to_lowercase())
            })
    });
    if let Some(limit) = limit {
        if limit >= 0 {
            scores.truncate(limit as usize);
        }
    }
    Ok(json!({ "scores": scores }))
}

fn f32_blob_to_vec(blob: &[u8]) -> Result<Vec<f32>> {
    if blob.len() % 4 != 0 {
        return Err(anyhow!("invalid f32 blob length: {}", blob.len()));
    }
    Ok(blob
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn dot_f32(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right.iter())
        .map(|(left, right)| left * right)
        .sum()
}
