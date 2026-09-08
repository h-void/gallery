<div align="center">

# Gallery

**Self-Hosted Media Library & Digital Asset Manager Written in Pure Rust**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS (Native FPK)](https://img.shields.io/badge/Platform-fnOS%20FPK-brightgreen.svg)](#deployment)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#project-structure)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#key-features)

[English](README.en.md) • [简体中文](README.md)

[Features](#key-features) • [Deployment](#deployment) • [Configuration](#configuration) • [Building](#building--packaging) • [Structure](#project-structure) • [License](#license)

</div>

---

## Introduction

**Gallery** is a self-hosted media library management system for local creative asset collections. The backend is written entirely in pure Rust (Axum + rusqlite + Tokio) with a responsive single-page web UI; it organizes media by artist (directories define artists) with multi-dimensional tags, AI character recognition, BLAKE3 content-hash deduplication, and standardized folder archiving.

---

## Key Features

### 1. Multi-Format Browsing & Smooth Playback
- Images (JPG/PNG/WebP/AVIF/BMP), inline GIF hover preview, and in-browser video streaming (MP4/WebM/MOV/MKV via ffmpeg/HLS); plus .txt/.md/.html text and PSD/CLIP/PSB/ZIP/RAR sources.
- Card ratio option: the maintenance Status page offers 4:3 full (default) and 3:4 portrait grid thumbnails; the choice is stored per browser.
- Lightbox: full-screen zoom, drag-pan, mobile pinch; ←/→ navigate, Esc close, download/favorite/delete shortcuts.
- URL-synced state: artist, folder, tags, date, sort, and search are written to the address bar for back/forward and sharing.

### 2. Artist Partitioning & Multi-Dimensional Tagging
- Artist as top-level boundary (folders define artists); filter inside by folder tree and multi-tag combinations.
- Batch tagging: single/marquee/whole-folder add or remove; tags sort by default/name/count with pinyin search for artists and tags.
- Link indexing: auto-extract text/cloud links and passcodes; manage external profiles (Pixiv, Fanbox, Patreon, Twitter/X, Bilibili).

### 3. AI Character Recognition (CCIP & OpenVINO)
- CCIP recognition on by default; backend auto-selects CUDA → OpenVINO → CPU and auto-downloads models and CUDA runtime; falls back to CPU when GPU unavailable.
- Single-character reference libraries capture varied styles; built-in semantic deduplication (≥0.95) and outlier pruning.
- Human-in-the-loop: AI suggests only in Edit Mode and applies only after your confirmation.

### 4. Content-Hash Deduplication & Path Tracking
- BLAKE3 fingerprinting locates duplicates with sidebar group views.
- Renamed/moved files re-link via hash and inode; same-artist moves keep tags, cross-artist conflicts route to Maintenance review.

### 5. Standardized Folder Archiving & Rule Engine
- Editable Default template (initial {year}/{date} {tags}); preview before execution.
- Safety: automatic SQLite online backup before any move; previews and revalidates source/target/authorization, never overwrites occupied targets.
- Execution records: Current successful plans are deleted in the same transaction and cannot be undone.
- Controlled auto-archive: Maintenance "Auto Organize" off by default; runs only after a successful full scan; single-artist/folder scans never trigger.

### 6. Storage Safety & Isolated State
- Metadata and indexes in SQLite; media stays within authorized directories; organization never crosses boundaries.
- Safe recycle: deletes prefer the system recycle bin, falling back to gallery/data/recycle, with one-click restore.

> [!WARNING]
> **Network Security Notice**: Gallery listens on port `8899` by default and **provides no built-in user authentication**. It is intended strictly for trusted local area networks (LAN). When exposing the service to public networks, configure authentication via fnOS or an upstream reverse proxy (such as Nginx or Caddy).

---

## Deployment

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

### Method 2: Docker Deployment (NAS / Local PC)

Prefer the native fnOS FPK for production. Choose either Docker workflow below:

| Configuration file | Purpose and usage |
| :--- | :--- |
| `docker-compose.yml` | **Universal configuration (recommended)**. Single file with built-in switching for CPU, Intel iGPU, and NVIDIA; CPU is active by default. Just fill in your media path. |
| `docker-compose.gpu.yml` | **Intel integrated GPU preset**. Preconfigured with `/dev/dri` passthrough; fill in your media path to launch. |
| `docker-compose.cuda.yml` | **NVIDIA GPU preset**. Preconfigured with GPU resource reservations; fill in your media path to launch. |
| `gallery.yml` | **Minimal launcher configuration**. Contains only mode and directories; used by `start.cmd` / `start.sh`. |
| `.env` | **Common parameter configuration**. Modify ports, scan/backup intervals, AI settings, etc.; read automatically on startup. |
| `docker-compose.launcher.yml` | **Internal launcher template**. Bundled in the project directory for the startup script; manual download or editing is not required. |

#### Workflow A: NAS Graphical Interface (No SSH or Python)

Suitable for Synology Container Manager, fnOS Docker, QNAP Container Station, UGOS, and similar NAS interfaces:

1. **Create Project**: Open your NAS Docker management app and create a new Project / Compose.
2. **Set Path and Confirm Mode**:
   - Open `docker-compose.yml`, change `/volume1/photo` under `volumes` to your actual absolute media directory. Each subfolder is recognized as an artist.
   - CPU mode is enabled by default (no GPU needed, universal compatibility). To enable Intel iGPU or NVIDIA, uncomment the respective section and comment out CPU (or use the ready-made `.gpu.yml` or `.cuda.yml` presets).
3. **Configure Parameters (Optional)**: Web port (default 8899), scan intervals, and backup retention have built-in defaults. To customize them, edit `.env` in the same project directory.
4. **Deploy and Scan**: Start the project, open `http://<NAS_IP>:8899/` in your browser, and click "Scan All".

> [!NOTE]
> - **Hardware Acceleration**: Intel graphics requires NAS `/dev/dri` passthrough; if permission issues arise, specify `GALLERY_RENDER_GID` and `GALLERY_VIDEO_GID` in `.env`. NVIDIA requires host GPU drivers and NVIDIA Container Toolkit. Images target x86_64 architecture.
> - **Media Safety & Read-Only**: Media folders are mounted read-only (`:ro`) by default. To enable recycle bin or archive operations, remove the trailing `:ro` and redeploy.
> - **Data Persistence**: Database, logs, backups, caches, and models are persisted in the `gallery-storage` volume. Switching modes preserves all data. Never delete the volume (avoid `docker compose down -v`).

#### Workflow B: One-Click Script Startup (Windows / Linux PC)

Requires Docker Desktop or Docker Engine with Docker Compose, and Python 3.10+ (standard library only, no extra Python packages required):

1. **Get Project**: Download the repository ZIP and extract it (or run `git clone`), keeping the directory structure intact (the launcher template `docker-compose.launcher.yml` and `tools/` are bundled inside).
2. **Configure**: Open `gallery.yml` and set the running mode and media directories:

   ```yaml
   模式: cpu
   目录:
     - D:/Pictures
     - E:/Art
   ```

   - **Path format**: One absolute path per line, unlimited count. Use `/` in Windows paths (e.g. `D:/Pictures`, `E:/Art`); use full paths on Linux (e.g. `/home/user/pictures`). Local disks only; network shares and UNC paths are not supported.
   - **Artist hierarchy**: Top-level subfolders are recognized as artists (e.g. `D:/Pictures/ArtistA/001.jpg`; loose files directly under the media root are not indexed).
3. **Start**: On Windows, double-click `start.cmd`. On Linux, run `sh start.sh` from the project directory.
4. **Access**: Open `http://localhost:8899/` (use host IP for other LAN devices) and click "Scan All". Models download automatically in the background.

| Mode | Supported Hardware & Environment |
| :--- | :--- |
| `cpu` | Default mode; uses CPU inference with universal compatibility. |
| `gpu` | Intel integrated graphics on native Linux; device group IDs are detected automatically (not supported by Windows Docker Desktop). |
| `cuda` | NVIDIA GPU. Linux requires NVIDIA drivers and Container Toolkit; Windows Docker Desktop requires WSL2 backend and compatible drivers. No CUDA Toolkit required. |

After adding directories or switching modes, re-run the same startup script; it automatically updates mounts and restarts the service; mode and directories come from `gallery.yml`. GPU initialization failures fall back to CPU by default; the maintenance page shows the actual GPU/CPU status.

**Data and upgrades**:

- The `gallery-storage` volume persists the database, logs, backups, recycle bin, caches, and models under `data/`, `cache/`, and `models/`. Switching modes preserves data. Do not add `-v` to `docker compose down`: it deletes the data volume.
- Keep `.gallery-launcher`, which records directory mappings so reordering directories does not change existing paths. Existing `.env` media mappings are inherited on the first launcher run.
- Media is read-only by default. Browsing, tagging, and recognition work; recycle-bin and archive operations require write access. Run `start.cmd --writable` or `sh start.sh --writable` to enable it. Starting without this flag restores read-only access. Operations still validate source/target paths and create SQLite online backups; database backups do not include media files.
- Offline model location: `models/character/ccip-caformer_b36-24/model_feat.onnx` inside the data volume. The CUDA runtime cache is under `models/ort/cuda-1.24.1`; the CUDA image includes CUDA 12.x / cuDNN 9.x user-space libraries.
- Keep an independent database backup before upgrading. Update the image with `start.cmd --pull` or `sh start.sh --pull`; build from current source with `start.cmd --build` or `sh start.sh --build`.
- To validate configuration without starting containers, use `start.cmd --check` or `sh start.sh --check`.

**fnOS parameters**: The FPK has no generic environment-variable editor. Docker `.env` does not apply to FPK. fnOS injects `TRIM_*` paths and ports. To change other defaults, edit `fnpack/cmd/main`, rebuild with `tools/build_rust_accel.py` and `tools/build_fnpack.py`, install the FPK, and restart Gallery.

---

## Configuration

Usually no changes are needed. In the NAS interface, edit Compose ports or environment directly. For script startup, advanced options such as the port, scan interval, and backups can still be set in `.env`; run the launcher again to apply them. Mode and directories come from `gallery.yml`. Only variables explicitly referenced by Compose reach the container; the selected mode determines the inference provider. Internal data and model paths are set by the image. For fnOS, use the rebuild process above.

| Variable | Default | Description |
| :--- | :--- | :--- |
| `CHARACTER_RECOGNITION_ENABLED` | `1` | Enable AI character recognition (1 on / 0 off). |
| `CHARACTER_RECOGNITION_PROVIDER` | `auto` | NAS Compose files and the launcher set cpu, openvino, or cuda according to the selected mode. |
| `CHARACTER_ALLOW_CPU_FALLBACK` | `1` | Fall back to CPU when GPU init fails (0 disables; legacy `CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK` still honored). |
| `ONNXRUNTIME_AUTO_DOWNLOAD` | `1` | Auto-download missing CCIP model and CUDA runtime in the background after startup (0 disables, for offline installs). |
| `CHARACTER_MODEL_IDLE_TIMEOUT_SECONDS` | `600` | Idle timeout in seconds before unloading the model (0 keeps resident). |
| `SCAN_INTERVAL` | `21600` | Background full-scan interval in seconds (default 6 hours; 0 disables). |
| `HASH_INTERVAL` | `30` | Background content-hash poll interval in seconds. |
| `DB_BACKUP_INTERVAL` | `43200` | Database auto-backup interval in seconds (default 12 hours). |
| `DB_BACKUP_RETENTION` | `8` | Maximum number of retained backup snapshots. |

For the remaining runtime variables and their defaults, see `fnpack/cmd/main` and `Dockerfile`.

---

## Building & Packaging

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

## Project Structure

```text
gallery/
├── rust/gallery_accel/   # Core Rust runtime: Axum HTTP API, SQLite connection pool, background scan/hash workers, CCIP engine
├── app/static/           # Single-page web application: HTML5 shell, layered styles/ design system, and ES-module JS scripts
├── fnpack/               # fnOS native package definition (package.json), lifecycle entry scripts (cmd/main), and permissions
├── Dockerfile            # Pure-Rust Docker image build definition (default target is runtime)
├── Dockerfile.cuda       # CUDA image variant (builds runtime-cuda for CI)
├── docker-compose.yml    # Docker Compose configuration (supports CPU, iGPU, and CUDA)
└── tools/                # Automation scripts for FPK packaging, Rust compilation, work logging, and public repository generation
```

---

## License

This project is licensed under the [GNU General Public License v3.0 only](LICENSE) (`GPL-3.0-only`).
Copyright (C) 2026 h-void.

Redistributing this project or modified derivatives requires providing corresponding source code under GPL-3.0-only.

## Third-Party Licenses

Bundled license notices for ONNX Runtime, OpenVINO, and third-party Rust crates in FPK and Docker distributions are available in `fnpack/app/licenses/`.
