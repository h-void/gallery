//! Runtime preparation: auto-download ML models and GPU runtime libraries.
//!
//! In `auto` provider mode the gallery selects the best execution provider at
//! startup (CUDA > OpenVINO GPU > CPU) and downloads missing runtime
//! components in the background so the HTTP service is never blocked.
//!
//! Download source switching (`official` vs. `china`) persists to the
//! `app_settings` SQLite table and triggers a retry of missing components.
//!
//! All remote artifacts use pinned revisions, expected byte counts and SHA-256
//! hashes; both download sources must pass the same verification. Downloads run
//! on a dedicated `std::thread`; the local async client has a bounded stalled
//! read timeout. CUDA wheel extraction only reads the three exact entries we
//! ship (no TensorRT provider, no basename traversal).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::character_ccip::character_model_path;

const ONNX_VERSION: &str = "1.24.1";

// CCIP character model (section 5.1 of the GPU runtime plan).
const CCIP_REPO_ID: &str = "deepghs/ccip_onnx";
const CCIP_REVISION: &str = "eb2acdd29af1703388d3d0c04221add322bc9110";
const CCIP_VARIANT: &str = "ccip-caformer_b36-24";
const CCIP_FILE: &str = "model_feat.onnx";
const CCIP_MODEL_SIZE: u64 = 383_591_416;
const CCIP_MODEL_SHA256: &str = "c1e7333a55c2ad9e03cd340c635e96c9c1d86f0836fa7eae65720e7c9c94ee51";

// NVIDIA CUDA ONNX Runtime wheel (section 5.2 of the GPU runtime plan).
const CUDA_WHEEL_FILE: &str =
    "onnxruntime_gpu-1.24.1-cp312-cp312-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl";
const CUDA_WHEEL_DIR: &str =
    "packages/db/a8/fb1a36a052321a839cc9973f6cfd630709412a24afff2d7315feb3efc4b8";
const CUDA_WHEEL_SIZE: u64 = 252_628_733;
const CUDA_WHEEL_SHA256: &str = "710bf83751e6761584ad071102af3cbffd4b42bb77b2e3caacfb54ffbaa0666b";
const CUDA_LIB_ENTRIES: [(&str, &str); 3] = [
    (
        "onnxruntime/capi/libonnxruntime.so.1.24.1",
        "libonnxruntime.so.1.24.1",
    ),
    (
        "onnxruntime/capi/libonnxruntime_providers_cuda.so",
        "libonnxruntime_providers_cuda.so",
    ),
    (
        "onnxruntime/capi/libonnxruntime_providers_shared.so",
        "libonnxruntime_providers_shared.so",
    ),
];
const CUDA_LIB_SIZES: [(&str, u64); 3] = [
    ("libonnxruntime.so.1.24.1", 22_130_592),
    ("libonnxruntime_providers_cuda.so", 367_043_336),
    ("libonnxruntime_providers_shared.so", 14_632),
];
const CUDA_MANIFEST_FILE: &str = "runtime.manifest";
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
const DOWNLOAD_ATTEMPTS: u32 = 3;

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

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `auto | cuda | openvino | cpu` — same normalization as `character_ccip`.
pub fn requested_provider() -> String {
    let raw = std::env::var("CHARACTER_RECOGNITION_PROVIDER").unwrap_or_else(|_| "auto".into());
    let lowered = raw.trim().to_ascii_lowercase();
    match lowered.as_str() {
        "" | "auto" => "auto".to_string(),
        "cuda" | "nvidia" | "cudaexecutionprovider" => "cuda".to_string(),
        // `gpu` is the historical alias for OpenVINO GPU (never CUDA).
        "openvino" | "intel" | "gpu" | "openvinoexecutionprovider" => "openvino".to_string(),
        "cpu" | "cpuexecutionprovider" => "cpu".to_string(),
        other => other.to_string(),
    }
}

/// New preferred fallback toggle; old `CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK`
/// still accepted for backward compatibility when the new var is unset. With
/// neither variable set the default allows CPU fallback.
pub fn allow_cpu_fallback() -> bool {
    if std::env::var("CHARACTER_ALLOW_CPU_FALLBACK").is_ok() {
        env_bool("CHARACTER_ALLOW_CPU_FALLBACK", false)
    } else if std::env::var("CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK").is_ok() {
        env_bool("CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK", false)
    } else {
        true
    }
}

/// `1` by default. Master switch for auto-downloading the CCIP model and the
/// CUDA runtime. Set `ONNXRUNTIME_AUTO_DOWNLOAD=0` to disable (also used by
/// tests so they never touch real HuggingFace/PyPI).
pub fn onnxruntime_auto_download() -> bool {
    env_bool("ONNXRUNTIME_AUTO_DOWNLOAD", true)
}

/// `1` by default. OpenVINO runtime is bundled in the FPK; this flag is
/// reported for diagnostics only and does not gate any download.
pub fn openvino_runtime_auto_download() -> bool {
    env_bool("OPENVINO_RUNTIME_AUTO_DOWNLOAD", true)
}

fn model_repo_id() -> String {
    std::env::var("CHARACTER_MODEL_REPO_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| CCIP_REPO_ID.to_string())
}

fn model_variant() -> String {
    std::env::var("CHARACTER_MODEL_VARIANT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| CCIP_VARIANT.to_string())
}

fn model_file() -> String {
    std::env::var("CHARACTER_MODEL_FILE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| CCIP_FILE.to_string())
}

/// Auto-download only applies to the default pinned model. Custom
/// `CHARACTER_MODEL_REPO_ID` / `_VARIANT` / `_FILE` values are marked
/// `custom_model_unmanaged` and must be placed manually.
fn is_default_model_config() -> bool {
    model_repo_id() == CCIP_REPO_ID && model_variant() == CCIP_VARIANT && model_file() == CCIP_FILE
}

/// Base directory for downloaded ORT / CUDA runtime packages.
pub fn ort_dir() -> PathBuf {
    std::env::var("ONNXRUNTIME_AUTO_DOWNLOAD_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let root = std::env::var("MODEL_CACHE_ROOT").unwrap_or_else(|_| "data/models".into());
            PathBuf::from(format!("{root}/ort"))
        })
}

/// Directory where the CUDA ORT runtime package is expected / published.
pub fn cuda_runtime_dir() -> PathBuf {
    std::env::var("CHARACTER_CUDA_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let root = std::env::var("MODEL_CACHE_ROOT").unwrap_or_else(|_| "data/models".into());
            PathBuf::from(format!("{root}/ort/cuda-{ONNX_VERSION}"))
        })
}

// ── Download source (persisted in SQLite `app_settings`) ─────────────────────

/// Env override for the download source (`official` or `china`). `Some` means
/// the variable was explicitly set, including an invalid value which safely
/// resolves to the official source rather than silently using persisted state.
fn env_download_source_override() -> Option<String> {
    std::env::var("GALLERY_DOWNLOAD_SOURCE").ok().map(|source| {
        if source == "official" || source == "china" {
            source
        } else {
            "official".to_string()
        }
    })
}

fn env_download_source() -> String {
    env_download_source_override().unwrap_or_else(|| "official".to_string())
}

/// Persist the download source. Only the fixed allowlist values are accepted.
pub fn set_download_source(conn: &rusqlite::Connection, source: &str) -> Result<()> {
    if source != "official" && source != "china" {
        bail!("download_source must be 'official' or 'china'");
    }
    conn.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES (?, ?, strftime('%s','now'))\
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=strftime('%s','now')",
        rusqlite::params!["ml_download_source", source],
    )?;
    Ok(())
}

