# Gallery

Gallery is a local media library for fnOS. The backend is written in Rust and distributed as a native fnOS FPK package (no Docker required). It indexes the media directories you connect, partitions the library by artist, and organizes images, videos, and source files by folders and tags within each artist.

## Features

- **Browse and preview**: images, videos, source files, text, and archives, with date/sort filters and shareable browse URLs that support browser back and forward.
- **Organization**: the library is partitioned by artist — your folders *are* the artists. After selecting an artist, you can browse and filter within that artist's library by folders and tags.
- **Character recognition**: AI character recognition is on by default (OpenVINO on GPU, with explicit CPU fallback); obtain the model from the Hugging Face repository `deepghs/ccip_onnx` and place it at `gallery/models/character/ccip-caformer_b36-24/model_feat.onnx`. Artist recognition is off by default, since folders already define artists.
- **Duplicate detection and path monitoring**: detects duplicate files by content hash and automatically reconnects unambiguous moves within the same artist. A cross-artist group is applied automatically only when every missing source and the unique target share the same nonempty hash; its source tags are merged, while all other uncertain cases require manual confirmation.
- **Archive planning**: gathers scattered content into an archive plan that executes only after confirmation (a backup is taken before execution). Auto-execution is off by default and triggers only after the switch is enabled and a full scan completes successfully.
- **Index and file location**: the index lives in SQLite. Active media stays within your fnOS-authorized media directories; organization and archiving move files only inside them. Deletion prefers the fnOS recycle area and falls back to `gallery/data/recycle` when needed.

The service listens on port `8899` by default and **has no built-in authentication**. Use it only on a trusted LAN; if exposed to the public internet, add authentication at the fnOS or reverse-proxy layer first. Pinyin search is supported for tags and artists.

## Installation

Download the latest published `gallery_<version>_x86_64.fpk` from [Releases](https://github.com/hczhr/gallery-archive/releases) and install it via the fnOS app installer.

The source repository and FPK **do not include** the character model, databases, or caches. The FPK supplies the runtime only; obtain the model separately and place it at the path above.

## Building

Windows release builds require WSL 2 with Podman installed inside WSL. The build may download the pinned builder image and ONNX Runtime libraries.

One-shot build (compiles the Rust runtime, then packages the FPK):

```powershell
python tools/build_release.py --fnpack .\output\fnpack\fnpack-1.2.3-windows-amd64.exe
```

fnOS Linux Rust runtime only:

```powershell
python tools/build_rust_accel.py
```

## Project structure

| Path | Description |
| --- | --- |
| `rust/gallery_accel/` | Rust runtime and Axum API |
| `app/static/` | Web UI |
| `fnpack/` | fnOS package config and startup scripts |
| `tools/` | Tools to build Rust, package the FPK, and generate the public source tree |

## License

This project is licensed under the [GNU General Public License v3.0 only](LICENSE)
(`GPL-3.0-only`). Redistributing this project or a modified version requires
providing the corresponding source code under GPL-3.0-only.
Copyright (C) 2026 hczhr.

## Third-party licenses

The FPK bundles ONNX Runtime, OpenVINO, and Rust dependency licenses and notices
under `fnpack/app/licenses/`.
