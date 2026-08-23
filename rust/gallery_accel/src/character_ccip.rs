//! Character CCIP embedding + recognition (pure Rust, official ONNX Runtime).
//!
//! Preprocess matches Python `embedding.py`:
//! RGB → resize 384×384 bicubic → NCHW float32 /255 → session → L2 normalize (dim 768).
//! Prefer existing `IMAGE_PREVIEW_CACHE_DIR` thumbs (512) over decoding the full original.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use image::imageops::FilterType;
use ort::ep::{self, ExecutionProvider};
use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::media_roots::{authorized_media_path, env_media_roots, MediaRoots};

const IMAGE_SIZE: u32 = 384;
const EMBEDDING_DIM: usize = 768;
const DEFAULT_THRESHOLD: f32 = 0.23;
const DEFAULT_MIN_GAP: f32 = 0.04;
const DEFAULT_MODEL_IDLE_TIMEOUT_SECONDS: u64 = 600;

/// ONNX Runtime core the process is locked to. A process can only initialize
/// one ORT core; CUDA and bundled-OpenVINO cores cannot be hot-swapped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OrtCoreType {
    Cpu,
    OpenVino,
    Cuda,
}

impl OrtCoreType {
    fn label(self) -> &'static str {
        match self {
            OrtCoreType::Cpu => "cpu",
            OrtCoreType::OpenVino => "openvino",
            OrtCoreType::Cuda => "cuda",
        }
    }
}

const CORE_NONE: u8 = 0;
const CORE_CPU: u8 = 1;
const CORE_OPENVINO: u8 = 2;
const CORE_CUDA: u8 = 3;

/// Atomic core type so runtime status can read it without taking a lock (the
/// runtime state lock is held by the caller; lock ordering is never inverted).
static ORT_CORE: AtomicU8 = AtomicU8::new(CORE_NONE);

/// Serializes the one-time ORT environment initialization across threads.
static ORT_INIT_LOCK: Mutex<()> = Mutex::new(());

static ACTIVE_PROVIDER: OnceLock<String> = OnceLock::new();
static ACTIVE_DEVICE: OnceLock<String> = OnceLock::new();

/// True while a thread is building the ORT session (possibly waiting minutes
/// for a model/CUDA download). Read without the session-slot lock so status
/// endpoints can report `preparing` instead of blocking behind the builder.
static SESSION_BUILDING: AtomicBool = AtomicBool::new(false);

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

fn env_f32(key: &str, default: f32) -> f32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn model_dir() -> PathBuf {
    std::env::var("CHARACTER_MODEL_DIR")
        .or_else(|_| std::env::var("MODEL_CACHE_ROOT").map(|r| format!("{r}/character")))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data/models/character"))
}

fn model_variant() -> String {
    std::env::var("CHARACTER_MODEL_VARIANT").unwrap_or_else(|_| "ccip-caformer_b36-24".into())
}

fn model_file() -> String {
    std::env::var("CHARACTER_MODEL_FILE").unwrap_or_else(|_| "model_feat.onnx".into())
}

fn model_repo_id() -> String {
    std::env::var("CHARACTER_MODEL_REPO_ID").unwrap_or_else(|_| "deepghs/ccip_onnx".into())
}

pub fn character_model_path() -> PathBuf {
    model_dir().join(model_variant()).join(model_file())
}

fn threshold() -> f32 {
    env_f32("CHARACTER_RECOGNITION_THRESHOLD", DEFAULT_THRESHOLD)
}

fn min_gap() -> f32 {
    env_f32("CHARACTER_RECOGNITION_MIN_GAP", DEFAULT_MIN_GAP)
}

struct CcipSession {
    session: Session,
    input_name: String,
    provider: String,
    active_device: String,
    fallback_reason: Option<String>,
}

#[derive(Default)]
struct CcipSessionSlot {
    session: Option<Result<CcipSession, String>>,
    last_used: Option<Instant>,
}

impl CcipSessionSlot {
    fn unload_if_idle(&mut self, now: Instant, timeout: Duration) -> bool {
        let idle = self
            .last_used
            .is_some_and(|last_used| now.saturating_duration_since(last_used) >= timeout);
        if !timeout.is_zero() && self.session.is_some() && idle {
            self.session = None;
            self.last_used = None;
            return true;
        }
        false
    }
}

fn session_slot() -> &'static Mutex<CcipSessionSlot> {
    static SLOT: OnceLock<Mutex<CcipSessionSlot>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(CcipSessionSlot::default()))
}

fn model_idle_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("CHARACTER_MODEL_IDLE_TIMEOUT_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(DEFAULT_MODEL_IDLE_TIMEOUT_SECONDS),
    )
}

fn start_session_idle_unloader() {
    static STARTED: OnceLock<()> = OnceLock::new();
    let timeout = model_idle_timeout();
    if timeout.is_zero() {
        return;
    }
    STARTED.get_or_init(|| {
        let check_interval = timeout.min(Duration::from_secs(60));
        if let Err(error) = std::thread::Builder::new()
            .name("gallery-ccip-idle-unloader".into())
            .spawn(move || loop {
                std::thread::sleep(check_interval);
                let mut slot = session_slot().lock().unwrap_or_else(|e| e.into_inner());
                if slot.unload_if_idle(Instant::now(), timeout) {
                    eprintln!(
                        "gallery-accel: unloaded character model after {} idle seconds",
                        timeout.as_secs()
                    );
                }
            })
        {
            eprintln!("gallery-accel: failed to start character model idle unloader: {error}");
        }
    });
}

fn requested_provider_raw() -> String {
    std::env::var("CHARACTER_RECOGNITION_PROVIDER").unwrap_or_else(|_| "auto".into())
}

fn provider_lower() -> String {
    requested_provider_raw().trim().to_ascii_lowercase()
}

fn want_cuda() -> bool {
    let p = provider_lower();
    matches!(
        p.as_str(),
        "auto" | "cuda" | "nvidia" | "cudaexecutionprovider"
    )
}

fn want_openvino() -> bool {
    let p = provider_lower();
    matches!(
        p.as_str(),
        "" | "auto" | "openvino" | "intel" | "gpu" | "openvinoexecutionprovider"
    )
}

