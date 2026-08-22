<div align="center">

# 🖼️ Gallery

**High-Performance, Lightweight Self-Hosted Media Library & Digital Asset Manager Written in Pure Rust**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS (Native FPK)](https://img.shields.io/badge/Platform-fnOS%20FPK-brightgreen.svg)](#-deployment)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#-project-structure)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#-key-features)

[English](README.en.md) • [简体中文](README.md)

[✨ Features](#-key-features) • [🚀 Deployment](#-deployment) • [⚙️ Configuration](#️-configuration) • [🛠️ Building](#️-building--packaging) • [📂 Structure](#-project-structure) • [📄 License](#-license)

</div>

---

## 📖 Introduction

**Gallery** is a self-hosted media library management system for local creative asset collections. The backend is written entirely in pure Rust (Axum + rusqlite + Tokio) with a responsive single-page web UI; it organizes media by artist (directories define artists) with multi-dimensional tags, AI character recognition, BLAKE3 content-hash deduplication, and standardized folder archiving.

---

## ✨ Key Features

### 1. 🎬 Multi-Format Browsing & Smooth Playback
- Images (JPG/PNG/WebP/AVIF/BMP), inline GIF hover preview, and in-browser video streaming (MP4/WebM/MOV/MKV via ffmpeg/HLS); plus .txt/.md/.html text and PSD/CLIP/PSB/ZIP/RAR sources.
- Immersive lightbox: full-screen zoom, drag-pan, mobile pinch; ←/→ navigate, Esc close, download/favorite/delete shortcuts.
- URL-synced state: artist, folder, tags, date, sort, and search are written to the address bar for back/forward and sharing.

### 2. 🎨 Artist Partitioning & Multi-Dimensional Tagging
- Artist as top-level boundary (folders define artists); filter inside by folder tree and multi-tag combinations.
- Batch tagging: single/marquee/whole-folder add or remove; tags sort by default/name/count with pinyin search for artists and tags.
- Link indexing: auto-extract text/cloud links and passcodes; manage external profiles (Pixiv, Fanbox, Patreon, Twitter/X, Bilibili).

### 3. 🧠 AI Character Recognition (CCIP & OpenVINO)
- CCIP recognition on by default; backend auto-selects CUDA → OpenVINO → CPU and auto-downloads models and CUDA runtime; falls back to CPU when GPU unavailable.
- Single-character reference libraries capture varied styles; built-in semantic deduplication (≥0.95) and outlier pruning.
- Human-in-the-loop: AI suggests only in Edit Mode and applies only after your confirmation.

### 4. 🔍 Content-Hash Deduplication & Path Tracking
- BLAKE3 fingerprinting locates duplicates with sidebar group views.
- Renamed/moved files re-link via hash and inode; same-artist moves keep tags, cross-artist conflicts route to Maintenance review.

### 5. 📁 Standardized Folder Archiving & Rule Engine
- Editable Default template (initial {year}/{date} {tags}); preview before execution.
- Safety: automatic SQLite online backup before any move; previews and revalidates source/target/authorization, never overwrites occupied targets.
- Execution records: Current successful plans are deleted in the same transaction and cannot be undone.
- Controlled auto-archive: Maintenance "Auto Organize" off by default; runs only after a successful full scan; single-artist/folder scans never trigger.

### 6. 🛡️ Storage Safety & Isolated State
- Metadata and indexes in SQLite; media stays within authorized directories; organization never crosses boundaries.
- Safe recycle: deletes prefer the system recycle bin, falling back to gallery/data/recycle, with one-click restore.

> [!WARNING]
> **Network Security Notice**: Gallery listens on port `8899` by default and **provides no built-in user authentication**. It is intended strictly for trusted local area networks (LAN). When exposing the service to public networks, configure authentication via fnOS or an upstream reverse proxy (such as Nginx or Caddy).

---

## 🚀 Deployment

### Method 1: fnOS Native FPK Package (Recommended)

Gallery offers an optimized native FPK package for fnOS, delivering native performance with minimal memory overhead.

1. **Download FPK**: Obtain the latest `gallery_<version>_x86_64.fpk` from [Releases](https://github.com/h-void/gallery/releases).
2. **Install via App Center**: Open the fnOS App Center, choose manual installation, and select the `.fpk` file.
3. **Authorize Media Directories**: Grant access to the media shares you want Gallery to manage.
4. **Persistent Data Layout**: fnOS creates a persistent `@appshare/gallery` directory structured as:
   ```text
   @appshare/gallery/
   ├── data/      # Main database gallery.db, runtime logs logs/, online backups db-backups/, recycle/
   ├── cache/     # Preview thumbnails transcode-cache/ and video segments
   └── models/    # AI models and OpenVINO runtime cache files
   ```
5. **Install AI Character Model**: The CCIP model downloads automatically in the background after startup; for offline installs, download it manually and place it at (create the directory if needed):
   ```text
   gallery/models/character/ccip-caformer_b36-24/model_feat.onnx
   ```
6. **Launch**: Access `http://<NAS_IP>:8899/` and click "Scan All" to start the initial index.

---

### Method 2: Docker / Docker Compose (Under Continuous Testing)

> ⚠️ **For production, prefer Method 1 (fnOS Native FPK).** The Docker path remains under active testing and maintenance; validate the current `Dockerfile` and Compose configuration before deployment.

1. **Prerequisites**: Install Docker Desktop or Docker Engine.
2. **Start the Stack**: Set your media directory at the project root and start:

```powershell
$env:GALLERY_MEDIA_DIR = 'D:\Pictures'
docker compose up -d
```

When no local image exists, Compose pulls the prebuilt `stable` image from the registry automatically — no local compilation. Add `--build` only after modifying the source; upgrade to a newly published image with `docker compose pull` followed by `docker compose up -d`.

For a normal NAS or Windows deployment, use only the default `gallery` service; it does not require `/dev/dri` or GPU permissions. `gallery-gpu` is optional and does not affect the normal path.

**GPU support matrix**:

| Host environment | Available option | Host requirement |
| :--- | :--- | :--- |
| Native Linux + NVIDIA | `cuda` profile | NVIDIA driver + NVIDIA Container Toolkit |
| Windows Docker Desktop + NVIDIA | `cuda` profile | NVIDIA driver only (WSL2 support is built into the driver) |
| Native Linux + Intel iGPU | `gpu` profile | `/dev/dri` passthrough with correct group GIDs |
| Windows with only an Intel iGPU | No GPU support | Use the default `gallery` (CPU inference) |

Intel GPU uses the optional profile in the same Compose file. Stop the default service first, then start the GPU service explicitly so both services do not claim the same port:

```powershell
docker compose stop gallery
docker compose --profile gpu up -d --build gallery-gpu
```

Common runtime settings are listed in the `environment` block of `docker-compose.yml`. Edit the YAML directly, or override the same variable names from a project `.env` file or the host environment, for example `CHARACTER_RECOGNITION_PROVIDER=cpu` or `SCAN_INTERVAL=0`. `DATA_DIR` stays fixed at `/gallery/data`; the recycle store is `/gallery/data/recycle` and persists in `gallery-storage`.

**Docker GPU note**: The `gallery-gpu` profile targets an Intel iGPU on a Linux Docker host. It maps the host `/dev/dri` device nodes into the container and adds the process to the Linux `render`/`video` groups that own those nodes. `GALLERY_RENDER_GID` and `GALLERY_VIDEO_GID` are numeric group IDs, not GPU model settings. They vary by host: run `stat -c '%n gid=%g' /dev/dri/renderD128 /dev/dri/card0` on the Linux host, then put the values in the project `.env`, for example `GALLERY_RENDER_GID=109` and `GALLERY_VIDEO_GID=44`. Windows Docker Desktop (WSL2 backend) has no `/dev/dri` device, so Intel iGPUs are not usable in that environment; the `gallery-gpu` profile applies to native Linux hosts only. On a Windows machine with only an Intel iGPU, use the default `gallery` service (CPU inference).

**Docker CUDA note**: NVIDIA hosts use the `cuda` profile in the same Compose file; no GPU index or GID needs to be entered. The host needs NVIDIA Container Toolkit (Windows Docker Desktop needs working WSL2 NVIDIA support):

```powershell
docker compose stop gallery
docker compose --profile cuda up -d --build gallery-cuda
```

Compose exposes the NVIDIA GPU to the container. `gallery-cuda` uses a dedicated `runtime-cuda` image target that bundles the CUDA 12.x and cuDNN 9.x user-mode libraries, so the host only needs a compatible NVIDIA driver — no CUDA Toolkit installation. The image bundles only the five NVIDIA libraries the ONNX Runtime CUDA provider actually links (cudart, cublas, cuDNN, cufft, curand). `Dockerfile.cuda` in the repository root matches `Dockerfile` stage for stage with the final target swapped, for CI services that always build the last stage. GPU detection supports both native `/dev/nvidia*` nodes and the WSL2/Docker Desktop `/dev/dxg` plus injected-driver layout. Gallery then follows the same fnOS order, CUDA → OpenVINO → CPU, and downloads the verified ONNX Runtime CUDA runtime into `gallery-storage` at `models/ort/cuda-1.24.1`. The registry publishes a `stable-cuda` tag alongside the source; once that tag is available, the commands above can drop `--build` and pull the prebuilt image directly. Without an NVIDIA GPU, do not start this profile; use the default `gallery` service.

**fnOS parameter note**: The current FPK does not expose a generic environment-variable editor, and Docker `.env` values do not carry over to FPK. fnOS injects the `TRIM_*` path and port variables. To change other defaults such as the provider, scan interval, or backup interval, edit `fnpack/cmd/main`, run `tools/build_rust_accel.py` and `tools/build_fnpack.py` again, install the new FPK, and restart Gallery.

3. **Notes**:
   - Databases, caches, and models are persisted in the `gallery-storage` volume (`data/`, `cache/`, `models/`).
   - Media is mounted read-only (`:ro`) by default; the app only reads originals and organizes within its own index/database — it does **not physically move or rename your files**.
   - Place the model at `models/character/ccip-caformer_b36-24/model_feat.onnx`; for managing media on fnOS, use the authorized-directory flow in Method 1.

---

## ⚙️ Configuration

The following runtime settings can be overridden in Compose (`environment`, project `.env`, or host environment). The fnOS FPK has no generic variable editor; use the rebuild flow above for fnOS:

| Variable | Default | Description |
| :--- | :--- | :--- |
| `SERVICE_PORT` | `8899` | HTTP service listen port. |
| `DATA_DIR` | `data` | Data persistence directory (gallery.db, logs, backups). |
| `IMAGE_PREVIEW_CACHE_DIR` | `cache` | Thumbnail and transcode-cache directory. |
| `CHARACTER_RECOGNITION_ENABLED` | `1` | Enable AI character recognition (1 on / 0 off). |
| `CHARACTER_RECOGNITION_PROVIDER` | `auto` | Inference backend auto-selection: CUDA → OpenVINO → CPU. |
| `CHARACTER_ALLOW_CPU_FALLBACK` | `1` | Fall back to CPU when GPU init fails (0 disables; legacy `CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK` still honored). |
| `ONNXRUNTIME_AUTO_DOWNLOAD` | `1` | Auto-download missing CCIP model and CUDA runtime in the background after startup (0 disables, for offline installs). |
| `CHARACTER_MODEL_IDLE_TIMEOUT_SECONDS` | `600` | Idle timeout in seconds before unloading the model (0 keeps resident). |
| `SCAN_INTERVAL` | `21600` | Background full-scan interval in seconds (default 6 hours; 0 disables). |
| `HASH_INTERVAL` | `30` | Background content-hash poll interval in seconds. |
| `DB_BACKUP_INTERVAL` | `43200` | Database auto-backup interval in seconds (default 12 hours). |
| `DB_BACKUP_RETENTION` | `8` | Maximum number of retained backup snapshots. |

---

## 🛠️ Building & Packaging

### 1. Prerequisites for Release Builds (Windows)
- **WSL 2** with **Podman** installed (build scripts cross-compile the Linux binary inside an isolated Debian Bookworm container).
- Official `fnpack` packaging binary (available at `output/fnpack/fnpack-1.2.3-windows-amd64.exe`).

### 2. Full Release Build (Compile Rust & Package FPK)
In Windows PowerShell, run the release pipeline:

```powershell
python tools/build_release.py --fnpack .\output\fnpack\fnpack-1.2.3-windows-amd64.exe
```

### 3. Compile Linux Rust Runtime Only
To compile the Linux Rust binary for fnOS without packaging:

```powershell
python tools/build_rust_accel.py
```

The compiled binary will be placed at `app/bin/gallery-accel`.

---

## 📂 Project Structure

```text
gallery/
├── rust/gallery_accel/   # Core Rust runtime: Axum HTTP API, SQLite connection pool, background scan/hash workers, CCIP engine
├── app/static/           # Single-page web application: HTML5 shell, core design system (style.css), and modular JS scripts
├── fnpack/               # fnOS native package definition (package.json), lifecycle entry scripts (cmd/main), and permissions
├── Dockerfile            # Pure-Rust Docker image build (under continuous testing; default target is the plain runtime)
├── Dockerfile.cuda       # CUDA image variant (matches Dockerfile with the final target swapped, for CI last-stage builds)
├── docker-compose.yml    # Docker Compose config (under continuous testing; production prefers fnOS FPK)
└── tools/                # Automation scripts for FPK packaging, Rust compilation, work logging, and public repository generation
```

---

## 📄 License

This project is licensed under the [GNU General Public License v3.0 only](LICENSE) (`GPL-3.0-only`).
Copyright (C) 2026 h-void.

Redistributing this project or modified derivatives requires providing corresponding source code under GPL-3.0-only.

## 📜 Third-Party Licenses

Bundled license notices for ONNX Runtime, OpenVINO, and third-party Rust crates in FPK and Docker distributions are available in `fnpack/app/licenses/`.
