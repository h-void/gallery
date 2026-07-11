from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import stat
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SKELETON = ROOT / "fnpack"
DEFAULT_OUTPUT = ROOT / "output" / "fnpack"
PACKAGE_METADATA_FILE = SKELETON / "package.json"
RUST_ACCEL_BINARY = ROOT / "output" / "rust" / "gallery-accel"
PACKAGE_ROOT_FILES = (
    "package.json",
    "config/privilege",
    "config/resource",
    "ICON.PNG",
    "ICON_256.PNG",
    "cmd/main",
    "app/ui/images/icon_64.png",
    "app/ui/images/icon_256.png",
)
FNPACK_BINARY_ENV = "FNPACK_BINARY"

# A real fnOS FPK is a gzip stream produced by official `fnpack build`.
# A zip renamed to `.fpk` (or any other payload) is rejected by fnOS and must
# never be staged or published. `MIN_FNPACK_SIZE` rejects trivial/truncated
# payloads that only carry the magic bytes.
GZIP_MAGIC = b"\x1f\x8b"
ZIP_MAGIC = b"PK"
MIN_FNPACK_SIZE = 64


def _copy_tree(source: Path, target: Path) -> None:
    ignore = shutil.ignore_patterns(
        "__pycache__",
        "*.pyc",
        "*.pyo",
        "*.db",
        "*.db-wal",
        "*.db-shm",
        "*.log",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
    )
    shutil.copytree(source, target, ignore=ignore, dirs_exist_ok=True)