fn force_cpu_only() -> bool {
    let p = provider_lower();
    matches!(p.as_str(), "cpu" | "cpuexecutionprovider")
}

fn openvino_device_type() -> String {
    std::env::var("CHARACTER_OPENVINO_DEVICE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "GPU".into())
}

fn openvino_cache_dir() -> Option<String> {
    std::env::var("CHARACTER_OPENVINO_CACHE_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn allow_cpu_fallback() -> bool {
    // New preferred: CHARACTER_ALLOW_CPU_FALLBACK.
    // Backward compat: CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK when the new var
    // is unset. When neither is set the default is to allow fallback.
    if std::env::var("CHARACTER_ALLOW_CPU_FALLBACK").is_ok() {
        env_bool("CHARACTER_ALLOW_CPU_FALLBACK", false)
    } else if std::env::var("CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK").is_ok() {
        env_bool("CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK", false)
    } else {
        true
    }
}

/// Resolve libonnxruntime.so for load-dynamic (next to binary, env, or system).
fn ort_dylib_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("ORT_DYLIB_PATH") {
        if !p.trim().is_empty() {
            out.push(PathBuf::from(p));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in [
                "libonnxruntime.so",
                "libonnxruntime.so.1",
                "libonnxruntime.so.1.24.1",
                "libonnxruntime.so.1.24.0",
            ] {
                out.push(dir.join(name));
            }
        }
    }
    out.push(PathBuf::from("libonnxruntime.so"));
    out
}

/// The ORT core this process is locked to, if already initialized.
pub fn ort_core_type() -> Option<&'static str> {
    match ORT_CORE.load(Ordering::SeqCst) {
        CORE_CPU => Some(OrtCoreType::Cpu.label()),
        CORE_OPENVINO => Some(OrtCoreType::OpenVino.label()),
        CORE_CUDA => Some(OrtCoreType::Cuda.label()),
        _ => None,
    }
}

/// One-time ONNX Runtime initialization. All CUDA/OpenVINO/CPU sessions go
/// through this single entry point so the process locks exactly one ORT core.
///
/// - CUDA: only when the prepared CUDA runtime is ready and the user wants
///   CUDA; the verified `libonnxruntime.so.1.24.1` from the cache is used.
/// - OpenVINO/CPU: the bundled/system ORT_DYLIB_PATH candidates are used.
/// - Once committed to a core it cannot be swapped; a later CUDA-ready state
///   is reported as `restart_required` by the runtime status instead.
fn ensure_ort_loaded() -> Result<(OrtCoreType, Option<String>)> {
    if let Some(core) = ort_core_type() {
        return Ok((core_label_to_type(core), None));
    }
    let _guard = ORT_INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(core) = ort_core_type() {
        return Ok((core_label_to_type(core), None));
    }
    let mut last_err = None;
    let want = provider_lower();
    let wants_cuda = matches!(
        want.as_str(),
        "auto" | "cuda" | "nvidia" | "cudaexecutionprovider"
    );
    // CUDA: only after the runtime preparation round marked the cached CUDA
    // runtime ready. This explicit init_from selection leaves the bundled
    // OpenVINO ORT_DYLIB_PATH untouched until this point.
    if wants_cuda {
        if let Some(path) = crate::runtime_prepare::cuda_ort_path_if_ready() {
            if path.is_file() {
                match ort::init_from(&path) {
                    Ok(builder) => {
                        if builder.commit() {
                            ORT_CORE.store(CORE_CUDA, Ordering::SeqCst);
                            return Ok((OrtCoreType::Cuda, last_err));
                        }
                        last_err = Some(format!("{}: commit returned false", path.display()));
                    }
                    Err(e) => {
                        last_err = Some(format!("{}: {e}", path.display()));
                    }
                }
            }
        }
    }
    // Bundled OpenVINO/system core.
    for cand in ort_dylib_candidates() {
        if !cand.is_file() {
            continue;
        }
        match ort::init_from(&cand) {
            Ok(builder) => {
                if builder.commit() {
                    ORT_CORE.store(CORE_OPENVINO, Ordering::SeqCst);
                    return Ok((OrtCoreType::OpenVino, last_err));
                }
                last_err = Some(format!("{}: commit returned false", cand.display()));
            }
            Err(e) => {
                last_err = Some(format!("{}: {e}", cand.display()));
            }
        }
    }
    // Fall back to the default loader (CPU-capable).
    if !ort::init().commit() {
        return Err(anyhow!(
            "ONNX Runtime initialization failed after all shared-library candidates"
        ));
    }
    ORT_CORE.store(CORE_CPU, Ordering::SeqCst);
    if last_err.is_some() {
        eprintln!(
            "gallery-accel: init_from candidates failed ({:?}); using ort::init()",
            last_err
        );
    }
    Ok((OrtCoreType::Cpu, last_err))
}

fn core_label_to_type(label: &str) -> OrtCoreType {
    match label {
        "cuda" => OrtCoreType::Cuda,
        "openvino" => OrtCoreType::OpenVino,
        _ => OrtCoreType::Cpu,
    }
}

