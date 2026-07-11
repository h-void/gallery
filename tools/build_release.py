"""One-click release build for the native fnOS FPK package.

Chains the Rust runtime build into the fnpack FPK build and validates the
produced artifact (it must be a real fnpack gzip stream, never a zip renamed
to ``.fpk``). Optional upload to a GitHub Release is supported via the
``--upload`` flag when the ``gh`` CLI and a token are available.

Usage::

    python tools/build_release.py
    python tools/build_release.py --no-rust          # skip the Rust rebuild
    python tools/build_release.py --upload           # upload to a Release
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from tools import build_fnpack
from tools import build_rust_accel


def _build_rust() -> Path:
    print("[build_release] building Rust runtime ...")
    binary = build_rust_accel.build_rust_accel()
    print(f"[build_release] rust accel: {binary}")
    return binary


def _build_fnpack(output_dir: Path, staging_dir: Path, fnpack_binary_arg: str | None = None) -> Path:
    print("[build_release] staging fnpack package ...")
    metadata = build_fnpack.load_package_metadata()
    staging = build_fnpack.stage_package(staging_dir, metadata)
    fnpack_binary = build_fnpack.find_fnpack(fnpack_binary_arg)
    artifact = build_fnpack.build_with_fnpack(staging, output_dir, fnpack_binary, metadata)
    return artifact


def _validate_artifact(artifact: Path, metadata: dict) -> None:
    # Delegate to the single shared validator in build_fnpack so the FPK build
    # and the release wrapper enforce identical artifact rules.
    build_fnpack.validate_fnpack_artifact(artifact, metadata)
    print(f"[build_release] validated: {artifact.name} (gzip fnpack stream)")


def _upload(artifact: Path, version: str) -> None:
    gh = build_fnpack.shutil.which("gh")
    if not gh:
        raise RuntimeError("gh CLI not found; cannot --upload release")
    # Only upload to the configured origin of THIS repo. Refuse wrong remotes.
    remote = subprocess.run(
        ["git", "remote", "get-url", "origin"],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    remote_url = (remote.stdout or "").strip().lower()
    if remote.returncode != 0 or not remote_url:
        raise RuntimeError("cannot resolve git origin; refusing --upload")
    # Accept only the gallery package repo (name contains gallery).
    if "gallery" not in remote_url:
        raise RuntimeError(
            f"origin remote does not look like gallery package repo: {remote_url!r}"
        )
    tag = f"v{version}"
    subprocess.run(
        [
            gh,
            "release",
            "create",
            tag,
            str(artifact),
            "--title",
            f"Gallery {version}",
            "--generate-notes",
        ],
        check=True,
    )
    print(f"[build_release] uploaded {artifact.name} to release {tag}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Build and validate the fnOS FPK release.")
    parser.add_argument("--output-dir", default=str(build_fnpack.DEFAULT_OUTPUT))
    parser.add_argument("--staging-dir", default=str(build_fnpack.DEFAULT_OUTPUT / "stage"))
    parser.add_argument("--fnpack", default=None, help="Path to the official fnpack CLI.")
    parser.add_argument("--no-rust", action="store_true",
                        help="Skip rebuilding the Rust runtime (use existing binary).")
    parser.add_argument("--upload", action="store_true",
                        help="Upload the artifact to a GitHub Release via gh.")
    args = parser.parse_args()

    if not args.no_rust:
        _build_rust()

    output_dir = Path(args.output_dir)
    staging_dir = Path(args.staging_dir)
    artifact = _build_fnpack(output_dir, staging_dir, args.fnpack)
    metadata = build_fnpack.load_package_metadata()
    _validate_artifact(artifact, metadata)

    if args.upload:
        _upload(artifact, metadata["version"])

    print(f"[build_release] release ready: {artifact}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