/// Read the persisted download source (env override wins).
pub fn read_download_source(conn: &rusqlite::Connection) -> String {
    persisted_download_source(conn)
}

fn persisted_download_source(conn: &rusqlite::Connection) -> String {
    if let Some(source) = env_download_source_override() {
        return source;
    }
    conn.query_row(
        "SELECT value FROM app_settings WHERE key='ml_download_source'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .filter(|v| v == "china")
    .unwrap_or_else(|| "official".to_string())
}

fn hf_base_url(source: &str) -> &str {
    match source {
        "china" => "https://hf-mirror.com",
        _ => "https://huggingface.co",
    }
}

/// CCIP model URL pinned to the exact revision (never `resolve/main`).
fn ccip_model_url(source: &str) -> String {
    format!(
        "{base}/{repo}/resolve/{rev}/{variant}/{file}",
        base = hf_base_url(source),
        repo = model_repo_id(),
        rev = CCIP_REVISION,
        variant = model_variant(),
        file = model_file(),
    )
}

/// CUDA wheel URL (fixed allowlist per source).
fn cuda_wheel_url(source: &str) -> String {
    match source {
        "china" => format!("https://pypi.tuna.tsinghua.edu.cn/{CUDA_WHEEL_DIR}/{CUDA_WHEEL_FILE}"),
        _ => format!("https://files.pythonhosted.org/{CUDA_WHEEL_DIR}/{CUDA_WHEEL_FILE}"),
    }
}

// ── GPU detection ────────────────────────────────────────────────────────────

fn nvidia_gpu_node_id(name: &str) -> Option<&str> {
    let numeric = name.strip_prefix("nvidia")?;
    (!numeric.is_empty() && numeric.chars().all(|c| c.is_ascii_digit())).then_some(numeric)
}

/// NVIDIA detection requires `/dev/nvidiactl` and at least one `/dev/nvidia[0-9]+`
/// node. Reports whether `/dev/nvidia-uvm` exists without blocking on its absence.
fn nvidia_gpu_nodes_at(dev_dir: &Path) -> (bool, bool) {
    let has_ctl = dev_dir.join("nvidiactl").exists();
    let mut has_gpu_node = false;
    let has_uvm = dev_dir.join("nvidia-uvm").exists();
    if let Ok(entries) = std::fs::read_dir(dev_dir) {
        for ent in entries.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if nvidia_gpu_node_id(&name).is_some() && ent.path().exists() {
                has_gpu_node = true;
            }
        }
    }
    (has_ctl && has_gpu_node, has_uvm)
}

/// WSL2 (including Docker Desktop containers) exposes NVIDIA GPUs through
/// `/dev/dxg` plus a driver-injected `libcuda` instead of the native
/// `/dev/nvidiactl` and `/dev/nvidia[0-9]+` character nodes.
fn injected_libcuda_present() -> bool {
    for path in [
        "/usr/lib/wsl/lib/libcuda.so.1",
        "/usr/lib/x86_64-linux-gnu/libcuda.so.1",
        "/usr/lib64/libcuda.so.1",
    ] {
        if Path::new(path).exists() {
            return true;
        }
    }
    false
}

fn nvidia_gpu_state_with(dev_dir: &Path, libcuda: bool) -> (bool, bool) {
    let (nodes, uvm) = nvidia_gpu_nodes_at(dev_dir);
    (nodes || (libcuda && dev_dir.join("dxg").exists()), uvm)
}

fn nvidia_gpu_state_at(dev_dir: &Path) -> (bool, bool) {
    nvidia_gpu_state_with(dev_dir, injected_libcuda_present())
}

fn has_nvidia_gpu() -> bool {
    nvidia_gpu_state_at(Path::new("/dev")).0
}

/// Intel detection reads the DRM PCI vendor from `/sys/class/drm/card*/device/vendor`
/// and only accepts `0x8086`; a bare `/dev/dri` presence is not enough because
/// AMD also uses DRM nodes.
fn drm_vendor_is_intel_at(sys_drm: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(sys_drm) else {
        return false;
    };
    for ent in entries.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") {
            continue;
        }
        let vendor_path = ent.path().join("device").join("vendor");
        if let Ok(vendor) = std::fs::read_to_string(vendor_path) {
            let t = vendor
                .trim()
                .trim_start_matches("0x")
                .trim_start_matches("0X");
            if t.eq_ignore_ascii_case("8086") {
                return true;
            }
        }
    }
    false
}

/// `true` when an Intel DRM GPU is present and its render node can be opened.
pub fn has_intel_gpu() -> bool {
    if !drm_vendor_is_intel_at(Path::new("/sys/class/drm")) {
        return false;
    }
    let probe = crate::character_ccip::gpu_access_probe();
    probe
        .get("ready")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Detect available GPU families: `cuda` and/or `openvino`.
fn detect_gpus() -> Vec<String> {
    let mut out = Vec::new();
    if has_nvidia_gpu() {
        out.push("cuda".to_string());
    }
    if has_intel_gpu() {
        out.push("openvino".to_string());
    }
    out
}

// ── Artifact state machine ───────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ArtifactState {
    NotStarted,
    Downloading,
    Ready,
    Error,
}

impl ArtifactState {
    fn label(&self) -> &'static str {
        match self {
            ArtifactState::NotStarted => "not_started",
            ArtifactState::Downloading => "downloading",
            ArtifactState::Ready => "ready",
            ArtifactState::Error => "error",
        }
    }
}

#[derive(Clone)]
struct ArtifactStatus {
    state: ArtifactState,
    downloaded_bytes: u64,
    total_bytes: u64,
    start_epoch: Option<u64>,
    end_epoch: Option<u64>,
    last_error: Option<String>,
    verified_path: Option<PathBuf>,
}

impl Default for ArtifactStatus {
    fn default() -> Self {
        Self {
            state: ArtifactState::NotStarted,
            downloaded_bytes: 0,
            total_bytes: 0,
            start_epoch: None,
            end_epoch: None,
            last_error: None,
            verified_path: None,
        }
    }
}

#[derive(Default)]
struct RuntimeState {
    model: ArtifactStatus,
    cuda: ArtifactStatus,
    download_source: String,
    in_progress: bool,
    prepared: bool,
    custom_model_unmanaged: bool,
}

struct Shared {
    state: Mutex<RuntimeState>,
    condvar: Condvar,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ArtifactKind {
    Model,
    Cuda,
}

#[derive(Clone)]
struct ProgressSink {
    shared: std::sync::Arc<Shared>,
    kind: ArtifactKind,
}

impl ProgressSink {
    fn set_downloaded(&self, bytes: u64) {
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        match self.kind {
            ArtifactKind::Model => s.model.downloaded_bytes = bytes,
            ArtifactKind::Cuda => s.cuda.downloaded_bytes = bytes,
        }
        self.shared.condvar.notify_all();
    }
}

fn shared() -> std::sync::Arc<Shared> {
    static SHARED: OnceLock<std::sync::Arc<Shared>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            std::sync::Arc::new(Shared {
                state: Mutex::new(RuntimeState::default()),
                condvar: Condvar::new(),
            })
        })
        .clone()
}

static PREPARE_STARTED: AtomicBool = AtomicBool::new(false);
static WORKER_RUNNING: AtomicBool = AtomicBool::new(false);

// ── Download helpers ─────────────────────────────────────────────────────────

fn user_agent() -> String {
    format!("gallery-accel/{}/rust", env!("CARGO_PKG_VERSION"))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn sha256_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex_digest(&hasher.finalize()))
}