def _copy_rust_accel(staging_dir: Path) -> None:
    if not RUST_ACCEL_BINARY.is_file():
        raise FileNotFoundError(
            f"missing Rust accel binary: {RUST_ACCEL_BINARY}. "
            "Run `python tools/build_rust_accel.py` before building the FPK."
        )

    bin_dir = staging_dir / "app" / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    target = bin_dir / "gallery-accel"
    shutil.copy2(RUST_ACCEL_BINARY, target)
    target.chmod(target.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    # OpenVINO-enabled ORT + GPU plugin (prefer ort-libs with SONAME symlinks).
    ort_dir = ROOT / "output" / "rust" / "ort-libs"
    if not ort_dir.is_dir():
        ort_dir = RUST_ACCEL_BINARY.parent
    if not ort_dir.is_dir():
        raise FileNotFoundError(f"missing ORT libs directory for FPK: {ort_dir}")
    # Windows fnpack.exe cannot open Linux symlinks on DrvFS. Ship one real
    # file per unique payload under the loader SONAME (+ one ORT_DYLIB name).
    # Prefer SONAME names so the dynamic linker resolves plugins correctly.
    ship_as = {
        # real source name in ort-libs → dest name(s) in package bin/
        # One copy under SONAME; cmd/main sets ORT_DYLIB_PATH to this file.
        "libonnxruntime.so.1.24.1": ("libonnxruntime.so.1",),
        "libonnxruntime_providers_openvino.so": ("libonnxruntime_providers_openvino.so",),
        "libonnxruntime_providers_shared.so": ("libonnxruntime_providers_shared.so",),
        "libopenvino.so.2025.4.1": ("libopenvino.so.2541",),
        "libopenvino_c.so.2025.4.1": ("libopenvino_c.so.2541",),
        "libopenvino_onnx_frontend.so.2025.4.1": ("libopenvino_onnx_frontend.so.2541",),
        "libopenvino_intel_gpu_plugin.so": ("libopenvino_intel_gpu_plugin.so",),
        "libopenvino_auto_plugin.so": ("libopenvino_auto_plugin.so",),
        "libopenvino_hetero_plugin.so": ("libopenvino_hetero_plugin.so",),
        "libtbb.so.12": ("libtbb.so.12",),
        "libtbbmalloc.so": ("libtbbmalloc.so",),
    }
    missing = []
    for src_name, dest_names in ship_as.items():
        src = ort_dir / src_name
        if not src.is_file():
            # resolve through symlink aliases
            try:
                src = (ort_dir / src_name).resolve(strict=True)
            except OSError:
                missing.append(src_name)
                continue
        if not src.is_file():
            missing.append(src_name)
            continue
        for dest_name in dest_names:
            dest = bin_dir / dest_name
            if dest.exists() or dest.is_symlink():
                try:
                    dest.unlink()
                except OSError:
                    pass
            shutil.copy2(src, dest)
            print(f"fnpack staged ORT lib: {dest.name} ({dest.stat().st_size})")
    # Required core libs: fail closed if any listed provider/runtime lib is absent.
    required = set(ship_as.keys())
    hard_missing = sorted(required.intersection(missing))
    if hard_missing:
        raise FileNotFoundError(
            "FPK missing required ORT/OpenVINO libs: " + ", ".join(hard_missing)
        )


def _bool_manifest(value: bool) -> str:
    return "true" if value else "false"


def version_icon_token(version: str) -> str:
    """Cache-busting token derived from the package version.

    ``1.0.143`` -> ``v1_0_143``. Bumping ``fnpack/package.json`` version
    automatically refreshes the fnOS desktop icon filename so stale icon
    caches are bypassed without hand-editing the icon field.
    """
    return "v" + version.replace(".", "_")


def _normalize_ui_icon(metadata: dict) -> None:
    """Ensure ``ui.icon`` carries the token for the current version.

    - If the icon already contains the token for the current version, leave it.
    - Else if it contains a stale ``v\\d+_\\d+_\\d+`` token, replace it.
    - Else insert ``-{token}`` before the ``{0}`` size placeholder.

    The icon filename thus tracks the version single-source instead of a
    hand-maintained literal.
    """
    icon = metadata.get("ui", {}).get("icon", "")
    if "{0}" not in icon:
        return
    token = version_icon_token(metadata["version"])
    if token in icon:
        return
    new_icon = re.sub(r"v\d+(?:_\d+)+", token, icon)
    if new_icon != icon:
        metadata["ui"]["icon"] = new_icon
        return
    metadata["ui"]["icon"] = icon.replace("_{0}", f"-{token}_{{0}}")


def load_package_metadata(path: Path = PACKAGE_METADATA_FILE) -> dict:
    metadata = json.loads(path.read_text(encoding="utf-8"))
    required = {
        "appname",
        "version",
        "display_name",
        "desc",
        "arch",
        "platform",
        "source",
        "maintainer",
        "desktop_uidir",
        "desktop_applaunchname",
        "service_port",
        "checkport",
        "disable_authorization_path",
        "ctl_stop",
        "runtime_dependency",
        "ui",
    }
    missing = sorted(required.difference(metadata))
    if missing:
        raise KeyError("missing fnpack package metadata: " + ", ".join(missing))

    metadata["service_port"] = int(metadata["service_port"])
    for key in ("checkport", "disable_authorization_path", "ctl_stop"):
        metadata[key] = bool(metadata[key])
    _normalize_ui_icon(metadata)
    return metadata


def render_manifest(metadata: dict) -> str:
    fields = (
        ("appname", metadata["appname"]),
        ("version", metadata["version"]),
        ("display_name", metadata["display_name"]),
        ("desc", metadata["desc"]),
        ("arch", metadata["arch"]),
        ("platform", metadata["platform"]),
        ("source", metadata["source"]),
        ("maintainer", metadata["maintainer"]),
        ("desktop_uidir", metadata["desktop_uidir"]),
        ("desktop_applaunchname", metadata["desktop_applaunchname"]),
        ("service_port", metadata["service_port"]),
        ("checkport", _bool_manifest(metadata["checkport"])),
        ("disable_authorization_path", _bool_manifest(metadata["disable_authorization_path"])),
        ("ctl_stop", _bool_manifest(metadata["ctl_stop"])),
        ("install_dep_apps", metadata["runtime_dependency"]),
    )
    return "".join(f"{key}={value}\n" for key, value in fields)


def render_ui_config(metadata: dict) -> str:
    ui = metadata["ui"]
    config = {
        ".url": {
            metadata["desktop_applaunchname"]: {
                "title": ui["title"],
                "icon": ui["icon"],
                "type": "url",
                "protocol": ui["protocol"],
                "port": str(metadata["service_port"]),
                "url": ui["url"],
                "allUsers": bool(ui["allUsers"]),
            }
        }
    }
    return json.dumps(config, indent=2) + "\n"


def render_cmd_main(metadata: dict, template_path: Path | None = None) -> str:
    template = (template_path or SKELETON / "cmd" / "main").read_text(encoding="utf-8")
    rendered = (
        template.replace("__FNPACK_RUNTIME_DEPENDENCY__", metadata["runtime_dependency"])
        .replace("__FNPACK_SERVICE_PORT__", str(metadata["service_port"]))
    )
    if "__FNPACK_" in rendered:
        raise ValueError("unresolved fnpack command template placeholder")
    return rendered


def normalized_artifact_name(metadata: dict) -> str:
    return f"{metadata['appname']}_{metadata['version']}_{metadata['arch']}.fpk"


def validate_skeleton() -> None:
    missing = [
        relative
        for relative in PACKAGE_ROOT_FILES
        if not (SKELETON / relative).exists()
    ]
    if missing:
        raise FileNotFoundError("missing fnpack files: " + ", ".join(missing))


def render_generated_files(staging_dir: Path, metadata: dict) -> None:
    (staging_dir / "manifest").write_text(render_manifest(metadata), encoding="utf-8", newline="\n")
    ui_config = staging_dir / "app" / metadata["desktop_uidir"] / "config"
    ui_config.parent.mkdir(parents=True, exist_ok=True)
    ui_config.write_text(render_ui_config(metadata), encoding="utf-8", newline="\n")
    cmd_main = staging_dir / "cmd" / "main"
    cmd_main.write_text(render_cmd_main(metadata), encoding="utf-8", newline="\n")


def stage_versioned_ui_icons(staging_dir: Path, metadata: dict) -> None:
    icon_template = metadata.get("ui", {}).get("icon", "")
    if "{0}" not in icon_template:
        return

    ui_dir = staging_dir / "app" / metadata["desktop_uidir"]
    source_dir = ui_dir / "images"
    for size in ("64", "256"):
        source = source_dir / f"icon_{size}.png"
        target = ui_dir / icon_template.replace("{0}", size)
        if target == source:
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)