/// Probe whether this process can open Intel render nodes for OpenVINO GPU.
pub fn gpu_access_probe() -> Value {
    let mut dri_nodes = Vec::new();
    let dri_dir = Path::new("/dev/dri");
    if dri_dir.is_dir() {
        if let Ok(rd) = std::fs::read_dir(dri_dir) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                let path = ent.path();
                let open_ok = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .is_ok()
                    || std::fs::File::open(&path).is_ok();
                dri_nodes.push(json!({
                    "name": name,
                    "path": path.display().to_string(),
                    "open_ok": open_ok,
                }));
            }
        }
    }
    let render_open_ok = dri_nodes.iter().any(|n| {
        n.get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.starts_with("renderD") || s.starts_with("card"))
            .unwrap_or(false)
            && n.get("open_ok").and_then(|v| v.as_bool()).unwrap_or(false)
    });
    // /proc/self/status: Groups: list of gids
    let mut groups_line = String::new();
    let mut uid_line = String::new();
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("Groups:") {
                groups_line = line.to_string();
            }
            if line.starts_with("Uid:") {
                uid_line = line.to_string();
            }
        }
    }
    let gid_names = {
        let mut names = Vec::new();
        if let Ok(txt) = std::fs::read_to_string("/etc/group") {
            let gids: Vec<u32> = groups_line
                .split_whitespace()
                .skip(1)
                .filter_map(|s| s.parse().ok())
                .collect();
            for line in txt.lines() {
                let mut parts = line.split(':');
                let name = parts.next().unwrap_or("");
                let _pw = parts.next();
                let gid: u32 = parts
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(u32::MAX);
                if gids.contains(&gid) {
                    names.push(name.to_string());
                }
            }
        }
        names
    };
    let has_render_group = gid_names.iter().any(|n| n == "render");
    let has_video_group = gid_names.iter().any(|n| n == "video");
    let ready = render_open_ok && !dri_nodes.is_empty();
    let message = if dri_nodes.is_empty() {
        "no /dev/dri nodes (no Intel GPU device nodes visible)"
    } else if !render_open_ok {
        "cannot open /dev/dri (missing render/video group or sg render at start)"
    } else if !has_render_group {
        "/dev/dri open ok but process not in render group (may still work if root)"
    } else {
        "render device open ok"
    };
    json!({
        "ready": ready,
        "message": message,
        "dri_nodes": dri_nodes,
        "process_groups": gid_names,
        "has_render_group": has_render_group,
        "has_video_group": has_video_group,
        "uid_status": uid_line,
        "groups_status": groups_line,
        "hint": "sudo usermod -aG render,video gallery; ensure cmd/main starts with `sg render`; restart Gallery",
    })
}