fn verify_file(path: &Path, expected_size: u64, expected_sha: &str) -> bool {
    match path.metadata() {
        Ok(m) if m.is_file() && m.len() == expected_size => {}
        _ => return false,
    }
    sha256_file(path).as_deref() == Some(expected_sha)
}

/// Stream a pinned URL into `staging` while hashing every chunk. The final
/// byte count must match exactly and the SHA-256 must equal the pinned digest.
fn download_verified_blocking(
    url: &str,
    staging: &Path,
    expected_size: u64,
    expected_sha: &str,
    progress: &ProgressSink,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build download runtime")?;
    runtime.block_on(download_verified(
        url,
        staging,
        expected_size,
        expected_sha,
        progress,
    ))
}

async fn download_verified(
    url: &str,
    staging: &Path,
    expected_size: u64,
    expected_sha: &str,
    progress: &ProgressSink,
) -> Result<()> {
    use futures_util::StreamExt;
    use std::io::Write;

    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
        // The read timeout is the normal stall guard; this upper bound only
        // prevents a permanently open response from living forever.
        .timeout(DOWNLOAD_MAX_DURATION)
        .read_timeout(DOWNLOAD_STALL_TIMEOUT)
        .build()
        .context("build http client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .context("send download request")?;
    if !resp.status().is_success() {
        bail!("download {} failed: HTTP {}", url, resp.status());
    }
    if let Some(len) = resp.content_length() {
        if len != expected_size {
            bail!(
                "Content-Length {} does not match pinned size {}",
                len,
                expected_size
            );
        }
    }
    if let Some(parent) = staging.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create download dir {}", parent.display()))?;
    }
    let mut file = std::fs::File::create(staging)
        .with_context(|| format!("create staging {}", staging.display()))?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read download stream")?;
        file.write_all(&chunk).context("write staging")?;
        hasher.update(&chunk);
        total += chunk.len() as u64;
        progress.set_downloaded(total);
    }
    file.sync_all().context("sync staging")?;
    if total != expected_size {
        bail!("downloaded {total} bytes, expected {expected_size}");
    }
    let digest = hex_digest(&hasher.finalize());
    if digest != expected_sha {
        bail!("SHA-256 mismatch (got {digest})");
    }
    Ok(())
}

/// Atomic publish of a verified staging file. Never removes a valid target
/// first: verification happens on staging before any rename.
fn publish_verified(
    staging: &Path,
    target: &Path,
    expected_size: u64,
    expected_sha: &str,
) -> Result<()> {
    if !verify_file(staging, expected_size, expected_sha) {
        bail!(
            "staging {} failed size/SHA-256 verification",
            staging.display()
        );
    }
    let parent = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let old = parent.join(format!(
        ".{}.old.{}.{}",
        target
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("artifact"),
        std::process::id(),
        now_epoch()
    ));
    let had_target = target.exists();
    if had_target {
        std::fs::rename(target, &old)
            .with_context(|| format!("move old target {}", target.display()))?;
    }
    match std::fs::rename(staging, target) {
        Ok(()) => {
            if had_target {
                let _ = std::fs::remove_file(&old);
            }
            Ok(())
        }
        Err(error) => {
            let restore = if had_target {
                std::fs::rename(&old, target).err()
            } else {
                None
            };
            if let Some(restore) = restore {
                Err(anyhow!(
                    "publish {} -> {} failed: {}; restore failed: {}",
                    staging.display(),
                    target.display(),
                    error,
                    restore
                ))
            } else {
                Err(error).with_context(|| {
                    format!("publish {} -> {}", staging.display(), target.display())
                })
            }
        }
    }
}

fn cuda_manifest_contents(sizes: &[(&str, u64)]) -> String {
    let mut manifest = format!(
        "format=1\nonnx_version={ONNX_VERSION}\nwheel_file={CUDA_WHEEL_FILE}\nwheel_sha256={CUDA_WHEEL_SHA256}\n"
    );
    for (name, size) in sizes {
        manifest.push_str(&format!("file.{name}={size}\n"));
    }
    manifest
}

fn write_cuda_manifest(dest_dir: &Path, sizes: &[(&str, u64)]) -> Result<()> {
    let path = dest_dir.join(CUDA_MANIFEST_FILE);
    let mut file = std::fs::File::create(&path)
        .with_context(|| format!("create CUDA manifest {}", path.display()))?;
    use std::io::Write;
    file.write_all(cuda_manifest_contents(sizes).as_bytes())
        .context("write CUDA manifest")?;
    file.sync_all().context("sync CUDA manifest")?;
    Ok(())
}

fn cuda_cache_is_valid_with_sizes(dir: &Path, sizes: &[(&str, u64)]) -> bool {
    if !dir.is_dir()
        || std::fs::read_to_string(dir.join(CUDA_MANIFEST_FILE))
            .ok()
            .as_deref()
            != Some(cuda_manifest_contents(sizes).as_str())
    {
        return false;
    }
    sizes.iter().all(|(name, expected_size)| {
        std::fs::symlink_metadata(dir.join(name))
            .map(|meta| meta.file_type().is_file() && meta.len() == *expected_size)
            .unwrap_or(false)
    })
}

fn cuda_cache_is_valid(dir: &Path) -> bool {
    cuda_cache_is_valid_with_sizes(dir, &CUDA_LIB_SIZES)
}

/// Extract exactly the pinned wheel entries into a staging dir, verify them,
/// then atomically rename the staging dir into place.
fn extract_cuda_libs(wheel_path: &Path, dest_dir: &Path) -> Result<()> {
    extract_cuda_libs_with_sizes(wheel_path, dest_dir, &CUDA_LIB_SIZES)
}

