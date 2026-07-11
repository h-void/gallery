"""Stage the small, pure-Rust source tree published to GitHub."""

from __future__ import annotations

import argparse
import shutil
import subprocess
from pathlib import Path
from typing import Iterable


ROOT = Path(__file__).resolve().parents[1]
ROOT_FILES = frozenset((".gitattributes", ".gitignore", "README.md"))
TOOL_FILES = frozenset(
    (
        "tools/__init__.py",
        "tools/_fetch_ort_openvino_libs.py",
        "tools/build_fnpack.py",
        "tools/build_release.py",
        "tools/build_rust_accel.py",
        "tools/build_public_release.py",
    )
)
REQUIRED_FILES = frozenset(
    (
        ".gitattributes",
        "README.md",
        "rust/gallery_accel/Cargo.toml",
        "rust/gallery_accel/Cargo.lock",
        "app/static/index.html",
        "fnpack/package.json",
        "tools/build_public_release.py",
    )
)


def _normalize(path: str | Path) -> str:
    return str(path).replace("\\", "/").removeprefix("./")


def is_public_file(path: str | Path) -> bool:
    """Return whether a tracked path belongs in the public source snapshot."""
    path = _normalize(path)
    if path in ROOT_FILES or path in TOOL_FILES:
        return True
    if path.startswith("app/static/"):
        return True
    if path.startswith("fnpack/cmd/") or path.startswith("fnpack/config/"):
        return True
    if path.startswith("fnpack/app/ui/images/") and path.lower().endswith(".png"):
        return True
    if path.startswith("fnpack/app/licenses/"):
        return True
    if path in {"fnpack/package.json", "fnpack/ICON.PNG", "fnpack/ICON_256.PNG"}:
        return True
    if path in {
        "rust/gallery_accel/Cargo.toml",
        "rust/gallery_accel/Cargo.lock",
    }:
        return True
    if path.startswith("rust/gallery_accel/src/") and path.endswith(".rs"):
        relative = path.removeprefix("rust/gallery_accel/src/")
        return relative not in {"test_support.rs", "tests.rs"} and not relative.startswith("tests/")
    return False


def select_public_files(paths: Iterable[str | Path]) -> list[str]:
    """Filter tracked paths through the explicit public-release boundary."""
    return sorted({_normalize(path) for path in paths if is_public_file(path)})


def _tracked_files() -> list[str]:
    result = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "-z"],
        check=True,
        capture_output=True,
    )
    return [item.decode("utf-8") for item in result.stdout.split(b"\0") if item]


def stage_public_release(output: Path, tracked_files: Iterable[str | Path] | None = None) -> list[str]:
    """Copy selected tracked files into an empty destination directory."""
    output = Path(output).resolve()
    if output.exists() and any(output.iterdir()):
        raise FileExistsError(f"public release destination is not empty: {output}")
    output.mkdir(parents=True, exist_ok=True)

    selected = select_public_files(_tracked_files() if tracked_files is None else tracked_files)
    missing = sorted(REQUIRED_FILES.difference(selected))
    if missing:
        raise RuntimeError(f"public release is missing required files: {', '.join(missing)}")

    for relative in selected:
        source = ROOT.joinpath(*relative.split("/"))
        if not source.is_file():
            raise FileNotFoundError(f"tracked public file is missing: {source}")
        destination = output.joinpath(*relative.split("/"))
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
    return selected


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="empty directory to populate")
    args = parser.parse_args()
    selected = stage_public_release(args.output)
    print(f"staged {len(selected)} public files in {args.output.resolve()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