fn build_openvino_session(model_path: &Path) -> Result<CcipSession> {
    let device = openvino_device_type();
    let want_gpu = device.to_ascii_uppercase().contains("GPU");
    let access = gpu_access_probe();
    if want_gpu && access.get("ready").and_then(|v| v.as_bool()) != Some(true) {
        let msg = access
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("gpu access denied");
        return Err(anyhow!(
            "OpenVINO device_type={device} blocked: {msg}. access={}",
            access
        ));
    }
    let ov_ep = ep::OpenVINO::default();
    match ov_ep.is_available() {
        Ok(true) => {}
        Ok(false) => {
            return Err(anyhow!(
                "OpenVINOExecutionProvider not compiled into this libonnxruntime (need onnxruntime-openvino dylibs)"
            ));
        }
        Err(e) => {
            return Err(anyhow!("OpenVINO is_available check failed: {e}"));
        }
    }
    let mut ov = ov_ep.with_device_type(&device);
    if let Some(cache) = openvino_cache_dir() {
        ov = ov.with_cache_dir(cache);
    }
    // Match Python: disable ORT graph opts when using OpenVINO EP.
    // error_on_failure: if OpenVINO EP fails to register, do not silently use CPU.
    let mut builder = Session::builder()
        .map_err(|e| anyhow!("ort session builder: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Disable)
        .map_err(|e| anyhow!("ort opt level: {e}"))?
        .with_execution_providers([ov.build().error_on_failure()])
        .map_err(|e| anyhow!("register OpenVINO EP: {e}"))?;
    let session = builder
        .commit_from_file(model_path)
        .map_err(|e| anyhow!(
            "load onnx with OpenVINO ({device}): {e}. On fnOS: `sudo usermod -aG render,video gallery` and restart under `sg render`."
        ))?;
    let input_name = session
        .inputs()
        .first()
        .map(|i| i.name().to_string())
        .unwrap_or_else(|| "input".into());
    let active = if want_gpu {
        format!("gpu:0:openvino:{device}")
    } else {
        format!("openvino:{device}")
    };
    Ok(CcipSession {
        session,
        input_name,
        provider: "OpenVINOExecutionProvider".into(),
        active_device: active,
        fallback_reason: None,
    })
}

fn build_cpu_session(model_path: &Path) -> Result<CcipSession> {
    let mut builder = Session::builder()
        .map_err(|e| anyhow!("ort session builder: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(|e| anyhow!("ort opt level: {e}"))?
        .with_execution_providers([ep::CPU::default().build()])
        .map_err(|e| anyhow!("register CPU EP: {e}"))?;
    let session = builder
        .commit_from_file(model_path)
        .map_err(|e| anyhow!("load onnx with CPU: {e}"))?;
    let input_name = session
        .inputs()
        .first()
        .map(|i| i.name().to_string())
        .unwrap_or_else(|| "input".into());
    Ok(CcipSession {
        session,
        input_name,
        provider: "CPUExecutionProvider".into(),
        active_device: "cpu".into(),
        fallback_reason: None,
    })
}

fn build_cuda_session(model_path: &Path) -> Result<CcipSession> {
    let cuda_ep = ep::CUDA::default().with_device_id(0);
    if !cuda_ep.is_available()? {
        return Err(anyhow!(
            "CUDA EP not available (CUDA libs missing or no GPU)"
        ));
    }
    let mut builder = Session::builder()
        .map_err(|e| anyhow!("ort session builder: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(|e| anyhow!("ort opt level: {e}"))?
        .with_execution_providers([cuda_ep.build().error_on_failure()])
        .map_err(|e| anyhow!("register CUDA EP: {e}"))?;
    let session = builder
        .commit_from_file(model_path)
        .map_err(|e| anyhow!("load onnx with CUDA: {e}"))?;
    let input_name = session
        .inputs()
        .first()
        .map(|i| i.name().to_string())
        .unwrap_or_else(|| "input".into());
    Ok(CcipSession {
        session,
        input_name,
        provider: "CUDAExecutionProvider".into(),
        active_device: "cuda:0".into(),
        fallback_reason: None,
    })
}

fn append_fallback_reason(slot: &mut Option<String>, reason: String) {
    match slot {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&reason);
        }
        None => *slot = Some(reason),
    }
}

fn load_session() -> Result<&'static Mutex<CcipSessionSlot>> {
    let slot = session_slot();
    // Fast path: a session (or cached failure) is already recorded.
    {
        let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        if guard.session.is_some() {
            drop(guard);
            start_session_idle_unloader();
            return Ok(slot);
        }
    }
    // Slow path: build OUTSIDE the slot lock. Model/CUDA waits can run for
    // minutes; holding the lock here would stall /api/ml-runtime/status,
    // recognition reads, and the idle unloader for the whole wait.
    if SESSION_BUILDING.swap(true, Ordering::SeqCst) {
        // Another thread is already building; fall through once it finishes
        // and its result is visible in the slot.
        while SESSION_BUILDING.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
        start_session_idle_unloader();
        return Ok(slot);
    }
    let loaded = (|| -> Result<CcipSession> {
        // Give the background preparation round a chance to conclude
        // before deciding the provider (never blocks a running service).
        let path = character_model_path();
        if !path.is_file() {
            crate::runtime_prepare::wait_for_character_model();
        }
        if want_cuda() {
            crate::runtime_prepare::wait_for_cuda_runtime();
        }
        if !path.is_file() {
            return Err(anyhow!("model file missing: {}", path.display()));
        }
        if force_cpu_only() {
            return build_cpu_session(&path);
        }
        let (core, mut fallback_reason) = ensure_ort_loaded()?;
        let auto_provider = provider_lower() == "auto";
        // auto/cuda: CUDA session when the process is locked to the CUDA
        // core; failures fall back to CPU only (never OpenVINO on a CUDA
        // core).
        if want_cuda() && core == OrtCoreType::Cuda {
            match build_cuda_session(&path) {
                Ok(mut sess) => {
                    sess.fallback_reason = fallback_reason;
                    return Ok(sess);
                }
                Err(e) if allow_cpu_fallback() => {
                    append_fallback_reason(
                        &mut fallback_reason,
                        format!("CUDAExecutionProvider failed: {e}"),
                    );
                    eprintln!("gallery-accel: CUDA EP failed ({e}); falling back to CPU EP");
                }
                Err(e) => return Err(e),
            }
        }
        // auto/openvino: OpenVINO session on the bundled core. In `auto`
        // mode this is only attempted when an Intel GPU was detected, so
        // AMD-only machines never take the OpenVINO path.
        if want_openvino() && core != OrtCoreType::Cuda {
            let intel_gpu = crate::runtime_prepare::has_intel_gpu();
            if !auto_provider || intel_gpu {
                match build_openvino_session(&path) {
                    Ok(mut sess) => {
                        sess.fallback_reason = fallback_reason;
                        return Ok(sess);
                    }
                    Err(e) if allow_cpu_fallback() => {
                        append_fallback_reason(
                            &mut fallback_reason,
                            format!("OpenVINOExecutionProvider failed: {e}"),
                        );
                        eprintln!(
                            "gallery-accel: OpenVINO GPU failed ({e}); falling back to CPU EP"
                        );
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        let mut session = build_cpu_session(&path)?;
        session.fallback_reason = fallback_reason;
        Ok(session)
    })()
    .map_err(|e| e.to_string());
    SESSION_BUILDING.store(false, Ordering::SeqCst);
    if let Ok(ref sess) = loaded {
        let _ = ACTIVE_PROVIDER.set(sess.provider.clone());
        let _ = ACTIVE_DEVICE.set(sess.active_device.clone());
    }
    {
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        if guard.session.is_none() {
            guard.session = Some(loaded);
            guard.last_used = Some(Instant::now());
        }
    }
    start_session_idle_unloader();
    Ok(slot)
}

/// Drop any cached failed-session result so a successful preparation round
/// (or a user retry) can build a fresh session immediately instead of waiting
/// for the idle unload timeout. Successful sessions are kept untouched.
pub fn clear_failed_session_cache() {
    let mut guard = session_slot().lock().unwrap_or_else(|e| e.into_inner());
    if matches!(guard.session, Some(Err(_))) {
        guard.session = None;
        guard.last_used = None;
        eprintln!("gallery-accel: cleared cached failed character session");
    }
}

pub fn active_provider() -> &'static str {
    ACTIVE_PROVIDER
        .get()
        .map(|s| s.as_str())
        .unwrap_or("unknown")
}

pub fn active_device() -> &'static str {
    ACTIVE_DEVICE.get().map(|s| s.as_str()).unwrap_or("unknown")
}

fn with_session<F, T>(f: F) -> Result<T>
where
    F: FnOnce(&mut CcipSession) -> Result<T>,
{
    let slot = load_session()?;
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    let result = match guard.session.as_mut() {
        Some(Ok(sess)) => f(sess),
        Some(Err(msg)) => Err(anyhow!("{msg}")),
        None => Err(anyhow!("session not initialized")),
    };
    guard.last_used = Some(Instant::now());
    result
}

pub fn session_status() -> Value {
    if !env_bool("CHARACTER_RECOGNITION_ENABLED", true) {
        return json!({
            "session_loaded": false,
            "reason": "disabled",
            "actual_provider": "not_initialized",
        });
    }
    let path = character_model_path();
    if !path.is_file() {
        return json!({
            "session_loaded": false,
            "reason": "ccip_model_not_found",
            "actual_provider": "not_initialized",
            "model_path": path.display().to_string(),
            "requested_provider": requested_provider_raw(),
            "allow_cpu_fallback": allow_cpu_fallback(),
            "openvino_device": openvino_device_type(),
        });
    }
    let timeout = model_idle_timeout();
    let guard = session_slot().lock().unwrap_or_else(|e| e.into_inner());
    let idle_seconds = guard
        .last_used
        .map(|last_used| last_used.elapsed().as_secs());
    match guard.session.as_ref() {
        Some(Ok(session)) => json!({
            "session_loaded": true,
            "reason": "ready",
            "model_path": path.display().to_string(),
            "backend": "onnxruntime",
            "provider": session.provider,
            "actual_provider": session.provider,
            "active_device": session.active_device,
            "fallback_reason": session.fallback_reason,
            "requested_provider": requested_provider_raw(),
            "allow_cpu_fallback": allow_cpu_fallback(),
            "openvino_device": openvino_device_type(),
            "idle_seconds": idle_seconds,
            "idle_timeout_seconds": timeout.as_secs(),
        }),
        Some(Err(error)) => json!({
            "session_loaded": false,
            "reason": "session_load_failed",
            "actual_provider": "none",
            "error": error,
            "model_path": path.display().to_string(),
            "backend": "onnxruntime",
            "requested_provider": requested_provider_raw(),
            "allow_cpu_fallback": allow_cpu_fallback(),
            "openvino_device": openvino_device_type(),
            "idle_seconds": idle_seconds,
            "idle_timeout_seconds": timeout.as_secs(),
        }),
        None => {
            let preparing = SESSION_BUILDING.load(Ordering::SeqCst);
            json!({
                "session_loaded": false,
                "reason": if preparing { "preparing" } else { "idle_unloaded" },
                "actual_provider": "not_initialized",
                "model_path": path.display().to_string(),
                "backend": "onnxruntime",
                "requested_provider": requested_provider_raw(),
                "allow_cpu_fallback": allow_cpu_fallback(),
                "openvino_device": openvino_device_type(),
                "idle_timeout_seconds": timeout.as_secs(),
            })
        }
    }
}

fn l2_normalize(mut v: Vec<f32>) -> Result<Vec<f32>> {
    let mut norm = 0.0f32;
    for x in &v {
        norm += x * x;
    }
    norm = norm.sqrt();
    if norm <= 1e-12 {
        return Err(anyhow!("embedding has zero norm"));
    }
    for x in &mut v {
        *x /= norm;
    }
    Ok(v)
}

/// Prefer disk preview thumb when present; return (rgb path source label).
fn open_rgb_for_recognition(path: &Path) -> Result<(image::RgbImage, &'static str)> {
    if let Some(cache) = crate::image_preview::existing_preview_cache_file(path) {
        let img = image::open(&cache)
            .with_context(|| format!("open preview cache {}", cache.display()))?
            .to_rgb8();
        return Ok((img, "preview_cache"));
    }
    let img = image::open(path)
        .with_context(|| format!("open original image {}", path.display()))?
        .to_rgb8();
    Ok((img, "original"))
}

/// NCHW float32 /255 plane, shape (1, 3, 384, 384).
// The `channel * plane_size` idiom is kept explicit for all three channels;
// clippy flags the channel-0 `0 * h * w` term as an erasing operation.
#[allow(clippy::erasing_op, clippy::identity_op)]
fn preprocess_rgb(img: &image::RgbImage) -> Vec<f32> {
    let resized = image::imageops::resize(img, IMAGE_SIZE, IMAGE_SIZE, FilterType::CatmullRom);
    let h = IMAGE_SIZE as usize;
    let w = IMAGE_SIZE as usize;
    let mut data = vec![0.0f32; 1 * 3 * h * w];
    for y in 0..h {
        for x in 0..w {
            let p = resized.get_pixel(x as u32, y as u32).0;
            data[0 * h * w + y * w + x] = p[0] as f32 / 255.0;
            data[1 * h * w + y * w + x] = p[1] as f32 / 255.0;
            data[2 * h * w + y * w + x] = p[2] as f32 / 255.0;
        }
    }
    data
}

fn run_embedding(data: Vec<f32>) -> Result<Vec<f32>> {
    with_session(|sess| {
        let tensor = Tensor::from_array((
            [1usize, 3, IMAGE_SIZE as usize, IMAGE_SIZE as usize],
            data.into_boxed_slice(),
        ))
        .map_err(|e| anyhow!("tensor from array: {e}"))?;
        let outputs = sess
            .session
            .run(inputs![sess.input_name.as_str() => tensor])
            .map_err(|e| anyhow!("ccip session.run: {e}"))?;
        let extracted = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|e| anyhow!("extract embedding: {e}"))?;
        let flat: Vec<f32> = extracted.iter().copied().collect();
        if flat.len() < EMBEDDING_DIM {
            return Err(anyhow!(
                "unexpected embedding size {} (want {EMBEDDING_DIM})",
                flat.len()
            ));
        }
        let vec = if flat.len() == EMBEDDING_DIM {
            flat
        } else {
            flat[flat.len() - EMBEDDING_DIM..].to_vec()
        };
        l2_normalize(vec)
    })
}

/// Embed a local image file path via CCIP.
pub fn embed_image_path(path: &Path) -> Result<Vec<f32>> {
    let (rgb, _src) = open_rgb_for_recognition(path)?;
    run_embedding(preprocess_rgb(&rgb))
}

/// Embed + report whether preview cache was used.
pub fn embed_image_path_with_source(path: &Path) -> Result<(Vec<f32>, &'static str)> {
    let (rgb, src) = open_rgb_for_recognition(path)?;
    Ok((run_embedding(preprocess_rgb(&rgb))?, src))
}

/// Embed a gallery item (image via preview path, video via ffmpeg frame).
pub(crate) fn embed_item_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    item_id: i64,
) -> Result<(Vec<f32>, String, String, &'static str)> {
    let row = conn
        .query_row(
            "SELECT file_path, file_name, media_type, COALESCE(is_archive,0)
             FROM items WHERE id=?",
            params![item_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2).unwrap_or_default(),
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| anyhow!("item not found"))?;
    let (file_path, file_name, media_type, is_archive) = row;
    let media = if media_type.is_empty() {
        if is_archive != 0 {
            "archive".into()
        } else {
            "image".into()
        }
    } else {
        media_type
    };
    if is_archive != 0 || (media != "image" && media != "video") {
        return Err(anyhow!(
            "Item media type is not supported for character recognition"
        ));
    }
    let path = authorized_media_path(roots, &file_path)
        .map_err(|_| anyhow!("item media path is not allowed"))?;
    if media == "image" {
        let (emb, src) = embed_image_path_with_source(&path)?;
        return Ok((emb, file_path, file_name, src));
    }
    // video: extract a frame via ffmpeg to a temp jpeg in memory path
    let jpeg = extract_video_frame_jpeg(&path, 0.1)?;
    let tmp = std::env::temp_dir().join(format!(
        "gallery-ccip-{item_id}-{}.jpg",
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    use std::io::Write;
    file.write_all(&jpeg)?;
    drop(file);
    struct TemporaryFrame(PathBuf);
    impl Drop for TemporaryFrame {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = TemporaryFrame(tmp.clone());
    let emb = embed_image_path(&tmp);
    let emb = emb?;
    Ok((emb, file_path, file_name, "video_frame"))
}

/// Validate a 768-d non-zero embedding and pack little-endian blob.
pub(crate) fn pack_embedding_blob(vec: &[f32]) -> Result<Vec<u8>> {
    if vec.len() != EMBEDDING_DIM {
        return Err(anyhow!("embedding dim {} != {EMBEDDING_DIM}", vec.len()));
    }
    let mut sum_sq = 0.0f32;
    let mut out = Vec::with_capacity(EMBEDDING_DIM * 4);
    for &v in vec {
        if !v.is_finite() {
            return Err(anyhow!("embedding contains non-finite value"));
        }
        sum_sq += v * v;
        out.extend_from_slice(&v.to_le_bytes());
    }
    if sum_sq < 1e-12 {
        return Err(anyhow!("embedding is zero vector"));
    }
    Ok(out)
}

pub(crate) fn embedding_model_meta() -> (String, String, String) {
    (model_repo_id(), model_variant(), model_file())
}

pub(crate) const CCIP_EMBEDDING_DIM: usize = EMBEDDING_DIM;

fn extract_video_frame_jpeg(path: &Path, t: f64) -> Result<Vec<u8>> {
    use std::process::{Command, Stdio};
    // Bounded extraction: a broken/truncated video must not park the
    // recognition path indefinitely. The child runs on a helper thread with a
    // kill handle so the timeout actually stops ffmpeg.
    const FRAME_EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &format!("{t:.3}"),
            "-i",
            &path.to_string_lossy(),
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn ffmpeg")?;
    let mut stdout = child.stdout.take().context("ffmpeg stdout missing")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let read_result = std::io::Read::read_to_end(&mut stdout, &mut buf);
        let _ = sender.send((buf, read_result));
    });
    match receiver.recv_timeout(FRAME_EXTRACT_TIMEOUT) {
        Ok((buf, Ok(_read))) => {
            let status = child.wait().context("wait for ffmpeg")?;
            if !status.success() || buf.is_empty() {
                return Err(anyhow!("ffmpeg failed to extract video frame"));
            }
            Ok(buf)
        }
        Ok((_, Err(error))) => Err(anyhow!("read ffmpeg output: {error}")),
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(anyhow!("ffmpeg frame extraction timed out"))
        }
    }
}

fn parse_embedding_blob(blob: &[u8], dim: i64) -> Option<Vec<f32>> {
    if dim as usize != EMBEDDING_DIM {
        return None;
    }
    let need = EMBEDDING_DIM * 4;
    if blob.len() < need {
        return None;
    }
    let mut out = Vec::with_capacity(EMBEDDING_DIM);
    let mut sum_sq = 0.0f32;
    for i in 0..EMBEDDING_DIM {
        let start = i * 4;
        let bytes: [u8; 4] = blob[start..start + 4].try_into().ok()?;
        let v = f32::from_le_bytes(bytes);
        if !v.is_finite() {
            return None;
        }
        sum_sq += v * v;
        out.push(v);
    }
    if sum_sq < 1e-12 {
        return None;
    }
    // re-normalize for safety
    l2_normalize(out).ok()
}

#[derive(Clone)]
struct RefMeta {
    character_id: i64,
    character_name: String,
    reference_id: i64,
    item_id: Option<i64>,
    vector: Vec<f32>,
}

/// Parsed-reference cache: recognition used to re-read and re-parse every
/// embedded blob on each request. Keyed by a table fingerprint so imports,
/// cleanups, and restores self-invalidate without explicit hooks.
type ReferenceIndexCache = Mutex<Option<(String, Vec<RefMeta>)>>;

static REFERENCE_INDEX_CACHE: OnceLock<ReferenceIndexCache> = OnceLock::new();

fn reference_index_cache_slot() -> &'static ReferenceIndexCache {
    REFERENCE_INDEX_CACHE.get_or_init(|| Mutex::new(None))
}

/// Fingerprint covering inserts (MAX(id)), deletes (COUNT), and item_id
/// remaps (item_id aggregates). Embedding blobs are written together with a
/// new row id in this codebase, so no update-only mutation exists.
fn reference_table_signature(conn: &Connection) -> Result<String> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(id), 0),
                COALESCE(MAX(COALESCE(item_id, 0)), 0),
                COALESCE(SUM(COALESCE(item_id, 0)), 0)
         FROM character_references",
        [],
        |row| {
            Ok(format!(
                "{}:{}:{}:{}",
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?
            ))
        },
    )
    .map_err(Into::into)
}