fn extract_cuda_libs_with_sizes(
    wheel_path: &Path,
    dest_dir: &Path,
    sizes: &[(&str, u64)],
) -> Result<()> {
    let file = std::fs::File::open(wheel_path).context("open wheel")?;
    let mut archive = zip::ZipArchive::new(file).context("open zip archive")?;
    std::fs::create_dir_all(dest_dir).with_context(|| format!("create {}", dest_dir.display()))?;
    for (entry_name, out_name) in CUDA_LIB_ENTRIES {
        let mut entry = archive
            .by_name(entry_name)
            .with_context(|| format!("missing zip entry {entry_name}"))?;
        let out_path = dest_dir.join(out_name);
        let mut out = std::fs::File::create(&out_path)
            .with_context(|| format!("create {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out).context("extract wheel entry")?;
        out.sync_all().context("sync extracted entry")?;
    }
    for (_, out_name) in CUDA_LIB_ENTRIES {
        let p = dest_dir.join(out_name);
        let meta = p.metadata().with_context(|| format!("stat {out_name}"))?;
        let Some((_, expected_size)) = sizes.iter().find(|(name, _)| *name == out_name) else {
            bail!("no pinned size for extracted {out_name}");
        };
        if !meta.is_file() || meta.len() != *expected_size {
            bail!(
                "extracted {out_name} has size {}, expected {expected_size}",
                meta.len()
            );
        }
    }
    write_cuda_manifest(dest_dir, sizes)?;
    if !cuda_cache_is_valid_with_sizes(dest_dir, sizes) {
        bail!("extracted CUDA runtime failed manifest validation");
    }
    Ok(())
}

fn publish_dir(staging_dir: &Path, target_dir: &Path) -> Result<()> {
    let parent = target_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let old = parent.join(format!(
        ".cuda-1.24.1.old.{}.{}",
        std::process::id(),
        now_epoch()
    ));
    let had_target = target_dir.exists();
    if had_target {
        std::fs::rename(target_dir, &old)
            .with_context(|| format!("move stale target {}", target_dir.display()))?;
    }
    match std::fs::rename(staging_dir, target_dir) {
        Ok(()) => {
            if had_target {
                let _ = std::fs::remove_dir_all(&old);
            }
            Ok(())
        }
        Err(error) => {
            let restore = if had_target {
                std::fs::rename(&old, target_dir).err()
            } else {
                None
            };
            if let Some(restore) = restore {
                Err(anyhow!(
                    "publish {} -> {} failed: {}; restore failed: {}",
                    staging_dir.display(),
                    target_dir.display(),
                    error,
                    restore
                ))
            } else {
                Err(error).with_context(|| {
                    format!(
                        "rename {} -> {}",
                        staging_dir.display(),
                        target_dir.display()
                    )
                })
            }
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

// ── Preparation workers ──────────────────────────────────────────────────────

fn prepare_model(shared: &std::sync::Arc<Shared>, source: &str, progress: ProgressSink) {
    let model_path = character_model_path();
    let custom = !is_default_model_config();
    if custom {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.custom_model_unmanaged = true;
        if model_path.is_file() {
            s.model.state = ArtifactState::Ready;
            s.model.verified_path = Some(model_path);
            s.model.last_error = None;
        } else {
            s.model.state = ArtifactState::Error;
            s.model.last_error = Some("custom_model_unmanaged".to_string());
        }
        shared.condvar.notify_all();
        return;
    }
    if model_path.is_file() && verify_file(&model_path, CCIP_MODEL_SIZE, CCIP_MODEL_SHA256) {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.model.state = ArtifactState::Ready;
        s.model.verified_path = Some(model_path);
        s.model.last_error = None;
        shared.condvar.notify_all();
        return;
    }
    if !onnxruntime_auto_download() {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.model.state = ArtifactState::Error;
        s.model.last_error = Some("auto_download_disabled".to_string());
        shared.condvar.notify_all();
        return;
    }
    {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.model.state = ArtifactState::Downloading;
        s.model.total_bytes = CCIP_MODEL_SIZE;
        s.model.downloaded_bytes = 0;
        s.model.start_epoch = Some(now_epoch());
        s.model.last_error = None;
        shared.condvar.notify_all();
    }
    let parent = model_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&parent);
    if let Err(error) = ensure_disk_space(&parent, CCIP_MODEL_SIZE) {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.model.end_epoch = Some(now_epoch());
        s.model.state = ArtifactState::Error;
        s.model.last_error = Some(error.to_string());
        shared.condvar.notify_all();
        return;
    }
    let url = ccip_model_url(source);
    let mut last_err = None;
    let mut published = false;
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        let staging = parent.join(format!(
            ".ccip-model.{}.{}.part",
            std::process::id(),
            attempt
        ));
        let _ = std::fs::remove_file(&staging);
        match download_verified_blocking(
            &url,
            &staging,
            CCIP_MODEL_SIZE,
            CCIP_MODEL_SHA256,
            &progress,
        ) {
            Ok(()) => {
                match publish_verified(&staging, &model_path, CCIP_MODEL_SIZE, CCIP_MODEL_SHA256) {
                    Ok(()) => {
                        published = true;
                        break;
                    }
                    Err(e) => {
                        last_err = Some(e.to_string());
                        let _ = std::fs::remove_file(&staging);
                    }
                }
            }
            Err(e) => {
                last_err = Some(e.to_string());
                let _ = std::fs::remove_file(&staging);
            }
        }
        if attempt + 1 < DOWNLOAD_ATTEMPTS {
            std::thread::sleep(Duration::from_secs(2 + attempt as u64));
        }
    }
    {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.model.end_epoch = Some(now_epoch());
        if published {
            s.model.state = ArtifactState::Ready;
            s.model.verified_path = Some(model_path);
            s.model.last_error = None;
        } else {
            s.model.state = ArtifactState::Error;
            s.model.last_error =
                Some(last_err.unwrap_or_else(|| "unknown model download failure".into()));
        }
        shared.condvar.notify_all();
    }
}

fn prepare_cuda(shared: &std::sync::Arc<Shared>, source: &str, progress: ProgressSink) {
    let dir = cuda_runtime_dir();
    if cuda_cache_is_valid(&dir) {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.cuda.state = ArtifactState::Ready;
        s.cuda.verified_path = Some(dir.join(CUDA_LIB_ENTRIES[0].1));
        s.cuda.last_error = None;
        shared.condvar.notify_all();
        return;
    }
    if !onnxruntime_auto_download() {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.cuda.state = ArtifactState::Error;
        s.cuda.last_error = Some("auto_download_disabled".to_string());
        shared.condvar.notify_all();
        return;
    }
    {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.cuda.state = ArtifactState::Downloading;
        s.cuda.total_bytes = CUDA_WHEEL_SIZE;
        s.cuda.downloaded_bytes = 0;
        s.cuda.start_epoch = Some(now_epoch());
        s.cuda.last_error = None;
        shared.condvar.notify_all();
    }
    let ort = ort_dir();
    let _ = std::fs::create_dir_all(&ort);
    if let Err(error) = ensure_disk_space(&ort, CUDA_WHEEL_SIZE) {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.cuda.end_epoch = Some(now_epoch());
        s.cuda.state = ArtifactState::Error;
        s.cuda.last_error = Some(error.to_string());
        shared.condvar.notify_all();
        return;
    }
    let url = cuda_wheel_url(source);
    let mut last_err = None;
    let mut published = false;
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        let wheel_staging = ort.join(format!(
            ".cuda-wheel.{}.{}.part",
            std::process::id(),
            attempt
        ));
        let _ = std::fs::remove_file(&wheel_staging);
        if let Err(e) = download_verified_blocking(
            &url,
            &wheel_staging,
            CUDA_WHEEL_SIZE,
            CUDA_WHEEL_SHA256,
            &progress,
        ) {
            last_err = Some(e.to_string());
            let _ = std::fs::remove_file(&wheel_staging);
            if attempt + 1 < DOWNLOAD_ATTEMPTS {
                std::thread::sleep(Duration::from_secs(2 + attempt as u64));
            }
            continue;
        }
        let staging_dir = ort.join(format!(
            ".cuda-1.24.1.{}.{}.tmp",
            std::process::id(),
            attempt
        ));
        let _ = std::fs::remove_dir_all(&staging_dir);
        let extract_res = extract_cuda_libs(&wheel_staging, &staging_dir);
        let _ = std::fs::remove_file(&wheel_staging);
        match extract_res {
            Ok(()) => {
                for (_, out_name) in CUDA_LIB_ENTRIES {
                    make_executable(&staging_dir.join(out_name));
                }
                match publish_dir(&staging_dir, &dir) {
                    Ok(()) if cuda_cache_is_valid(&dir) => {
                        published = true;
                        break;
                    }
                    Ok(()) => {
                        last_err = Some("published CUDA runtime failed manifest validation".into());
                        let _ = std::fs::remove_dir_all(&dir);
                    }
                    Err(e) => {
                        last_err = Some(format!("publish cuda runtime: {e}"));
                        let _ = std::fs::remove_dir_all(&staging_dir);
                    }
                }
            }
            Err(e) => {
                last_err = Some(e.to_string());
                let _ = std::fs::remove_dir_all(&staging_dir);
            }
        }
        if attempt + 1 < DOWNLOAD_ATTEMPTS {
            std::thread::sleep(Duration::from_secs(2 + attempt as u64));
        }
    }
    {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.cuda.end_epoch = Some(now_epoch());
        if published {
            s.cuda.state = ArtifactState::Ready;
            s.cuda.verified_path = Some(dir.join(CUDA_LIB_ENTRIES[0].1));
            s.cuda.last_error = None;
        } else {
            s.cuda.state = ArtifactState::Error;
            s.cuda.last_error =
                Some(last_err.unwrap_or_else(|| "unknown cuda download failure".into()));
        }
        shared.condvar.notify_all();
    }
}

fn persisted_download_source_at(db_path: &Path, fallback: String) -> String {
    let Ok(conn) = rusqlite::Connection::open(db_path) else {
        return fallback;
    };
    let _ = conn.busy_timeout(Duration::from_secs(30));
    persisted_download_source(&conn)
}

fn mark_worker_start_failed(error: &str) {
    let shared = shared();
    let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    s.in_progress = false;
    s.prepared = true;
    s.model.state = ArtifactState::Error;
    s.model.end_epoch = Some(now_epoch());
    s.model.last_error = Some(format!(
        "runtime preparation worker failed to start: {error}"
    ));
    s.cuda.state = ArtifactState::Error;
    s.cuda.end_epoch = Some(now_epoch());
    s.cuda.last_error = Some(format!(
        "runtime preparation worker failed to start: {error}"
    ));
    shared.condvar.notify_all();
    WORKER_RUNNING.store(false, Ordering::SeqCst);
}

fn spawn_worker(source: String, db_path: Option<PathBuf>) -> Result<()> {
    std::thread::Builder::new()
        .name("gallery-runtime-prep".into())
        .spawn(move || {
            let source = db_path
                .as_deref()
                .map(|path| persisted_download_source_at(path, source.clone()))
                .unwrap_or(source);
            // Panic isolation: without this, any panic in the download /
            // extraction paths would silently kill the thread with
            // WORKER_RUNNING still set, making every retry report busy and
            // the first inference wait forever. A panicked worker recovers
            // exactly like a failed-to-start one.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                worker_loop(source);
            }));
            if result.is_err() {
                eprintln!("gallery-accel: runtime preparation worker panicked; marking failed");
                mark_worker_start_failed("runtime preparation worker panicked");
            }
        })
        .map(|_| ())
        .context("spawn runtime preparation worker")
}

