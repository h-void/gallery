//! Native character/artist recognition status (no Python residual).

use std::path::PathBuf;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

pub fn character_recognition_status(conn: &Connection) -> Result<Value> {
    // Character recognition on by default; artist identity still comes from folders.
    let enabled = env_bool("CHARACTER_RECOGNITION_ENABLED", true);
    let model_path = crate::character_ccip::character_model_path();
    let present = model_path.is_file();
    let variant =
        std::env::var("CHARACTER_MODEL_VARIANT").unwrap_or_else(|_| "ccip-caformer_b36-24".into());
    let model_file =
        std::env::var("CHARACTER_MODEL_FILE").unwrap_or_else(|_| "model_feat.onnx".into());
    let (indexed_characters, indexed_references): (i64, i64) = (
        conn.query_row("SELECT COUNT(*) FROM characters", [], |r| r.get(0))
            .unwrap_or(0),
        conn.query_row("SELECT COUNT(*) FROM character_references", [], |r| r.get(0))
            .unwrap_or(0),
    );
    let session = if enabled && present {
        crate::character_ccip::session_status()
    } else {
        json!({"session_loaded": false})
    };
    let session_ok = session
        .get("session_loaded")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let (available, reason) = if !enabled {
        (false, "disabled")
    } else if !present {
        (false, "ccip_model_not_found")
    } else if !session_ok {
        (
            false,
            session
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("session_load_failed"),
        )
    } else {
        (true, "ready")
    };
    let provider = if session_ok {
        crate::character_ccip::active_provider()
    } else {
        "unavailable"
    };
    let active_device = if session_ok {
        crate::character_ccip::active_device()
    } else {
        "unavailable"
    };
    let gpu_access = crate::character_ccip::gpu_access_probe();
    Ok(json!({
        "enabled": enabled,
        "available": available,
        "reason": reason,
        "backend": "onnxruntime-openvino",
        "provider": provider,
        "requested_provider": std::env::var("CHARACTER_RECOGNITION_PROVIDER").unwrap_or_else(|_| "openvino".into()),
        "active_device": active_device,
        "openvino_device": std::env::var("CHARACTER_OPENVINO_DEVICE").unwrap_or_else(|_| "GPU".into()),
        "gpu_access": gpu_access,
        "model_variant": variant,
        "model_file": model_file,
        "model_path": model_path.display().to_string(),
        "model_present": present,
        "indexed_characters": indexed_characters,
        "indexed_references": indexed_references,
        "session": session,
        "runtime": "rust-primary",
        "note": "CCIP via onnxruntime-openvino. gpu_access.ready must be true for real Intel iGPU; otherwise CPU fallback. NAS CPU meters still move because preprocess runs on CPU.",
    }))
}

pub fn artist_recognition_status() -> Value {
    // Artist recognition off by default: folders already partition by artist.
    let enabled = env_bool("ARTIST_RECOGNITION_ENABLED", false);
    let model_dir = std::env::var("ARTIST_MODEL_DIR")
        .or_else(|_| std::env::var("MODEL_CACHE_ROOT").map(|r| format!("{r}/artist")))
        .unwrap_or_else(|_| "data/models/artist".into());
    let dir = PathBuf::from(&model_dir);
    let files = [
        "dino_vits8.onnx",
        "wd14.onnx",
    ];
    let models: Vec<Value> = files
        .iter()
        .map(|file| {
            let path = dir.join(file);
            json!({
                "file": file,
                "path": path.display().to_string(),
                "present": path.is_file(),
            })
        })
        .collect();
    let missing: Vec<_> = models
        .iter()
        .filter(|m| m["present"] == false)
        .filter_map(|m| m["file"].as_str().map(|s| s.to_string()))
        .collect();
    let available = enabled && missing.is_empty();
    json!({
        "enabled": enabled,
        "available": available,
        "reason": if !enabled { "disabled" } else if missing.is_empty() { "ready" } else { "missing_models" },
        "backend": "onnxruntime-rust",
        "model_dir": model_dir,
        "models": models,
        "missing_models": missing,
        "artifacts_ready": missing.is_empty(),
        "runtime": "rust-primary",
    })
}

pub fn character_model_signature() -> Value {
    let variant =
        std::env::var("CHARACTER_MODEL_VARIANT").unwrap_or_else(|_| "ccip-caformer_b36-24".into());
    let model_file =
        std::env::var("CHARACTER_MODEL_FILE").unwrap_or_else(|_| "model_feat.onnx".into());
    json!({
        "model_variant": variant,
        "model_file": model_file,
        // Align with historical Python signature components used for stale detection.
        "model_repo_id": std::env::var("CHARACTER_MODEL_REPO_ID")
            .unwrap_or_else(|_| "deepghs/ccip_onnx".into()),
        "signature": format!("rust:{variant}/{model_file}"),
    })
}

pub fn suggest_artists_native(conn: &Connection, item_id: i64, limit: i64) -> Result<Value> {
    // Pure-Rust metadata fallback: return empty model candidates when embeddings
    // are absent; never call Python.
    let _item_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM items WHERE id=?",
        rusqlite::params![item_id],
        |r| r.get(0),
    )?;
    Ok(json!({
        "item_id": item_id,
        "candidates": [],
        "limit": limit,
        "reason": "embedding_index_empty_or_model_unavailable",
        "backend": "rust-primary",
    }))
}

pub fn recognize_character_native(conn: &Connection, item_id: i64) -> Result<Value> {
    crate::character_ccip::recognize_character_native(conn, item_id, 3)
}

pub fn recognize_character_native_topk(
    conn: &Connection,
    item_id: i64,
    top_k: i64,
) -> Result<Value> {
    crate::character_ccip::recognize_character_native(conn, item_id, top_k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_shapes_without_python() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE characters (id INTEGER PRIMARY KEY);
             CREATE TABLE character_references (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        let st = character_recognition_status(&conn).unwrap();
        assert!(st.get("enabled").is_some());
        // Product backend is onnxruntime-openvino; rust-primary is in runtime.
        assert_eq!(st["backend"], "onnxruntime-openvino");
        assert_eq!(st["runtime"], "rust-primary");
        let a = artist_recognition_status();
        assert_eq!(a["runtime"], "rust-primary");
    }
}
