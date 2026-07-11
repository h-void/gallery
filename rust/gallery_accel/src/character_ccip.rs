//! Character CCIP embedding + recognition (pure Rust, official ONNX Runtime).
//!
//! Preprocess matches Python `embedding.py`:
//! RGB → resize 384×384 bicubic → NCHW float32 /255 → session → L2 normalize (dim 768).
//! Prefer existing `IMAGE_PREVIEW_CACHE_DIR` thumbs (512) over decoding the full original.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use image::imageops::FilterType;
use ort::ep::{self, ExecutionProvider};
use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

const IMAGE_SIZE: u32 = 384;
const EMBEDDING_DIM: usize = 768;
const DEFAULT_THRESHOLD: f32 = 0.23;
const DEFAULT_MIN_GAP: f32 = 0.04;
static ORT_INIT: AtomicBool = AtomicBool::new(false);
static ACTIVE_PROVIDER: OnceLock<String> = OnceLock::new();
static ACTIVE_DEVICE: OnceLock<String> = OnceLock::new();

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
}

fn session_slot() -> &'static Mutex<Option<Result<CcipSession, String>>> {
    static SLOT: OnceLock<Mutex<Option<Result<CcipSession, String>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn requested_provider_raw() -> String {
    std::env::var("CHARACTER_RECOGNITION_PROVIDER").unwrap_or_else(|_| "openvino".into())
}

fn want_openvino() -> bool {
    let raw = requested_provider_raw();
    let lowered = raw.trim().to_ascii_lowercase();
    matches!(
        lowered.as_str(),
        "" | "auto" | "openvino" | "gpu" | "openvinoexecutionprovider"
    )
}

fn force_cpu_only() -> bool {
    let lowered = requested_provider_raw().trim().to_ascii_lowercase();
    matches!(lowered.as_str(), "cpu" | "cpuexecutionprovider")
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

fn allow_cpu_fallback() -> bool {
    // Keep a missing render/video group visible instead of silently consuming CPU.
    env_bool("CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK", false)
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

fn ensure_ort_loaded() -> Result<()> {
    if ORT_INIT.load(Ordering::SeqCst) {
        return Ok(());
    }
    // Prefer explicit path so manylinux ORT ships with the FPK.
    // init_from returns Result<EnvironmentBuilder>; commit() applies settings (bool).
    let mut last_err = None;
    for cand in ort_dylib_candidates() {
        if !cand.is_file() {
            continue;
        }
        match ort::init_from(&cand) {
            Ok(builder) => {
                let _ = builder.commit();
                ORT_INIT.store(true, Ordering::SeqCst);
                return Ok(());
            }
            Err(e) => last_err = Some(format!("{}: {e}", cand.display())),
        }
    }
    // Fall back to default loader (ORT_DYLIB_PATH / LD_LIBRARY_PATH / system).
    let _ = ort::init().commit();
    ORT_INIT.store(true, Ordering::SeqCst);
    if last_err.is_some() {
        eprintln!(
            "gallery-accel: init_from candidates failed ({:?}); using ort::init()",
            last_err
        );
    }
    Ok(())
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
    })
}

fn load_session() -> Result<&'static Mutex<Option<Result<CcipSession, String>>>> {
    let slot = session_slot();
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let path = character_model_path();
        let loaded = (|| -> Result<CcipSession> {
            ensure_ort_loaded()?;
            if !path.is_file() {
                return Err(anyhow!("model file missing: {}", path.display()));
            }
            if force_cpu_only() {
                return build_cpu_session(&path);
            }
            if want_openvino() {
                match build_openvino_session(&path) {
                    Ok(sess) => return Ok(sess),
                    Err(e) if allow_cpu_fallback() => {
                        eprintln!(
                            "gallery-accel: OpenVINO GPU failed ({e}); falling back to CPU EP"
                        );
                    }
                    Err(e) => return Err(e),
                }
            }
            build_cpu_session(&path)
        })()
        .map_err(|e| e.to_string());
        if let Ok(ref sess) = loaded {
            let _ = ACTIVE_PROVIDER.set(sess.provider.clone());
            let _ = ACTIVE_DEVICE.set(sess.active_device.clone());
        }
        *guard = Some(loaded);
    }
    drop(guard);
    Ok(slot)
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
    match guard.as_mut() {
        Some(Ok(sess)) => f(sess),
        Some(Err(msg)) => Err(anyhow!("{msg}")),
        None => Err(anyhow!("session not initialized")),
    }
}

pub fn session_status() -> Value {
    if !env_bool("CHARACTER_RECOGNITION_ENABLED", true) {
        return json!({"session_loaded": false, "reason": "disabled"});
    }
    let path = character_model_path();
    if !path.is_file() {
        return json!({
            "session_loaded": false,
            "reason": "ccip_model_not_found",
            "model_path": path.display().to_string(),
        });
    }
    match with_session(|_| Ok(())) {
        Ok(()) => json!({
            "session_loaded": true,
            "reason": "ready",
            "model_path": path.display().to_string(),
            "backend": "onnxruntime-openvino",
            "provider": active_provider(),
            "active_device": active_device(),
            "requested_provider": requested_provider_raw(),
            "openvino_device": openvino_device_type(),
        }),
        Err(e) => json!({
            "session_loaded": false,
            "reason": "session_load_failed",
            "error": e.to_string(),
            "model_path": path.display().to_string(),
            "backend": "onnxruntime-openvino",
            "requested_provider": requested_provider_raw(),
            "openvino_device": openvino_device_type(),
        }),
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
pub(crate) fn embed_item(
    conn: &Connection,
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
    let path = PathBuf::from(&file_path);
    if media == "image" {
        let (emb, src) = embed_image_path_with_source(&path)?;
        return Ok((emb, file_path, file_name, src));
    }
    // video: extract a frame via ffmpeg to a temp jpeg in memory path
    let jpeg = extract_video_frame_jpeg(&path, 0.1)?;
    let tmp = std::env::temp_dir().join(format!("gallery-ccip-{item_id}.jpg"));
    std::fs::write(&tmp, &jpeg)?;
    let emb = embed_image_path(&tmp);
    let _ = std::fs::remove_file(&tmp);
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
    let output = Command::new("ffmpeg")
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
        .output()
        .context("spawn ffmpeg")?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(anyhow!("ffmpeg failed to extract video frame"));
    }
    Ok(output.stdout)
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

struct RefMeta {
    character_id: i64,
    character_name: String,
    reference_id: i64,
    item_id: Option<i64>,
    vector: Vec<f32>,
}

fn load_index_vectors(conn: &Connection) -> Result<Vec<RefMeta>> {
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
    // Fallback: if signature filter empty, use any 768-d non-zero embedding.
    if out.is_empty() {
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
    let (query, _path, _name, image_source) = embed_item(conn, item_id)?;
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
                "backend": "onnxruntime-openvino",
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
            "backend": "onnxruntime-openvino",
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
    fn cpu_fallback_is_disabled_by_default() {
        let key = "CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK";
        let previous = std::env::var(key).ok();
        std::env::remove_var(key);
        assert!(!allow_cpu_fallback());
        if let Some(value) = previous {
            std::env::set_var(key, value);
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