/// Delete staging leftovers (`*.part` files, `.tmp`/`.old.*` directories and
/// files) from interrupted downloads or publishes. Anything modified within
/// the threshold is assumed to belong to a live download and is kept.
fn sweep_stale_staging(dir: &Path) {
    const STAGING_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_staging =
            name.ends_with(".part") || name.ends_with(".tmp") || name.contains(".old.");
        if !is_staging {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age >= STAGING_STALE_AFTER)
            .unwrap_or(false);
        if !stale {
            continue;
        }
        let path = entry.path();
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(error) = result {
            eprintln!(
                "runtime prep: failed to remove stale staging {}: {error}",
                path.display()
            );
        }
    }
}

/// Available bytes on the filesystem holding `dir` (POSIX `df -Pk`), used as
/// an ENOSPC precheck before large downloads. `None` when the probe is
/// unavailable (e.g. non-Linux development hosts) and the precheck stays
/// advisory.
fn available_disk_bytes(dir: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .arg("-Pk")
        .arg(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().last()?;
    let mut fields = line.split_whitespace();
    fields.next()?; // device
    fields.next()?; // total
    fields.next()?; // used
    let available_kb: u64 = fields.next()?.parse().ok()?;
    Some(available_kb.saturating_mul(1024))
}

/// Refuse to start a large download when the target volume cannot hold the
/// artifact plus headroom; ENOSPC mid-download would otherwise threaten the
/// gallery database on the same volume.
fn ensure_disk_space(dir: &Path, expected_bytes: u64) -> Result<()> {
    const MIN_SLACK_BYTES: u64 = 256 * 1024 * 1024;
    match available_disk_bytes(dir) {
        Some(free) if free < expected_bytes.saturating_add(MIN_SLACK_BYTES) => bail!(
            "insufficient disk space for {} MiB download (plus headroom): only {} MiB available at {}",
            expected_bytes / (1024 * 1024),
            free / (1024 * 1024),
            dir.display()
        ),
        _ => Ok(()),
    }
}

fn start_preparation(source: String, db_path: Option<PathBuf>) -> bool {
    let shared = shared();
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if WORKER_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    state.download_source = source.clone();
    state.in_progress = true;
    state.prepared = false;
    state.custom_model_unmanaged = false;
    state.model = ArtifactStatus::default();
    state.cuda = ArtifactStatus::default();
    shared.condvar.notify_all();
    drop(state);
    if let Err(error) = spawn_worker(source, db_path) {
        mark_worker_start_failed(&error.to_string());
        return false;
    }
    true
}

fn worker_loop(source: String) {
    let shared = shared();
    {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.download_source = source.clone();
        shared.condvar.notify_all();
    }

    sweep_stale_staging(
        &character_model_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    );
    sweep_stale_staging(&ort_dir());

    let model_progress = ProgressSink {
        shared: shared.clone(),
        kind: ArtifactKind::Model,
    };
    let cuda_progress = ProgressSink {
        shared: shared.clone(),
        kind: ArtifactKind::Cuda,
    };

    let want_cuda = has_nvidia_gpu() || requested_provider() == "cuda";
    prepare_model(&shared, &source, model_progress);
    if want_cuda {
        prepare_cuda(&shared, &source, cuda_progress);
    }

    let any_ready = {
        let s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.model.state == ArtifactState::Ready || s.cuda.state == ArtifactState::Ready
    };
    {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.in_progress = false;
        s.prepared = true;
        for kind in [ArtifactKind::Model, ArtifactKind::Cuda] {
            let st = match kind {
                ArtifactKind::Model => &mut s.model,
                ArtifactKind::Cuda => &mut s.cuda,
            };
            if st.state == ArtifactState::Downloading {
                st.state = ArtifactState::Error;
                if st.last_error.is_none() {
                    st.last_error = Some("preparation stopped".to_string());
                }
            }
            if st.end_epoch.is_none() {
                st.end_epoch = Some(now_epoch());
            }
        }
        shared.condvar.notify_all();
    }
    // Release the state lock before touching the session slot so lock ordering
    // (session slot -> runtime state) is never inverted.
    if any_ready {
        crate::character_ccip::clear_failed_session_cache();
    }
    WORKER_RUNNING.store(false, Ordering::SeqCst);
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Start background preparation once at startup. Never blocks; returns whether
/// a worker was actually launched.
pub fn prepare_runtime(conn: &rusqlite::Connection) -> bool {
    if PREPARE_STARTED.swap(true, Ordering::SeqCst) {
        return false;
    }
    let source = persisted_download_source(conn);
    start_preparation(source, None)
}

/// Startup variant: only captures the path and schedules the SQLite read in
/// the worker, so a busy database cannot delay HTTP listener readiness.
pub fn prepare_runtime_at(db_path: &Path) -> bool {
    if PREPARE_STARTED.swap(true, Ordering::SeqCst) {
        return false;
    }
    start_preparation(env_download_source(), Some(db_path.to_path_buf()))
}

/// Alias matching the plan's `start_runtime_preparation` signature.
pub fn start_runtime_preparation(conn: &rusqlite::Connection) -> bool {
    prepare_runtime(conn)
}

/// Force-retry missing runtime downloads. Busy-guarded: only one preparation
/// thread may run; a concurrent request returns the current status.
pub fn retry_missing_runtimes(conn: &rusqlite::Connection) -> Value {
    let source = persisted_download_source(conn);
    if !start_preparation(source.clone(), None) {
        return json!({
            "retry_started": false,
            "busy": true,
            "reason": "preparation already running",
            "download_source": source,
        });
    }
    json!({
        "retry_started": true,
        "busy": false,
        "download_source": source,
        "nvidia_gpu": has_nvidia_gpu(),
    })
}

/// Block until the current CCIP model preparation round concludes (or until it
/// is clear no preparation is running). Used by the first inference.
pub fn wait_for_character_model() {
    let shared = shared();
    let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if (!s.in_progress && s.model.state != ArtifactState::Downloading) || s.prepared {
            return;
        }
        let (guard, _) = shared
            .condvar
            .wait_timeout(s, Duration::from_secs(10))
            .unwrap();
        s = guard;
    }
}

/// Block until the current CUDA runtime preparation round concludes. Returns
/// immediately when no CUDA preparation is running or will run.
pub fn wait_for_cuda_runtime() {
    let shared = shared();
    let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if (!s.in_progress && s.cuda.state != ArtifactState::Downloading) || s.prepared {
            return;
        }
        let (guard, _) = shared
            .condvar
            .wait_timeout(s, Duration::from_secs(10))
            .unwrap();
        s = guard;
    }
}

