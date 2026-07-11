# Gallery

A self-hosted media library with AI-assisted character and artist recognition,
duplicate/move detection, folder archiving, and video streaming. The product
runtime is the pure Rust `gallery-accel` service packaged as a native **fnOS**
FPK; it is not a Docker deployment.

The web UI and API listen on the native package port `8899`. The SQLite library
is stored in the fnOS `gallery-data` share. Video preview/transcoding requires
`ffmpeg` on the target system, and character recognition requires the model
files supplied through `CHARACTER_MODEL_DIR` (the model is not bundled here).

## Install

Production installation uses the FPK artifact
`gallery_1.0.175_x86_64.fpk`, whose version comes from `fnpack/package.json`.
Upload that artifact through the fnOS app installer. The source archive does
not contain generated binaries, model files, databases, or caches.

## Build

The release wrapper builds the Linux Rust runtime, stages the fnOS package, and
validates the resulting gzip-based FPK. Use the official `fnpack` CLI and pass
its path when it is not already discoverable:

```powershell
python tools/build_release.py --fnpack .\output\fnpack\fnpack-1.2.1-windows-amd64.exe
```

The Rust crate can also be built directly with a locked release build:

```powershell
cargo build --release --locked --manifest-path rust/gallery_accel/Cargo.toml
```

## Source layout

| Path | Purpose |
| --- | --- |
| `rust/gallery_accel/` | Pure Rust product runtime and Axum API. |
| `app/static/` | Browser UI assets served by the runtime. |
| `fnpack/` | Native fnOS package metadata, callbacks, and command skeleton. |
| `tools/` | Reproducible Rust/FPK and public-tree build scripts. |

The public repository is a sanitized product-source snapshot. `.gitattributes`
keeps GitHub's language summary focused on the Rust runtime. The legacy
Python backend, internal tests and operations notes, development databases,
models, and generated artifacts remain in the private development repository
and are intentionally not part of this archive.
