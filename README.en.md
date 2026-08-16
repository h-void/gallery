<div align="center">

# 🖼️ Gallery

**High-Performance, Lightweight Self-Hosted Media Library & Digital Asset Manager Written in Pure Rust**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS & Docker](https://img.shields.io/badge/Platform-fnOS%20%7C%20Docker-brightgreen.svg)](#-deployment)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#-project-structure)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#-key-features)

<p align="center">
  Tailored for large local collections of illustrations, manga, videos, and creative works.<br>
  Partitioned by artist with AI character recognition, content-hash deduplication, and automated folder archiving.
</p>

[✨ Features](#-key-features) • [🚀 Deployment](#-deployment) • [⚙️ Configuration](#️-configuration) • [🛠️ Building](#️-building--packaging) • [📂 Structure](#-project-structure) • [📄 License](#-license)

</div>

---

## 📖 Introduction

**Gallery** is a self-hosted media library management system engineered specifically for local creative asset collections. The backend is written entirely in pure Rust (Axum + rusqlite + Tokio) for near-instant response times and ultra-low resource usage, complemented by a responsive modern single-page web interface optimized for both desktop widescreen and mobile touchscreens.

Gallery indexes user-authorized media directories, partitions the entire collection with "Artists" as the top-level boundary (directories define artists), and methodically organizes images, videos, text files, and original creative source files using structured folders and multi-dimensional tags. It features robust BLAKE3 content-hash deduplication, OpenVINO-accelerated AI character recognition, and transactional folder archiving with automatic online backups.

---

## ✨ Key Features

### 1. 🎬 Multi-Format Browsing & Smooth Media Playback
- **Comprehensive Format Support**:
  - **Images**: Browse JPG, PNG, WebP, AVIF, BMP, and more with high-performance asynchronous thumbnail generation and memory queue management for instant repeat loading.
  - **Animated GIFs**: Hover over cards in the gallery grid for automatic inline GIF playback.
  - **Video Streaming**: Play MP4, WebM, MOV, and MKV files directly in-browser with `ffmpeg`-powered stream extraction, keyframe capture, and HLS transcoding.
  - **Text & Notes**: Integrated reader supports viewing `.txt`, `.md`, `.html`, and `.htm` text files.
  - **Source Files & Archives**: Index and manage PSD, CLIP, PSB, TGA project files and ZIP/RAR archives.
- **Immersive Lightbox**:
  - Full-screen high-resolution preview with mouse-wheel zoom, drag-to-pan, and mobile touch gestures.
  - Complete keyboard shortcuts (`←` / `→` navigate, `Esc` close, quick download, favorite, and delete).
- **Synchronized Shareable URLs**:
  - All filter states (artist, folder hierarchy, multi-tag combinations, date ranges, sorting rules, and search terms) are bidirectionally synced with the URL query string for natural browser navigation (back/forward) and direct link sharing.

### 2. 🎨 Artist Partitioning & Multi-Dimensional Tagging
- **Artist-Centric Boundary**: Clean segregation by artist (folders define artists). Inside an artist's collection, effortlessly filter across folder trees and multi-tag combinations.
- **Efficient Batch Operations**:
  - Batch assign or remove tags across single items, marquee rectangular selections, or whole folders.
  - Sidebar tag list supports sorting by system default, alphabetical name order, or item count.
  - Native Pinyin search enables rapid fuzzy lookups for Chinese artist names and tags.
- **Link Indexing & Profile Management**:
  - **Automated Text & Cloud Link Extraction**: Automatically scans text and HTML files (`.txt`, `.html`, `.htm`) inside artist directories to extract download links, cloud drive shares, and passcodes for quick searching and copying.
  - **Artist Social & Subscription Profiles**: Provides a dedicated "Social & Subscriptions" panel to record and manage external profile links (Pixiv, Fanbox, Patreon, Twitter/X, Bilibili, etc.).

### 3. 🧠 AI Character Recognition & Feature Library (CCIP & OpenVINO)
- **Hardware Acceleration & Suggestions**:
  - Built-in AI character recognition powered by OpenVINO (accelerated on Intel integrated/discrete GPUs, with explicit CPU fallback support).
  - Uses the CCIP model from Hugging Face's `deepghs/ccip_onnx` repository, placed at `gallery/models/character/ccip-caformer_b36-24/model_feat.onnx`.
- **Multi-Style Reference Library**:
  - Enrolls single-character reference artworks to build rich character embedding profiles that capture varied artistic styles and costumes.
  - Built-in semantic deduplication (SemDeDup, similarity $\ge 0.95$) and low-confidence outlier pruning.
- **Human-in-the-Loop Safeguards**:
  - AI inference presents suggestions in Edit Mode; tags are only applied when explicitly confirmed by the user, ensuring zero unwanted tag mutations.

### 4. 🔍 Content-Hash Deduplication & Path Tracking
- **BLAKE3 Precision Fingerprinting**: High-throughput content hashing pinpoints duplicate files across the library, with dedicated duplicate group views in the sidebar.
- **Seamless Relocation Tracking**:
  - Automatically tracks moved or renamed files via content hashes and file inodes. Files moved within the same artist retain tag metadata seamlessly.
  - Cross-artist moves automatically consolidate when all missing sources and the unique target share identical hashes; ambiguous conflicts route to the Maintenance workbench for review.

### 5. 📁 Standardized Folder Archiving & Rule Engine
- **Template-Based Naming**: Format artist subdirectories according to customizable profiles (e.g. `{year}/{date} {tags}`, `{artist}/{title}`), with configurable collision policies (auto-suffix `-1` or skip).
- **Zero Data-Loss Safety Guarantee**:
  - **Automatic Backups**: Automatically takes a SQLite online backup snapshot prior to executing any directory operations.
  - **Dry-Run Validation**: Comprehensive pre-flight checks detect path conflicts before moving files.
  - **One-Click Rollback**: Supports full one-click undo for executed operations.
- **Automated Archiving Hook**: An "Auto Organize" global toggle in Maintenance allows automated execution of high-confidence archive rules strictly upon the successful completion of a full-library scan.

### 6. 🛡️ Storage Safety & Isolated State
- **Storage Boundary Enforcement**: Application state, indexes, and metadata reside safely in SQLite. Media files remain strictly within user-authorized directories; organization routines operate exclusively within these boundaries.
- **Safe Recycle Storage**: Deleted items prioritize the fnOS system recycle bin, safely falling back to `gallery/data/recycle` when needed, supporting clean restores to original paths.

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
5. **Install AI Character Model**: Download the CCIP ONNX model and place it at:
   ```text
   gallery/models/character/ccip-caformer_b36-24/model_feat.onnx
   ```
6. **Launch**: Access Gallery via the fnOS desktop shortcut or in your browser at `http://<NAS_IP>:8899/`, then click "扫描全库" (Scan All) to perform the initial index.

---

### Method 2: Docker / Docker Compose

Deploy seamlessly across Windows, Linux, and macOS using Docker.

1. **Prerequisites**: Install Docker Desktop or Docker Engine.
2. **Start the Stack**: Define your media path and start the container:

```powershell
$env:GALLERY_MEDIA_DIR = 'D:\Pictures'
docker compose up -d --build
```

3. **Storage & Permission Notes**:
   - Databases, preview caches, and models are stored in the unified `gallery-storage` Docker volume under `data/`, `cache/`, and `models/`.
   - Docker uses CPU character recognition by default; place the model at `models/character/ccip-caformer_b36-24/model_feat.onnx` within the volume.
   - Media is mounted read-only (`:ro`) by default. To enable file organization, archiving, or deletion, change `/media:ro` to `/media` in `docker-compose.yml`.

---

## ⚙️ Configuration

The following environment variables can be customized in your environment or Compose file:

| Variable | Default | Description |
| :--- | :--- | :--- |
| `SERVICE_PORT` | `8899` | HTTP service port |
| `DATA_DIR` | `data` | Data directory path (contains `gallery.db`, logs, backups) |
| `IMAGE_PREVIEW_CACHE_DIR` | `cache` | Thumbnail and video transcode cache directory |
| `CHARACTER_RECOGNITION_ENABLED` | `1` | Enable AI character recognition (`1` for enabled, `0` for disabled) |
| `CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK` | `0` | Allow OpenVINO CPU fallback when GPU is unavailable |
| `CHARACTER_MODEL_IDLE_TIMEOUT_SECONDS` | `600` | Idle timeout in seconds before unloading the model (`0` to keep resident) |
| `SCAN_INTERVAL` | `21600` | Background full scan interval in seconds (default 6 hours; `0` disables) |
| `HASH_INTERVAL` | `30` | Background content hasher poll interval in seconds |
| `DB_BACKUP_INTERVAL` | `43200` | Database automatic backup interval in seconds (default 12 hours) |
| `DB_BACKUP_RETENTION` | `8` | Number of historical database snapshot backups to retain |

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
├── Dockerfile            # Pure-Rust Docker container build definitions
├── docker-compose.yml    # Multi-platform Docker Compose configurations
└── tools/                # Automation scripts for FPK packaging, Rust compilation, work logging, and public repository generation
```

---

## 📄 License

This project is licensed under the [GNU General Public License v3.0 only](LICENSE) (`GPL-3.0-only`).  
Copyright (C) 2026 h-void.

Redistributing this project or modified derivatives requires providing corresponding source code under GPL-3.0-only.

## 📜 Third-Party Licenses

Bundled license notices for ONNX Runtime, OpenVINO, and third-party Rust crates in FPK and Docker distributions are available in `fnpack/app/licenses/`.