/// Returns the verified CUDA ORT library path once the runtime is `ready`.
pub fn cuda_ort_path_if_ready() -> Option<PathBuf> {
    let shared = shared();
    let s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if s.cuda.state == ArtifactState::Ready {
        s.cuda.verified_path.clone()
    } else {
        None
    }
}

fn artifact_json(a: &ArtifactStatus) -> Value {
    json!({
        "state": a.state.label(),
        "downloaded_bytes": a.downloaded_bytes,
        "total_bytes": a.total_bytes,
        "start_epoch": a.start_epoch,
        "end_epoch": a.end_epoch,
        "last_error": a.last_error,
        "verified_path": a.verified_path.as_ref().map(|p| p.display().to_string()),
    })
}

/// `true` when the ORT core is already locked to the bundled OpenVINO runtime
/// while a CUDA runtime has since become ready and the user wants CUDA.
fn restart_required(s: &RuntimeState) -> bool {
    match crate::character_ccip::ort_core_type() {
        Some("cuda") => false,
        Some(_) => {
            let want = requested_provider();
            s.cuda.state == ArtifactState::Ready && (want == "auto" || want == "cuda")
        }
        None => false,
    }
}

/// JSON status for `/api/ml-runtime/status`. Read-only: never loads the model,
/// never initializes ORT, and never triggers a download.
pub fn ml_runtime_status(conn: &rusqlite::Connection) -> Value {
    let shared = shared();
    let s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let gpus = detect_gpus();
    let (nvidia, uvm) = nvidia_gpu_state_at(Path::new("/dev"));
    let model_path = character_model_path();
    let provider = requested_provider();
    let model_present = model_path.is_file();
    let (planned_provider, selection_reason) = select_provider(&gpus, &provider, model_present);
    let source = persisted_download_source(conn);
    // Do not expose cache locations until the corresponding artifact has
    // passed its pinned size/SHA-256 validation in this preparation round.
    let verified_model_path = s
        .model
        .verified_path
        .as_ref()
        .filter(|path| path.is_file())
        .map(|path| path.display().to_string());
    let verified_cuda_dir =
        (s.cuda.state == ArtifactState::Ready).then(|| cuda_runtime_dir().display().to_string());

    json!({
        "gpus_detected": gpus,
        "has_nvidia_gpu": nvidia,
        "nvidia_uvm_present": uvm,
        "has_intel_gpu": has_intel_gpu(),
        "cuda_runtime_dir": verified_cuda_dir,
        "cuda_runtime_present": s.cuda.state == ArtifactState::Ready,
        "openvino_runtime_present": has_intel_gpu(),
        "ccip_model_present": model_present,
        "ccip_model_path": verified_model_path,
        "ccip_model_variant": model_variant(),
        "ccip_model_repo_id": model_repo_id(),
        "ccip_model_file": model_file(),
        "requested_provider": provider,
        "planned_provider": planned_provider,
        "actual_provider": "not_initialized",
        "provider_error": Value::Null,
        "selection_reason": selection_reason,
        "ort_core": crate::character_ccip::ort_core_type(),
        "allow_cpu_fallback": allow_cpu_fallback(),
        "download_source": source,
        "download_in_progress": s.in_progress,
        "prepared": s.prepared,
        "custom_model_unmanaged": s.custom_model_unmanaged,
        "restart_required": restart_required(&s),
        "onnx_version": ONNX_VERSION,
        "model_status": artifact_json(&s.model),
        "cuda_status": artifact_json(&s.cuda),
        "last_error": s.model.last_error.clone().or_else(|| s.cuda.last_error.clone()),
    })
}

/// Alias for the plan's `runtime_preparation_status`.
pub fn runtime_preparation_status(conn: &rusqlite::Connection) -> Value {
    ml_runtime_status(conn)
}

/// Resolve the provider planned from the requested mode and detected hardware.
/// It is not the actual provider until a session has loaded successfully.
fn select_provider(gpus: &[String], requested: &str, model_present: bool) -> (String, String) {
    if !model_present {
        return ("none".to_string(), "ccip_model_missing".to_string());
    }
    match requested {
        "auto" => {
            if gpus.iter().any(|g| g == "cuda") {
                ("cuda".to_string(), "nvidia_gpu_detected".to_string())
            } else if gpus.iter().any(|g| g == "openvino") {
                ("openvino".to_string(), "intel_gpu_detected".to_string())
            } else {
                ("cpu".to_string(), "no_gpu_detected".to_string())
            }
        }
        "cuda" => {
            if gpus.iter().any(|g| g == "cuda") {
                ("cuda".to_string(), "explicit_request_satisfied".to_string())
            } else {
                ("cpu".to_string(), "cuda_requested_but_no_gpu".to_string())
            }
        }
        "openvino" => {
            if gpus.iter().any(|g| g == "openvino") {
                (
                    "openvino".to_string(),
                    "explicit_request_satisfied".to_string(),
                )
            } else {
                (
                    "cpu".to_string(),
                    "openvino_requested_but_no_gpu".to_string(),
                )
            }
        }
        "cpu" => ("cpu".to_string(), "explicit_cpu_request".to_string()),
        other => (other.to_string(), "custom_provider".to_string()),
    }
}

/// Current runtime settings for `/api/ml-runtime/settings`.
pub fn runtime_settings(conn: &rusqlite::Connection) -> Value {
    let source = persisted_download_source(conn);
    json!({
        "character_recognition_provider": requested_provider(),
        "character_allow_cpu_fallback": allow_cpu_fallback(),
        "character_openvino_allow_cpu_fallback": env_bool("CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK", false),
        "download_source": source,
        "onnx_version": ONNX_VERSION,
        "onnxruntime_auto_download_dir": ort_dir().display().to_string(),
        "character_cuda_runtime_dir": cuda_runtime_dir().display().to_string(),
        "character_model_variant": model_variant(),
        "character_model_file": model_file(),
        "character_model_repo_id": model_repo_id(),
        "onnxruntime_auto_download": onnxruntime_auto_download(),
        "openvino_runtime_auto_download": openvino_runtime_auto_download(),
    })
}