def stage_package(staging_dir: Path, metadata: dict | None = None) -> Path:
    metadata = metadata or load_package_metadata()
    validate_skeleton()
    if staging_dir.exists():
        shutil.rmtree(staging_dir)

    shutil.copytree(SKELETON, staging_dir)
    render_generated_files(staging_dir, metadata)
    stage_versioned_ui_icons(staging_dir, metadata)
    (staging_dir / "wizard").mkdir(parents=True, exist_ok=True)
    # Pure-Rust FPK: stage only the static UI assets, not the whole Python
    # `app/` tree, its requirements, or the legacy runtime tools.
    _copy_tree(ROOT / "app" / "static", staging_dir / "app" / "app" / "static")
    _copy_rust_accel(staging_dir)
    return staging_dir


def find_fnpack(explicit_binary: str | None = None) -> str:
    candidate = explicit_binary or os.environ.get(FNPACK_BINARY_ENV) or "fnpack"
    resolved = shutil.which(candidate)
    if resolved:
        return resolved
    path = Path(candidate)
    if path.exists():
        return str(path.resolve())
    raise FileNotFoundError(
        "fnpack CLI was not found. Install/download the official fnpack binary "
        f"or set {FNPACK_BINARY_ENV} to its path. Refusing to create a fake .fpk."
    )


def validate_fnpack_artifact(artifact: Path, metadata: dict) -> None:
    """Fail closed on an invalid FPK artifact.

    Shared by ``build_with_fnpack`` and ``build_release`` so both code paths
    enforce the same rules: correct normalized name, non-trivial size, gzip
    magic, and an explicit rejection of zip-renamed payloads.
    """
    if not artifact.is_file():
        raise FileNotFoundError(f"expected artifact missing: {artifact}")

    expected_name = normalized_artifact_name(metadata)
    if artifact.name != expected_name:
        raise ValueError(
            f"artifact name {artifact.name!r} != normalized name {expected_name!r}"
        )

    size = artifact.stat().st_size
    if size < MIN_FNPACK_SIZE:
        raise ValueError(
            f"{artifact.name} is only {size} bytes; too small to be a valid FPK "
            f"(minimum {MIN_FNPACK_SIZE})"
        )

    head = artifact.read_bytes()[:2]
    if head == ZIP_MAGIC:
        raise ValueError(
            f"{artifact.name} begins with ZIP magic bytes; fnOS rejects zip-renamed "
            "FPKs. A real fnpack build was not produced."
        )
    if head != GZIP_MAGIC:
        raise ValueError(
            f"{artifact.name} does not begin with gzip magic bytes (1f8b); "
            "unexpected FPK payload."
        )


def build_with_fnpack(staging_dir: Path, output_dir: Path, fnpack_binary: str, metadata: dict) -> Path:
    output_dir.mkdir(parents=True, exist_ok=True)
    artifact = staging_dir / f"{metadata['appname']}.fpk"
    if artifact.exists():
        artifact.unlink()

    # Must use official `fnpack build`; a zip renamed to .fpk is rejected by fnOS.
    subprocess.run(
        [fnpack_binary, "build"],
        check=True,
        cwd=staging_dir,
    )

    if not artifact.exists():
        raise FileNotFoundError(f"fnpack did not create expected artifact: {artifact}")

    final_artifact = output_dir / normalized_artifact_name(metadata)
    if final_artifact.exists():
        final_artifact.unlink()
    shutil.move(str(artifact), final_artifact)
    legacy_artifact = output_dir / artifact.name
    if legacy_artifact.exists() and legacy_artifact != final_artifact:
        legacy_artifact.unlink()

    # Reject stale / wrong / zip-renamed artifacts before reporting success —
    # do not stage or publish an invalid FPK. The name check runs against the
    # final normalized artifact, not the {appname}.fpk produced by `fnpack build`.
    try:
        validate_fnpack_artifact(final_artifact, metadata)
    except Exception:
        final_artifact.unlink(missing_ok=True)
        raise
    return final_artifact


def main() -> int:
    parser = argparse.ArgumentParser(description="Build the native fnOS FPK package.")
    parser.add_argument("--output-dir", default=str(DEFAULT_OUTPUT))
    parser.add_argument("--staging-dir", default=str(DEFAULT_OUTPUT / "stage"))
    parser.add_argument("--fnpack", default=None, help="Path to the official fnpack CLI.")
    args = parser.parse_args()

    metadata = load_package_metadata()
    staging_dir = stage_package(Path(args.staging_dir), metadata)
    fnpack_binary = find_fnpack(args.fnpack)
    artifact = build_with_fnpack(staging_dir, Path(args.output_dir), fnpack_binary, metadata)
    print(f"wrote fnpack .fpk: {artifact}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
