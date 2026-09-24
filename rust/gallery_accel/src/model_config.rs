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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvVar, ENV_LOCK};

    // `runtime_prepare` and `character_ccip` parse the same `CHARACTER_*`
    // variables. Every test here mutates them, so all three modules take the
    // process-wide `ENV_LOCK`: without it a concurrent reader observes another
    // test's value mid-assertion and the failure looks like a logic bug.

    #[test]
    fn test_requested_provider_normalization() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let _provider = EnvVar::remove("CHARACTER_RECOGNITION_PROVIDER");
        for (raw, expected, cuda, openvino) in [
            ("auto", "auto", true, true),
            ("", "auto", true, true),
            ("   ", "auto", true, true),
            ("cuda", "cuda", true, false),
            ("nvidia", "cuda", true, false),
            ("cudaexecutionprovider", "cuda", true, false),
            ("openvino", "openvino", false, true),
            ("intel", "openvino", false, true),
            ("gpu", "openvino", false, true),
            ("openvinoexecutionprovider", "openvino", false, true),
            ("cpu", "cpu", false, false),
            ("cpuexecutionprovider", "cpu", false, false),
        ] {
            std::env::set_var("CHARACTER_RECOGNITION_PROVIDER", raw);
            assert_eq!(requested_provider(), expected, "provider for {raw}");
            assert_eq!(want_cuda(), cuda, "want_cuda for {raw}");
            assert_eq!(want_openvino(), openvino, "want_openvino for {raw}");
        }
    }

    #[test]
    fn test_cpu_fallback_precedence() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let new_key = "CHARACTER_ALLOW_CPU_FALLBACK";
        let old_key = "CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK";
        let _new = EnvVar::remove(new_key);
        let _old = EnvVar::remove(old_key);

        // Neither variable set -> default allows CPU fallback.
        assert!(allow_cpu_fallback());

        std::env::set_var(old_key, "1");
        assert!(allow_cpu_fallback());

        std::env::set_var(old_key, "0");
        assert!(!allow_cpu_fallback());

        std::env::set_var(new_key, "0");
        std::env::set_var(old_key, "1");
        assert!(!allow_cpu_fallback());

        std::env::set_var(new_key, "1");
        assert!(allow_cpu_fallback());
    }

    #[test]
    fn test_model_config_defaults_and_blank_fallback() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let _repo = EnvVar::set("CHARACTER_MODEL_REPO_ID", "  ");
        let _variant = EnvVar::set("CHARACTER_MODEL_VARIANT", "");
        let _file = EnvVar::remove("CHARACTER_MODEL_FILE");

        assert_eq!(character_model_repo_id(), CCIP_REPO_ID);
        assert_eq!(character_model_variant(), CCIP_VARIANT);
        assert_eq!(character_model_file(), CCIP_FILE);
        assert!(is_default_model_config());
    }
}