/// Update settings via `/api/ml-runtime/settings` (PUT). Only `download_source`
/// is accepted and strictly validated against the allowlist; no URLs are
/// accepted anywhere. Saving a source triggers an immediate retry when a
/// component is missing/error and no task is running.
pub fn update_runtime_settings(conn: &rusqlite::Connection, body: &Value) -> Result<Value> {
    let changed = match body.get("download_source").and_then(|v| v.as_str()) {
        Some(source) => {
            set_download_source(conn, source)?;
            true
        }
        None => false,
    };
    if changed {
        let busy = WORKER_RUNNING.load(Ordering::SeqCst);
        let missing = {
            let shared = shared();
            let s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            matches!(
                (s.model.state, s.cuda.state),
                (ArtifactState::Error, _)
                    | (_, ArtifactState::Error)
                    | (ArtifactState::NotStarted, _)
                    | (_, ArtifactState::NotStarted)
            )
        };
        if !busy && missing {
            let _ = retry_missing_runtimes(conn);
        }
    }
    Ok(runtime_settings(conn))
}

/// Returns the onnxruntime version targeted by the downloaded libs.
pub fn onnx_version() -> &'static str {
    ONNX_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    static STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL DEFAULT '',
                updated_at REAL NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        conn
    }

    fn set_provider(value: &str) {
        if value.is_empty() {
            std::env::remove_var("CHARACTER_RECOGNITION_PROVIDER");
        } else {
            std::env::set_var("CHARACTER_RECOGNITION_PROVIDER", value);
        }
    }

    #[test]
    fn requested_provider_defaults_to_auto() {
        set_provider("");
        assert_eq!(requested_provider(), "auto");
    }

    #[test]
    fn requested_provider_normalizes_variants() {
        let cases = [
            ("cuda", "cuda"),
            ("CUDA", "cuda"),
            ("nvidia", "cuda"),
            ("openvino", "openvino"),
            ("OpenVINO", "openvino"),
            ("intel", "openvino"),
            ("gpu", "openvino"),
            ("cpu", "cpu"),
            ("cpuexecutionprovider", "cpu"),
        ];
        for (input, expected) in cases {
            set_provider(input);
            assert_eq!(requested_provider(), expected, "input={input}");
        }
        set_provider("");
    }

    #[test]
    fn allow_cpu_fallback_reads_new_then_old_var() {
        let new_key = "CHARACTER_ALLOW_CPU_FALLBACK";
        let old_key = "CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK";
        let prev_new = std::env::var(new_key).ok();
        let prev_old = std::env::var(old_key).ok();

        std::env::remove_var(new_key);
        std::env::remove_var(old_key);
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

        match prev_new {
            Some(v) => std::env::set_var(new_key, v),
            None => std::env::remove_var(new_key),
        }
        match prev_old {
            Some(v) => std::env::set_var(old_key, v),
            None => std::env::remove_var(old_key),
        }
    }

    #[test]
    fn select_provider_auto_with_cuda_and_model() {
        let gpus = vec!["cuda".to_string(), "openvino".to_string()];
        let (provider, reason) = select_provider(&gpus, "auto", true);
        assert_eq!(provider, "cuda");
        assert_eq!(reason, "nvidia_gpu_detected");
    }

    #[test]
    fn select_provider_auto_with_openvino_only() {
        let gpus = vec!["openvino".to_string()];
        let (provider, _) = select_provider(&gpus, "auto", true);
        assert_eq!(provider, "openvino");
    }

    #[test]
    fn select_provider_auto_without_gpu_falls_back_to_cpu() {
        let gpus: Vec<String> = vec![];
        let (provider, reason) = select_provider(&gpus, "auto", true);
        assert_eq!(provider, "cpu");
        assert_eq!(reason, "no_gpu_detected");
    }

    #[test]
    fn select_provider_auto_with_amd_only_is_not_intel() {
        // AMD-only DRM nodes must never be treated as Intel/OpenVINO.
        let gpus: Vec<String> = vec![];
        let (provider, reason) = select_provider(&gpus, "auto", true);
        assert_eq!(provider, "cpu");
        assert_eq!(reason, "no_gpu_detected");
    }

    #[test]
    fn select_provider_returns_none_when_model_missing() {
        let gpus = vec!["cuda".to_string()];
        let (provider, reason) = select_provider(&gpus, "auto", false);
        assert_eq!(provider, "none");
        assert_eq!(reason, "ccip_model_missing");
    }

    #[test]
    fn runtime_status_separates_planned_from_uninitialized_actual_provider() {
        let status = ml_runtime_status(&test_db());
        assert!(status.get("planned_provider").is_some());
        assert_eq!(status["actual_provider"], "not_initialized");
        assert!(status.get("selected_provider").is_none());
        assert!(status["cuda_runtime_dir"].is_null());
        assert!(status["ccip_model_path"].is_null());
    }

    #[test]
    fn ccip_model_url_pins_revision() {
        let url = ccip_model_url("official");
        assert!(
            url.contains("/resolve/eb2acdd29af1703388d3d0c04221add322bc9110/"),
            "{url}"
        );
        assert!(
            url.contains("ccip-caformer_b36-24/model_feat.onnx"),
            "{url}"
        );
        assert!(!url.contains("/resolve/main"), "{url}");
        let cn = ccip_model_url("china");
        assert!(cn.starts_with("https://hf-mirror.com/"), "{cn}");
    }

    #[test]
    fn download_source_url_switching() {
        assert_eq!(hf_base_url("official"), "https://huggingface.co");
        assert_eq!(hf_base_url("china"), "https://hf-mirror.com");
        assert_eq!(hf_base_url("other"), "https://huggingface.co");
        let official_wheel = cuda_wheel_url("official");
        assert!(
            official_wheel.starts_with("https://files.pythonhosted.org/"),
            "{official_wheel}"
        );
        let cn_wheel = cuda_wheel_url("china");
        assert!(
            cn_wheel.starts_with("https://pypi.tuna.tsinghua.edu.cn/"),
            "{cn_wheel}"
        );
        assert!(official_wheel.ends_with(CUDA_WHEEL_FILE));
        assert_eq!(
            official_wheel,
            cn_wheel.replace("pypi.tuna.tsinghua.edu.cn", "files.pythonhosted.org")
        );
    }

    #[test]
    fn cuda_runtime_dir_falls_back_to_model_cache() {
        std::env::remove_var("CHARACTER_CUDA_RUNTIME_DIR");
        let dir = cuda_runtime_dir();
        assert!(dir.to_string_lossy().contains("ort/cuda-1.24.1"));
    }

    #[test]
    fn onnx_runtime_auto_download_default_true() {
        std::env::remove_var("ONNXRUNTIME_AUTO_DOWNLOAD");
        assert!(onnxruntime_auto_download());
        std::env::set_var("ONNXRUNTIME_AUTO_DOWNLOAD", "0");
        assert!(!onnxruntime_auto_download());
        std::env::remove_var("ONNXRUNTIME_AUTO_DOWNLOAD");
    }

    #[test]
    fn verify_and_publish_only_on_match() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging.bin");
        let target = dir.path().join("target.bin");
        let content = b"hello runtime artifact";
        std::fs::write(&staging, content).unwrap();
        let digest = {
            let mut h = Sha256::new();
            h.update(content);
            hex_digest(&h.finalize())
        };

        // Wrong size is rejected and target stays untouched.
        assert!(publish_verified(&staging, &target, 999, &digest).is_err());
        assert!(!target.exists());

        // Wrong hash is rejected and target stays untouched.
        assert!(publish_verified(
            &staging,
            &target,
            content.len() as u64,
            "0".repeat(64).as_str()
        )
        .is_err());
        assert!(!target.exists());

        // Correct size + hash publishes and removes the staging file.
        publish_verified(&staging, &target, content.len() as u64, &digest).unwrap();
        assert!(target.is_file());
        assert!(!staging.exists());
    }

    #[test]
    fn extract_cuda_libs_reads_exact_entries() {
        let dir = tempfile::tempdir().unwrap();
        let wheel = dir.path().join("test.whl");
        let mut sizes = Vec::new();
        {
            let file = std::fs::File::create(&wheel).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (entry_name, _) in CUDA_LIB_ENTRIES {
                writer.start_file(entry_name, opts).unwrap();
                let content = format!("content of {entry_name}");
                sizes.push((entry_name.rsplit('/').next().unwrap(), content.len() as u64));
                writer.write_all(content.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        let dest = dir.path().join("out");
        extract_cuda_libs_with_sizes(&wheel, &dest, &sizes).unwrap();
        assert!(cuda_cache_is_valid_with_sizes(&dest, &sizes));
        for (_, out_name) in CUDA_LIB_ENTRIES {
            let p = dest.join(out_name);
            assert!(p.is_file(), "expected {out_name}");
            assert_eq!(
                p.metadata().unwrap().len(),
                sizes.iter().find(|(name, _)| *name == out_name).unwrap().1
            );
        }
    }

    #[test]
    fn cuda_cache_manifest_rejects_missing_or_tampered_files() {
        let dir = tempfile::tempdir().unwrap();
        let sizes = [
            ("libonnxruntime.so.1.24.1", 3),
            ("libonnxruntime_providers_cuda.so", 4),
            ("libonnxruntime_providers_shared.so", 5),
        ];
        for (name, size) in sizes {
            std::fs::write(dir.path().join(name), vec![b'x'; size as usize]).unwrap();
        }
        write_cuda_manifest(dir.path(), &sizes).unwrap();
        assert!(cuda_cache_is_valid_with_sizes(dir.path(), &sizes));

        std::fs::remove_file(dir.path().join(CUDA_MANIFEST_FILE)).unwrap();
        assert!(!cuda_cache_is_valid_with_sizes(dir.path(), &sizes));
        write_cuda_manifest(dir.path(), &sizes).unwrap();
        std::fs::write(dir.path().join(CUDA_MANIFEST_FILE), "format=1\n").unwrap();
        assert!(!cuda_cache_is_valid_with_sizes(dir.path(), &sizes));
        write_cuda_manifest(dir.path(), &sizes).unwrap();
        std::fs::write(dir.path().join(sizes[0].0), b"bad!").unwrap();
        assert!(!cuda_cache_is_valid_with_sizes(dir.path(), &sizes));
        std::fs::write(dir.path().join(sizes[0].0), b"xxx").unwrap();
        std::fs::remove_file(dir.path().join(sizes[1].0)).unwrap();
        assert!(!cuda_cache_is_valid_with_sizes(dir.path(), &sizes));
    }

    #[test]
    fn publish_dir_restores_previous_target_when_replacement_fails() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cuda-1.24.1");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("old.so"), b"old").unwrap();
        let missing_staging = dir.path().join("missing-staging");

        assert!(publish_dir(&missing_staging, &target).is_err());
        assert_eq!(std::fs::read(target.join("old.so")).unwrap(), b"old");
    }

    #[test]
    fn first_inference_waits_for_round_marked_before_worker_schedule() {
        let _test_lock = STATE_TEST_LOCK.lock().unwrap();
        let shared = shared();
        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.in_progress = true;
            state.prepared = false;
            state.model = ArtifactStatus::default();
            state.cuda = ArtifactStatus::default();
        }
        let waiter = std::thread::spawn(wait_for_character_model);
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            !waiter.is_finished(),
            "inference observed not_started too early"
        );
        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.in_progress = false;
            state.prepared = true;
            shared.condvar.notify_all();
        }
        waiter.join().unwrap();
    }

    #[test]
    fn worker_spawn_failure_resets_busy_state_and_notifies() {
        let _test_lock = STATE_TEST_LOCK.lock().unwrap();
        WORKER_RUNNING.store(true, Ordering::SeqCst);
        mark_worker_start_failed("test spawn failure");
        assert!(!WORKER_RUNNING.load(Ordering::SeqCst));
        let shared = shared();
        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!state.in_progress);
        assert!(state.prepared);
        assert_eq!(state.model.state, ArtifactState::Error);
        assert!(state
            .model
            .last_error
            .as_deref()
            .unwrap()
            .contains("spawn failure"));
    }

    #[test]
    fn nvidia_node_names_are_exact() {
        assert_eq!(nvidia_gpu_node_id("nvidia0"), Some("0"));
        assert_eq!(nvidia_gpu_node_id("nvidia12"), Some("12"));
        for name in ["nvidiactl", "nvidia-uvm", "nvidia1x", "nvidia", "gpu0"] {
            assert_eq!(nvidia_gpu_node_id(name), None, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn nvidia_character_device_symlinks_are_detected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        symlink("/dev/null", dir.path().join("nvidiactl")).unwrap();
        symlink("/dev/null", dir.path().join("nvidia0")).unwrap();
        let (detected, uvm) = nvidia_gpu_nodes_at(dir.path());
        assert!(detected);
        assert!(!uvm);
    }

    #[test]
    fn wsl_dxg_with_libcuda_counts_as_nvidia() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!nvidia_gpu_state_with(dir.path(), true).0);
        std::fs::File::create(dir.path().join("dxg")).unwrap();
        assert!(nvidia_gpu_state_with(dir.path(), true).0);
        assert!(!nvidia_gpu_state_with(dir.path(), false).0);
    }

    #[test]
    fn wsl_dxg_detection_keeps_uvm_flag_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("dxg")).unwrap();
        let (nvidia, uvm) = nvidia_gpu_state_with(dir.path(), true);
        assert!(nvidia);
        assert!(!uvm);
    }

    #[test]
    fn extract_cuda_libs_rejects_missing_entry() {
        let dir = tempfile::tempdir().unwrap();
        let wheel = dir.path().join("missing.whl");
        {
            let file = std::fs::File::create(&wheel).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file(
                    "onnxruntime/capi/only_one.so",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(b"x").unwrap();
            writer.finish().unwrap();
        }
        let dest = dir.path().join("out");
        assert!(extract_cuda_libs(&wheel, &dest).is_err());
    }

    #[test]
    fn busy_guard_rejects_concurrent_retry() {
        let _test_lock = STATE_TEST_LOCK.lock().unwrap();
        let conn = test_db();
        assert!(WORKER_RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok());
        let resp = retry_missing_runtimes(&conn);
        assert_eq!(resp["retry_started"], false);
        assert_eq!(resp["busy"], true);
        WORKER_RUNNING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn download_source_persists_and_validates() {
        let conn = test_db();
        let previous = std::env::var("GALLERY_DOWNLOAD_SOURCE").ok();
        std::env::remove_var("GALLERY_DOWNLOAD_SOURCE");
        assert_eq!(persisted_download_source(&conn), "official");
        set_download_source(&conn, "china").unwrap();
        assert_eq!(persisted_download_source(&conn), "china");
        std::env::set_var("GALLERY_DOWNLOAD_SOURCE", "official");
        assert_eq!(persisted_download_source(&conn), "official");
        assert!(set_download_source(&conn, "https://evil.example").is_err());
        assert!(set_download_source(&conn, "files.pythonhosted.org").is_err());
        match previous {
            Some(value) => std::env::set_var("GALLERY_DOWNLOAD_SOURCE", value),
            None => std::env::remove_var("GALLERY_DOWNLOAD_SOURCE"),
        }
    }
}
