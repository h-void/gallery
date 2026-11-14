<div align="center">

# Gallery

**Self-Hosted Media Library & Artist Asset Manager Written in Pure Rust**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS (Native FPK)](https://img.shields.io/badge/Platform-fnOS%20FPK-brightgreen.svg)](#deployment)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#project-structure)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#key-features)

[English](README.en.md) • [简体中文](README.md)

[Features](#key-features) • [Deployment](#deployment) • [Configuration](#configuration) • [AI Agent & API](#ai-agent--http-api) • [Building](#building--packaging) • [License](#license)

</div>

---

## Introduction

**Gallery** is a self-hosted media library designed for local illustration collections and creator portfolios. Powered by a pure-Rust backend (Axum + rusqlite + Tokio) with zero Python runtime dependencies and a framework-free responsive single-page web UI, it organizes media around **artists as top-level directories** and integrates multi-facet filtering, CCIP character recognition and cluster discovery, BLAKE3 content deduplication, rule-based folder archiving, creator subscriptions, and JDownloader netdisk dispatch.

---

## Key Features

- **Multi-Format Browsing & Studio Folio Aesthetics**
  - **Broad media support**: Browse JPG/PNG/WebP/AVIF/BMP images, inline GIF hover previews (with automatic aspect-ratio probing on load for unmeasured GIFs), and streamed MP4/WebM/MOV/MKV videos (`ffmpeg` / HLS), alongside `.txt`/`.md`/`.html` text files and PSD/CLIP/PSB source assets.
  - **In-archive inspection & smart batch extraction**: Click ZIP/RAR/7Z cards to inspect directory trees, file sizes, and image previews on demand (bounded per-entry memory, with arrow-key navigation and index counters). Extract selected archives or all archives under an artist in one click (skipping passwordless `.7z` cold-storage archives by default while extracting `.zip`, `.rar`, and encrypted `.7z`), with automatic password trial from sibling `.txt` notes, netdisk passcodes, and per-artist password memory, plus automatic source recycling.
  - **Fluid views, drag-and-drop & URL state**: Switch between `4:3 Full` and `3:4 Portrait` grids and a high-density single-row list, paired with warm-paper light mode, the ink-charcoal Studio Folio Dark palette, and instant English/Chinese switching. The full-screen lightbox supports smooth pan/zoom, media spec inspection, clickable tag filtering, and one-click folder location; drag cards directly onto sidebar folders to move files or onto character rows to assign tags; use `Shift + Click` for contiguous range selection, with active filters, sorting, and scroll state synced to the URL bar.
- **Artist Partitioning & Multi-Facet Search**
  - **Per-artist & cross-artist browsing**: First-level subfolders under each media root map to artists, supporting both single-artist views and cross-artist `All Artists` browsing, collapsible folder trees with pinyin filtering, combined character-tag and media-type filtering, date-range and `Undated` filters, and `All / Pending / Archived` organization views.
  - **Fast batch tagging & search**: Apply or remove tags via single selection, marquee drag-selection, or whole-folder actions (tags are case-sensitive identities and propagate across same-hash copies); pin an "Untagged" character backlog in the sidebar; search artists, folders, and tags with native pinyin matching and a `Character names only` mode.
  - **Automatic link indexing**: Extracts web URLs, cloud-drive links, and passcodes from artist text files, and manages external creator profiles (Pixiv, Fanbox, Patreon, Twitter/X, Bilibili).
- **AI Character Recognition (CCIP) & Cluster Discovery**
  - **Automatic hardware acceleration**: Enabled by default for both images and sampled video frames (with automatic fallback past black/white fade-in frames); prepares the CCIP model and CUDA runtime in the background after startup and automatically selects `CUDA → OpenVINO → CPU` (with automatic CPU fallback).
  - **Reference library, candidate mining & cluster discovery**: Extracts reference embeddings from single-character works (pruning ≥0.95 near-duplicates and outliers), supports background character import, manual reference uploads, and one-click `Discover candidates` with an `Unverified only` review filter for individual or batch approval; groups untagged works into cohesive character clusters via 64-bit perceptual-hash folding and Complete-Linkage feature clustering. All AI suggestions and clusters apply only after confirmation.
- **Content-Hash Deduplication & Move Tracking**
  - **BLAKE3 deduplication**: Groups duplicate folders and files in the sidebar and silently refreshes views as background hashing completes.
  - **Automatic move reconciliation**: Relinks externally renamed or moved files via BLAKE3 hash and inode; same-artist moves preserve their tags automatically, while cross-artist conflicts route to Maintenance review.
- **Standardized Folder Archiving & Dual-Mode Recycle Bin**
  - **Rule engine & inline conflict resolution**: Uses an editable `Default` template (default `{year}/{date} {tags}`, deriving missing dates from filenames or Twitter/X snowflake posting dates). Fill missing character tags or manual dates directly from the folder sidebar, archive the current folder in one click, bundle selected files into a target subfolder, resolve `date_conflict` plans inline from the Maintenance panel (unify folder dates or switch to tag/date split archiving), and preview library-wide naming-rule migrations before switching.
  - **Transactional & backup safety**: Takes an automatic SQLite online backup before moving files; preserves existing targets and rolls back on failure; supports optional auto-archiving after completed full-library scans.
  - **Dual-mode recycle bin**: Switch between native fnOS system trash and internal app trash from the Maintenance panel (with comparison cards); supports file and folder deletion, image/video/archive cover thumbnail previews, grid/list view toggle, one-click batch restore (including tag and favorite snapshots), and permanent deletion.
- **Download Subscriptions & Netdisk Dispatch**
  - **Subscription sync & coverage calendar**: Subscribe to creator profiles in automatic or manual mode for round-based discovery, local folder reconciliation, and resumable downloads, with a full-width 53-week coverage calendar on each card showing daily gaps and post details.
  - **All Works & JDownloader bridge**: Filter across all subscriptions, inspect file delivery states, and import local works; dispatch post cloud links or direct external links to JDownloader via the local Event Scripter bridge with automatic archive import.

> [!WARNING]
> **Network security**: Gallery listens on port `8899` and ships without built-in user authentication, so it fits a home or trusted LAN. For public access, add authentication through fnOS or an upstream reverse proxy (Nginx / Caddy).

---

## Deployment

### Method 1: fnOS Native FPK Package (Recommended)

Gallery ships as a native fnOS package (FPK) that runs a single Rust binary directly on the host, binding its port immediately on launch while completing initialization in the background, with `jemalloc` memory reclamation and a bundled `7zz` engine.

1. **Download & Install**: Download `gallery_<version>_x86_64.fpk` from [Releases](https://github.com/h-void/gallery/releases) and install it manually via the fnOS App Center.
2. **Authorize Media Folders**: Select the media shares you want Gallery to manage during the setup wizard.
3. **Launch & Scan**: Open `http://<NAS_IP>:8899/` and click **Scan All** to build the initial index.

Persistent application state is stored under `@appshare/gallery` (strictly separated from your media roots):

```text
@appshare/gallery/
├── data/      # Main database gallery.db, runtime logs/, online db-backups/, fallback recycle/
├── cache/     # Preview thumbnails, video frames, and HLS segments under transcode-cache/
└── models/    # AI character models, OpenVINO caches, and CUDA runtime caches
```

> [!TIP]
> **Offline Model Placement**: The CCIP model (`deepghs/ccip_onnx`) downloads automatically in the background after startup. For offline NAS setups, place the model manually at `gallery/models/character/ccip-caformer_b36-24/model_feat.onnx`.

---

### Method 2: Docker Deployment (NAS GUI / Local PC)

For Synology Container Manager, QNAP Container Station, UGOS, fnOS Docker, or Windows/Linux PCs, choose either workflow below:

#### Workflow A: One-Click Script Startup (Windows / Linux PC)

Requires Docker (with Docker Compose) and Python 3.10+ (standard library only):

1. **Configure mode & directories**: Edit `gallery.yml` with your inference mode (`cpu` / `gpu` / `cuda`) and local absolute media paths (use `/` on Windows; first-level subfolders under each path define artists):
   ```yaml
   模式: cpu
   目录:
     - D:/Pictures
     - E:/Art
   ```
2. **Start**: Double-click `start.cmd` on Windows or run `sh start.sh` on Linux, then open `http://localhost:8899/`.
3. **Common flags** (re-run the script anytime after editing `gallery.yml` to update mounts):
   - `start.cmd --writable` (or `sh start.sh --writable`): Enable media write access for recycle-bin deletions and folder archiving (omitting the flag restores read-only mode)
   - `start.cmd --pull` / `--build`: Pull the latest image / build from local source
   - `start.cmd --check`: Validate configuration without starting containers

#### Workflow B: NAS Graphical Interface (Docker Compose, No SSH)

1. **Pick a Compose file**:
   - `docker-compose.yml`: Universal file (CPU active by default, with inline comments for Intel iGPU and NVIDIA)
   - `docker-compose.gpu.yml`: Intel iGPU preset (preconfigured with `/dev/dri` passthrough)
   - `docker-compose.cuda.yml`: NVIDIA GPU preset (preconfigured with GPU reservations and CUDA image)
2. **Set media path**: Replace `/volume1/photo` under `volumes` with your actual NAS media directory path (customize port `8899` or intervals in `.env` if needed).
3. **Deploy**: Start the Compose project in your NAS UI, open `http://<NAS_IP>:8899/`, and click **Scan All**.

| Mode | Hardware & Environment Requirements |
| :--- | :--- |
| `cpu` | Default mode; runs on CPU across all `x86_64` hosts. |
| `gpu` | Intel iGPU on native Linux (`/dev/dri` passthrough; set `GALLERY_RENDER_GID` and `GALLERY_VIDEO_GID` in `.env` if needed on NAS; not supported on Windows Docker Desktop). |
| `cuda` | NVIDIA GPU (requires host drivers + NVIDIA Container Toolkit on Linux, or WSL2 backend + compatible drivers on Windows Docker Desktop; image bundles CUDA 12.x / cuDNN 9.x libraries). |

> [!IMPORTANT]
> **Docker Data Safety & Permissions**
> - **Keep the data volume**: Database, logs, backups, caches, and AI models live in the `gallery-storage` volume (offline model path: `models/character/ccip-caformer_b36-24/model_feat.onnx`). Do not run `docker compose down -v` when upgrading or switching modes. Script users should also keep `.gallery-launcher` to preserve `/mediaN` directory slot mappings.
> - **Read-only media default**: Media folders mount read-only (`:ro`) by default. Remove `:ro` in Compose or pass `--writable` to the launcher script to enable recycle-bin and archive operations.
> - **7-Zip in Docker**: To inspect or extract archives in Docker, ensure `7zz` / `p7zip-full` is available in the container or set `GALLERY_7Z_PATH`.

---

## Configuration

Most deployments need no manual tuning. In Docker, customize settings via `.env` or Compose environment variables. The native fnOS FPK has no generic environment editor (`TRIM_*` variables are injected by fnOS and other defaults live in `fnpack/cmd/main`).

| Variable | Default | Description |
| :--- | :--- | :--- |
| `CHARACTER_RECOGNITION_ENABLED` | `1` | Enable AI character recognition (`1` on / `0` off). |
| `CHARACTER_RECOGNITION_PROVIDER` | `auto` | Inference provider (`auto` selects `CUDA → OpenVINO → CPU`; Docker presets set `cpu`, `openvino`, or `cuda`). |
| `CHARACTER_ALLOW_CPU_FALLBACK` | `1` | Fall back to CPU when GPU initialization fails (`0` disables). |
| `ONNXRUNTIME_AUTO_DOWNLOAD` | `1` | Auto-download missing CCIP model and CUDA runtime in the background after startup (`0` disables for offline installs). |
| `CHARACTER_MODEL_IDLE_TIMEOUT_SECONDS` | `600` | Unload the AI model after N idle seconds (`0` keeps it resident). |
| `SCAN_INTERVAL` | `21600` | Scheduled full-scan interval in seconds (default 6 hours; `0` disables). |
| `HASH_INTERVAL` | `30` | Background content-hash poll interval in seconds. |
| `DB_BACKUP_INTERVAL` | `43200` | Automatic SQLite online backup interval in seconds (default 12 hours). |
| `DB_BACKUP_RETENTION` | `8` | Maximum number of retained backup snapshots. |

---

## AI Agent & HTTP API

Gallery exposes a complete HTTP REST API and ships with [`docs/API_USAGE.md`](docs/API_USAGE.md), an operational playbook written for multimodal and coding AI Agents. Point your Agent to [`docs/API_USAGE.md`](docs/API_USAGE.md) as context or a skill so it can autonomously inspect artist portfolios, test passwords and unpack archives, deduplicate via BLAKE3, run multimodal character clustering and tagging, execute folder archiving, and dispatch JDownloader tasks under built-in approval guardrails.

---

## Building & Packaging

Cross-compiling the Linux `x86_64` binary and packaging the fnOS FPK on Windows requires **WSL 2** with **Podman** and the official [`fnpack-1.2.3-windows-amd64.exe`](https://static2.fnnas.com/fnpack/fnpack-1.2.3-windows-amd64) placed in `output/fnpack/`:

```powershell
# Cross-compile the Linux Rust runtime and package the validated FPK
python tools/build_release.py --fnpack .\output\fnpack\fnpack-1.2.3-windows-amd64.exe

# Compile the Linux Rust binary only (outputs app/bin/gallery-accel)
python tools/build_rust_accel.py
```

---

## Project Structure

```text
gallery/
├── rust/gallery_accel/   # Pure-Rust backend: Axum HTTP API, SQLite pool, scan/hash workers, CCIP inference & subscriptions
├── app/static/           # Responsive web SPA: HTML5 shell, 4-layer CSS design system, and native ES-module scripts
├── fnpack/               # fnOS native package manifest (package.json), lifecycle scripts (cmd/main), and permissions
├── Dockerfile            # Pure-Rust Docker image build definition (Dockerfile.cuda for CUDA variant)
├── docker-compose.yml    # Docker Compose configuration (with .gpu.yml, .cuda.yml, and gallery.yml)
├── docs/API_USAGE.md     # AI-Agent HTTP REST API operational playbook and full route reference
└── tools/                # Build pipelines, FPK packager, local launcher, and public-release generator
```

---

## License

Licensed under the [GNU General Public License v3.0 only](LICENSE) (`GPL-3.0-only`). Copyright (C) 2026 h-void. Redistributing this project or modified derivatives requires providing corresponding source code under `GPL-3.0-only`.

Third-party license notices for ONNX Runtime, OpenVINO, 7-Zip (`7zz`, © Igor Pavlov, LGPL-2.1+), and Rust crates are located in `fnpack/app/licenses/`.
