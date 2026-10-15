//! Unified model and inference runtime configuration.
//!
//! Shared across `runtime_prepare`, `character_ccip`, and `recognition_status`
//! so environment parsing, fallback semantics, and model paths never fork.

use std::path::PathBuf;

pub const CCIP_REPO_ID: &str = "deepghs/ccip_onnx";
pub const CCIP_VARIANT: &str = "ccip-caformer_b36-24";
pub const CCIP_FILE: &str = "model_feat.onnx";

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

/// Raw requested provider as provided in the environment (or "auto" if unset or empty).
pub fn requested_provider_raw() -> String {
    std::env::var("CHARACTER_RECOGNITION_PROVIDER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "auto".to_string())
}

/// Normalized requested provider: `auto | cuda | openvino | cpu | <custom>`
pub fn requested_provider() -> String {
    let raw = requested_provider_raw();
    match raw.to_ascii_lowercase().as_str() {
        "" | "auto" => "auto".to_string(),
        "cuda" | "nvidia" | "cudaexecutionprovider" => "cuda".to_string(),
        // `gpu` is the historical alias for OpenVINO GPU (never CUDA).
        "openvino" | "intel" | "gpu" | "openvinoexecutionprovider" => "openvino".to_string(),
        "cpu" | "cpuexecutionprovider" => "cpu".to_string(),
        other => other.to_string(),
    }
}

pub fn want_cuda() -> bool {
    matches!(requested_provider().as_str(), "auto" | "cuda")
}

pub fn want_openvino() -> bool {
    matches!(requested_provider().as_str(), "auto" | "openvino")
}

pub fn force_cpu_only() -> bool {
    requested_provider() == "cpu"
}

/// Preferred fallback toggle: `CHARACTER_ALLOW_CPU_FALLBACK`.
/// Backward compat: `CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK` when new var is unset.
/// When neither is set, defaults to true.
pub fn allow_cpu_fallback() -> bool {
    if std::env::var("CHARACTER_ALLOW_CPU_FALLBACK").is_ok() {
        env_bool("CHARACTER_ALLOW_CPU_FALLBACK", false)
    } else if std::env::var("CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK").is_ok() {
        env_bool("CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK", false)
    } else {
        true
    }
}

pub fn character_model_repo_id() -> String {
    std::env::var("CHARACTER_MODEL_REPO_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| CCIP_REPO_ID.to_string())
}

pub fn character_model_variant() -> String {
    std::env::var("CHARACTER_MODEL_VARIANT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| CCIP_VARIANT.to_string())
}

pub fn character_model_file() -> String {
    std::env::var("CHARACTER_MODEL_FILE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| CCIP_FILE.to_string())
}

/// Auto-download only applies to the default pinned model. Custom
/// `CHARACTER_MODEL_REPO_ID` / `_VARIANT` / `_FILE` values are marked
/// `custom_model_unmanaged` and must be placed manually.
pub fn is_default_model_config() -> bool {
    character_model_repo_id() == CCIP_REPO_ID
        && character_model_variant() == CCIP_VARIANT
        && character_model_file() == CCIP_FILE
}

pub fn character_model_dir() -> PathBuf {
    std::env::var("CHARACTER_MODEL_DIR")
        .or_else(|_| std::env::var("MODEL_CACHE_ROOT").map(|r| format!("{r}/character")))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data/models/character"))
}

pub fn character_model_path() -> PathBuf {
    character_model_dir()
        .join(character_model_variant())
        .join(character_model_file())
}

pub fn openvino_device_type() -> String {
    std::env::var("CHARACTER_OPENVINO_DEVICE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "GPU".into())
}

pub fn openvino_cache_dir() -> Option<String> {
    std::env::var("CHARACTER_OPENVINO_CACHE_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