fn cached_reference_index(conn: &Connection, signature: &str) -> Option<Vec<RefMeta>> {
    // Only file-backed databases are cached: tests share the process-wide
    // slot across distinct in-memory databases.
    let path = conn.path()?;
    let key = format!("{}|{}", path.replace('\\', "/"), signature);
    let guard = reference_index_cache_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .filter(|(cached_key, _)| *cached_key == key)
        .map(|(_, vectors)| vectors.clone())
}

fn store_reference_index(conn: &Connection, signature: &str, vectors: &[RefMeta]) {
    let Some(path) = conn.path() else {
        return;
    };
    let key = format!("{}|{}", path.replace('\\', "/"), signature);
    let mut guard = reference_index_cache_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *guard = Some((key, vectors.to_vec()));
}

fn load_index_vectors(conn: &Connection) -> Result<Vec<RefMeta>> {
    let signature = reference_table_signature(conn)?;
    if let Some(cached) = cached_reference_index(conn, &signature) {
        return Ok(cached);
    }
    let repo = model_repo_id();
    let variant = model_variant();
    let file = model_file();
    let mut stmt = conn.prepare(
        "SELECT cr.id, cr.character_id, c.name, cr.item_id, cr.embedding, cr.embedding_dim,
                cr.embedding_model_repo_id, cr.embedding_model_variant, cr.embedding_model_file
         FROM character_references cr
         JOIN characters c ON c.id = cr.character_id
         WHERE cr.embedding IS NOT NULL
           AND cr.embedding_dim = ?
           AND (
             (cr.embedding_model_variant = ? AND cr.embedding_model_file = ?)
             OR (cr.embedding_model_variant = ? AND (cr.embedding_model_file = '' OR cr.embedding_model_file IS NULL))
             OR (cr.embedding_model_repo_id = ? AND cr.embedding_model_variant = ?)
           )",
    )?;
    // Accept historical rows with matching variant (+ optional repo_id).
    let rows = stmt.query_map(
        params![EMBEDDING_DIM as i64, variant, file, variant, repo, variant],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (ref_id, char_id, name, item_id, blob, dim) = row?;
        if let Some(vector) = parse_embedding_blob(&blob, dim) {
            out.push(RefMeta {
                character_id: char_id,
                character_name: name,
                reference_id: ref_id,
                item_id,
                vector,
            });
        }
    }
    // Fallback: when the signature filter is empty, loading arbitrary 768-dim
    // rows would mix incompatible model spaces — a cross-model cosine can pass
    // the threshold and persist as accepted. Return an empty index instead
    // (matching character_cleanup's strict signature matching); an explicit
    // env switch restores the legacy permissive load.
    if out.is_empty() {
        if !env_bool("CHARACTER_EMBEDDING_LEGACY_FALLBACK", false) {
            return Ok(out);
        }
        let mut stmt = conn.prepare(
            "SELECT cr.id, cr.character_id, c.name, cr.item_id, cr.embedding, cr.embedding_dim
             FROM character_references cr
             JOIN characters c ON c.id = cr.character_id
             WHERE cr.embedding IS NOT NULL AND cr.embedding_dim = ?",
        )?;
        let rows = stmt.query_map(params![EMBEDDING_DIM as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        for row in rows {
            let (ref_id, char_id, name, item_id, blob, dim) = row?;
            if let Some(vector) = parse_embedding_blob(&blob, dim) {
                out.push(RefMeta {
                    character_id: char_id,
                    character_name: name,
                    reference_id: ref_id,
                    item_id,
                    vector,
                });
            }
        }
    }
    store_reference_index(conn, &signature, &out);
    Ok(out)
}

fn rank_characters(query: &[f32], refs: &[RefMeta], top_k: usize) -> Vec<Value> {
    let mut best: HashMap<i64, (f32, &RefMeta)> = HashMap::new();
    for r in refs {
        let mut score = 0.0f32;
        for (a, b) in query.iter().zip(r.vector.iter()) {
            score += a * b;
        }
        match best.get(&r.character_id) {
            Some((prev, _)) if *prev >= score => {}
            _ => {
                best.insert(r.character_id, (score, r));
            }
        }
    }
    let mut ranked: Vec<_> = best.into_values().collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(top_k);
    ranked
        .into_iter()
        .map(|(score, r)| {
            json!({
                "character_id": r.character_id,
                "character_name": r.character_name,
                "score": score,
                "matched_ref_id": r.reference_id,
                "matched_ref_item_id": r.item_id,
            })
        })
        .collect()
}

fn get_character(conn: &Connection, id: i64) -> Result<Option<Value>> {
    conn.query_row(
        "SELECT id, name, created_at FROM characters WHERE id=?",
        params![id],
        |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "created_at": r.get::<_, Option<f64>>(2)?,
            }))
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Full character recognition for one gallery item (Python `recognize_item` shape).
pub fn recognize_character_native(conn: &Connection, item_id: i64, top_k: i64) -> Result<Value> {
    recognize_character_native_with_roots(conn, &env_media_roots(), item_id, top_k)
}

pub fn recognize_character_native_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    item_id: i64,
    top_k: i64,
) -> Result<Value> {
    if !env_bool("CHARACTER_RECOGNITION_ENABLED", true) {
        return Ok(json!({
            "item_id": item_id,
            "status": "unavailable",
            "reason": "disabled",
            "character": null,
            "predictions": [],
            "backend": "rust-primary",
        }));
    }
    let top_k = top_k.clamp(1, 20) as usize;
    let started = Instant::now();
    let (query, _path, _name, image_source) = embed_item_with_roots(conn, roots, item_id)?;
    let refs = load_index_vectors(conn)?;
    let ref_count = refs.len();
    let decision = rank_characters(&query, &refs, top_k.max(2));
    let ranked: Vec<Value> = decision.iter().take(top_k).cloned().collect();
    let duration_ms = started.elapsed().as_millis() as u64;

    if decision.is_empty() {
        return Ok(json!({
            "item_id": item_id,
            "status": "unknown",
            "reason": "no_references",
            "character": null,
            "predictions": [],
            "runtime": {
                "backend": "onnxruntime",
                "provider": active_provider(),
                "active_device": active_device(),
                "duration_ms": duration_ms,
                "indexed_references": ref_count,
                "model_path": character_model_path().display().to_string(),
                "image_source": image_source,
            },
            "backend": "rust-primary",
        }));
    }

    let top_score = decision[0]["score"].as_f64().unwrap_or(0.0) as f32;
    let second_score = decision
        .get(1)
        .and_then(|v| v["score"].as_f64())
        .unwrap_or(0.0) as f32;
    let gap = top_score - second_score;
    let thr = threshold();
    let mg = min_gap();

    let (status, character_id, reason) = if top_score >= thr && gap >= mg {
        (
            "accepted",
            decision[0]["character_id"].as_i64(),
            String::new(),
        )
    } else if top_score >= thr * 0.8 {
        (
            "needs_review",
            decision[0]["character_id"].as_i64(),
            if gap < mg {
                "low_gap".into()
            } else {
                "low_score".into()
            },
        )
    } else {
        ("unknown", None, "below_threshold".into())
    };

    let character = if status == "accepted" {
        character_id
            .map(|id| get_character(conn, id))
            .transpose()?
            .flatten()
    } else if status == "needs_review" {
        character_id
            .map(|id| get_character(conn, id))
            .transpose()?
            .flatten()
    } else {
        None
    };

    // Best-effort persist result for UI/history.
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS character_recognition_results (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            item_id INTEGER NOT NULL UNIQUE,
            character_id INTEGER,
            status TEXT NOT NULL,
            top_score REAL,
            second_score REAL,
            gap REAL,
            threshold REAL,
            reference_count INTEGER NOT NULL DEFAULT 0,
            checked_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            error TEXT NOT NULL DEFAULT ''
        )",
        [],
    );
    let _ = conn.execute(
        "INSERT INTO character_recognition_results
         (item_id, character_id, status, top_score, second_score, gap, threshold, reference_count, checked_at, error)
         VALUES (?,?,?,?,?,?,?,?,strftime('%s','now'),?)
         ON CONFLICT(item_id) DO UPDATE SET
           character_id=excluded.character_id,
           status=excluded.status,
           top_score=excluded.top_score,
           second_score=excluded.second_score,
           gap=excluded.gap,
           threshold=excluded.threshold,
           reference_count=excluded.reference_count,
           checked_at=excluded.checked_at,
           error=excluded.error",
        params![
            item_id,
            if status == "accepted" {
                character_id
            } else {
                None
            },
            status,
            top_score as f64,
            second_score as f64,
            gap as f64,
            thr as f64,
            ref_count as i64,
            reason,
        ],
    );

    Ok(json!({
        "item_id": item_id,
        "status": status,
        "reason": reason,
        "character": character,
        "predictions": ranked,
        "runtime": {
            "backend": "onnxruntime",
            "duration_ms": duration_ms,
            "indexed_references": ref_count,
            "model_path": character_model_path().display().to_string(),
            "provider": active_provider(),
            "active_device": active_device(),
            "threshold": thr,
            "min_gap": mg,
            "image_source": image_source,
        },
        "backend": "rust-primary",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_normalize_unit() {
        let v = l2_normalize(vec![3.0, 4.0]).unwrap();
        assert!((v[0] - 0.6).abs() < 1e-5);
        assert!((v[1] - 0.8).abs() < 1e-5);
    }

    #[test]
    fn session_slot_unloads_only_after_idle_timeout() {
        let now = Instant::now();
        let mut slot = CcipSessionSlot {
            session: Some(Err("test session".into())),
            last_used: Some(now - Duration::from_secs(10)),
        };
        assert!(!slot.unload_if_idle(now, Duration::from_secs(11)));
        assert!(slot.unload_if_idle(now, Duration::from_secs(10)));
        assert!(slot.session.is_none());
    }

    #[test]
    fn cpu_fallback_allowed_by_default_and_prefers_new_var() {
        let new_key = "CHARACTER_ALLOW_CPU_FALLBACK";
        let old_key = "CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK";
        let previous_new = std::env::var(new_key).ok();
        let previous_old = std::env::var(old_key).ok();
        std::env::remove_var(new_key);
        std::env::remove_var(old_key);
        // Neither variable set -> default allows CPU fallback.
        assert!(allow_cpu_fallback());
        std::env::set_var(new_key, "0");
        std::env::set_var(old_key, "1");
        assert!(!allow_cpu_fallback());
        std::env::remove_var(new_key);
        std::env::set_var(old_key, "0");
        assert!(!allow_cpu_fallback());
        if let Some(value) = previous_new {
            std::env::set_var(new_key, value);
        } else {
            std::env::remove_var(new_key);
        }
        if let Some(value) = previous_old {
            std::env::set_var(old_key, value);
        } else {
            std::env::remove_var(old_key);
        }
    }

    #[test]
    fn provider_aliases_map_gpu_to_openvino_not_cuda() {
        let previous = std::env::var("CHARACTER_RECOGNITION_PROVIDER").ok();
        for (raw, cuda, openvino) in [
            ("auto", true, true),
            ("cuda", true, false),
            ("nvidia", true, false),
            ("cudaexecutionprovider", true, false),
            ("openvino", false, true),
            ("intel", false, true),
            ("gpu", false, true),
            ("openvinoexecutionprovider", false, true),
            ("cpu", false, false),
            ("cpuexecutionprovider", false, false),
        ] {
            std::env::set_var("CHARACTER_RECOGNITION_PROVIDER", raw);
            assert_eq!(want_cuda(), cuda, "want_cuda({raw})");
            assert_eq!(want_openvino(), openvino, "want_openvino({raw})");
        }
        std::env::set_var("CHARACTER_RECOGNITION_PROVIDER", "");
        assert!(want_openvino());
        match previous {
            Some(value) => std::env::set_var("CHARACTER_RECOGNITION_PROVIDER", value),
            None => std::env::remove_var("CHARACTER_RECOGNITION_PROVIDER"),
        }
    }

    #[test]
    fn clear_failed_session_cache_removes_error_state_only() {
        let slot = session_slot();
        {
            let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            guard.session = Some(Err("previous failure".into()));
            guard.last_used = Some(Instant::now());
        }
        clear_failed_session_cache();
        {
            let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            assert!(guard.session.is_none(), "failed cache must be cleared");
            assert!(guard.last_used.is_none());
        }
        // An empty slot stays untouched (no successful session to unload).
        clear_failed_session_cache();
        {
            let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            assert!(guard.session.is_none());
        }
    }

    #[test]
    fn pack_embedding_rejects_wrong_dim_and_zero() {
        assert!(pack_embedding_blob(&[1.0, 0.0]).is_err());
        assert!(pack_embedding_blob(&vec![0.0f32; EMBEDDING_DIM]).is_err());
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[0] = 1.0;
        let blob = pack_embedding_blob(&v).unwrap();
        assert_eq!(blob.len(), EMBEDDING_DIM * 4);
        assert!(parse_embedding_blob(&blob, EMBEDDING_DIM as i64).is_some());
    }

    #[test]
    fn rank_picks_best_character() {
        let q = vec![1.0, 0.0, 0.0];
        let refs = vec![
            RefMeta {
                character_id: 1,
                character_name: "A".into(),
                reference_id: 10,
                item_id: Some(1),
                vector: vec![0.9, 0.1, 0.0],
            },
            RefMeta {
                character_id: 2,
                character_name: "B".into(),
                reference_id: 20,
                item_id: Some(2),
                vector: vec![0.1, 0.9, 0.0],
            },
            RefMeta {
                character_id: 1,
                character_name: "A".into(),
                reference_id: 11,
                item_id: Some(3),
                vector: vec![1.0, 0.0, 0.0],
            },
        ];
        // normalize query-like vectors roughly by using as-is for unit test
        let ranked = rank_characters(&q, &refs, 2);
        assert_eq!(ranked[0]["character_id"], 1);
        assert_eq!(ranked[0]["matched_ref_id"], 11);
    }
}
