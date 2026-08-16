# syntax=docker/dockerfile:1

FROM python:3.12-slim-bookworm AS ort

WORKDIR /src
COPY tools/_fetch_ort_openvino_libs.py ./fetch_ort.py
RUN python ./fetch_ort.py --dest /tmp/ort-full --cache /tmp/ort-wheel \
    && mkdir -p /opt/ort \
    && cp /tmp/ort-full/libonnxruntime.so.1.24.1 /opt/ort/ \
    && ln -s libonnxruntime.so.1.24.1 /opt/ort/libonnxruntime.so.1 \
    && ln -s libonnxruntime.so.1 /opt/ort/libonnxruntime.so

FROM rust:1-bookworm AS builder

WORKDIR /src
COPY rust/gallery_accel/Cargo.toml rust/gallery_accel/Cargo.lock ./
COPY rust/gallery_accel/src/ ./src/
RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg libgomp1 tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 gallery \
    && useradd --uid 10001 --gid gallery --create-home gallery

WORKDIR /opt/gallery
COPY --from=builder /src/target/release/gallery_accel /usr/local/bin/gallery-accel
COPY --from=ort /opt/ort/ ./lib/
COPY app/static/ ./static/
COPY LICENSE ./LICENSE
COPY fnpack/app/licenses/ONNXRUNTIME_LICENSE.txt fnpack/app/licenses/ONNXRUNTIME_THIRD_PARTY_NOTICES.txt ./licenses/

RUN mkdir -p /gallery/data/logs /gallery/data/db-backups \
        /gallery/cache/video-frames /gallery/cache/transcode-cache \
        /gallery/models/character /gallery/models/artist \
    && chown -R gallery:gallery /gallery /opt/gallery

ENV DATA_DIR=/gallery/data \
    GALLERY_STATIC_DIR=/opt/gallery/static \
    PICTURES_ROOT=/media \
    PICTURES_ROOT_REAL_PATHS=/media \
    PICTURES_ROOT_LABELS=media \
    IMAGE_PREVIEW_CACHE_DIR=/gallery/cache \
    IMAGE_PREVIEW_CACHE_MAX_BYTES=10000000000 \
    IMAGE_PREVIEW_MAX_SOURCE_PIXELS=134217728 \
    VIDEO_FRAME_CACHE_DIR=/gallery/cache/video-frames \
    VIDEO_FRAME_CACHE_MAX_BYTES=2000000000 \
    VIDEO_TRANSCODE_CACHE_DIR=/gallery/cache/transcode-cache \
    VIDEO_TRANSCODE_CACHE_MAX_BYTES=900000000 \
    MODEL_CACHE_ROOT=/gallery/models \
    CHARACTER_MODEL_DIR=/gallery/models/character \
    ARTIST_MODEL_DIR=/gallery/models/artist \
    CHARACTER_RECOGNITION_PROVIDER=cpu \
    CHARACTER_IMPORT_IDLE_ENABLED=0 \
    ARTIST_RECOGNITION_ENABLED=0 \
    SCAN_ON_START=0 \
    SCAN_INTERVAL=21600 \
    HASH_INTERVAL=30 \
    HASH_BATCH_SIZE=500 \
    DB_BACKUP_INTERVAL=43200 \
    DB_BACKUP_RETENTION=8 \
    DB_BACKUP_ON_START=0 \
    DB_BACKUP_START_DELAY=120 \
    LD_LIBRARY_PATH=/opt/gallery/lib \
    ORT_DYLIB_PATH=/opt/gallery/lib/libonnxruntime.so.1

USER gallery

EXPOSE 8899
VOLUME ["/gallery"]

HEALTHCHECK --interval=30s --timeout=10s --retries=3 --start-period=20s \
    CMD curl -fsS http://127.0.0.1:8899/api/health > /dev/null || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "gallery-accel"]
CMD ["--primary", "--enable-ml", "--db", "/gallery/data/gallery.db", "--static-dir", "/opt/gallery/static"]
