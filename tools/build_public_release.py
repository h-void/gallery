"""Stage the small, pure-Rust source tree published to GitHub."""

from __future__ import annotations

import argparse
import shutil
import subprocess
from pathlib import Path
from typing import Iterable


ROOT = Path(__file__).resolve().parents[1]
ROOT_FILES = frozenset(
    (
        ".dockerignore",
        ".env",
        ".gitattributes",
        ".gitignore",
        "Dockerfile",
        "Dockerfile.cuda",
        "docker-entrypoint.sh",
        "LICENSE",
        "README.md",
        "README.en.md",
        "docker-compose.yml",
        "docker-compose.gpu.yml",
        "docker-compose.cuda.yml",
        "docker-compose.launcher.yml",
        "gallery.yml",
        "start.cmd",
        "start.sh",
    )
)
TOOL_FILES = frozenset(
    (
        "tools/__init__.py",
        "tools/_fetch_ort_openvino_libs.py",
        "tools/build_fnpack.py",
        "tools/build_release.py",
        "tools/build_rust_accel.py",
        "tools/build_public_release.py",
        "tools/start_gallery.py",
    )
)
REQUIRED_FILES = frozenset(
    (
        ".env",
        ".gitattributes",
        "Dockerfile",
        "Dockerfile.cuda",
        "docker-entrypoint.sh",
        "README.md",
        "docker-compose.yml",
        "docker-compose.gpu.yml",
        "docker-compose.cuda.yml",
        "docker-compose.launcher.yml",
        "gallery.yml",
        "start.cmd",
        "start.sh",
        "tools/start_gallery.py",
        "rust/gallery_accel/Cargo.toml",
        "rust/gallery_accel/Cargo.lock",
        "app/static/index.html",
        "fnpack/package.json",
        "tools/build_public_release.py",
    )
)


def _normalize(path: str | Path) -> str:
    return str(path).replace("\\", "/").removeprefix("./")


# The private tree ships Rust unit tests (src/tests.rs, src/test_support.rs,
# src/tests/) that are stripped from the public snapshot. Their `mod`
# declarations must be stripped too, or the public tree fails `cargo test`.
_TEST_MOD_FILES = {
    "rust/gallery_accel/src/lib.rs",
    "rust/gallery_accel/src/main.rs",
}


def strip_test_mod_declarations(relative: str, text: str) -> str:
    """Remove test-module declarations (and their cfg attribute lines)."""
    if relative not in _TEST_MOD_FILES:
        return text
    kept: list[str] = []
    pending_cfg = False
    for line in text.splitlines(keepends=True):
        stripped = line.strip()
        if stripped == "#[cfg(test)]":
            # Attribute may belong to something else; remember and re-emit
            # unless the next meaningful line declares a test module.
            pending_cfg = True
            kept.append(line)
            continue
        is_test_mod = (
            stripped.startswith("mod tests;")
            or stripped.startswith("mod test_support;")
            or stripped.startswith("pub(crate) mod test_support;")
        )
        if is_test_mod:
            if pending_cfg:
                kept.pop()
            pending_cfg = False
            continue
        kept.append(line)
        pending_cfg = False
    return "".join(kept)


def sanitize_public_env(text: str) -> str:
    """Keep the release profile while dropping local active overrides."""
    return "".join(
        line
        for line in text.splitlines(keepends=True)
        if "=" not in line
        or line.lstrip().startswith("#")
        or line.strip().startswith("COMPOSE_PROFILES=")
    )


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


def _dirty_tracked_files() -> set[str]:
    """Tracked paths whose worktree content differs from the index/HEAD.

    `git status --porcelain -z` marks them ` M`, `M ` or `MM`; untracked (`??`)
    and ignored entries are not listed at all, which is the distinction that
    matters here: an untracked file is not in the selection either, so it cannot
    be published, but the *tracked* file that references it can.

    A staging root that is not itself a git repository answers "nothing dirty".
    That case is not hypothetical: `git -C <dir>` walks *up* to the enclosing
    repository, so asking about a scratch directory inside a checkout would
    otherwise report the checkout's own modifications. The gate exists for this
    repository's worktree, and no other tree needs it.
    """
    if not (ROOT / ".git").exists():
        return set()
    try:
        result = subprocess.run(
            ["git", "-C", str(ROOT), "status", "--porcelain", "-z", "--untracked-files=no"],
            check=True,
            capture_output=True,
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        return set()
    dirty: set[str] = set()
    for entry in result.stdout.split(b"\0"):
        if len(entry) < 4:
            continue
        status = entry[:2]
        path = entry[3:].decode("utf-8", errors="replace")
        if status != b"  ":
            dirty.add(path)
    return dirty


def stage_public_release(
    output: Path,
    tracked_files: Iterable[str | Path] | None = None,
    allow_dirty: bool = False,
) -> list[str]:
    """Copy selected tracked files into an empty destination directory.

    The file *list* comes from the index and the file *content* from the
    worktree, so a dirty public file would be published in a form that exists in
    no commit — and a module whose source file is still untracked makes the
    published tree uncompilable while this function reports success. The gate
    below refuses that, naming the files; `allow_dirty` is the deliberate
    override for previewing a release tree while iterating.

    `tracked_files` is an explicit selection (used by tests): it is trusted as
    given, so the gate applies to the files actually being copied.
    """
    output = Path(output).resolve()
    if output.exists() and any(output.iterdir()):
        raise FileExistsError(f"public release destination is not empty: {output}")
    output.mkdir(parents=True, exist_ok=True)

    selected = select_public_files(_tracked_files() if tracked_files is None else tracked_files)
    missing = sorted(REQUIRED_FILES.difference(selected))
    if missing:
        raise RuntimeError(f"public release is missing required files: {', '.join(missing)}")

    if not allow_dirty:
        # Only a file that is both dirty *and* part of the tree being staged can
        # be published in a state no commit contains. Without the second half
        # the gate would also refuse while the staging tool itself is being
        # edited, and would fire for callers that stage an explicit file list
        # from somewhere other than this worktree (the test suite does).
        dirty = sorted(
            path
            for path in _dirty_tracked_files().intersection(selected)
            if ROOT.joinpath(*path.split("/")).is_file()
        )
        if dirty:
            raise RuntimeError(
                "refusing to stage a public release from a dirty worktree; these "
                "files would be published in a state no commit contains: "
                + ", ".join(dirty)
                + " (commit them, or pass --allow-dirty to stage a preview)"
            )

    for relative in selected:
        source = ROOT.joinpath(*relative.split("/"))
        if not source.is_file():
            raise FileNotFoundError(f"tracked public file is missing: {source}")
        destination = output.joinpath(*relative.split("/"))
        destination.parent.mkdir(parents=True, exist_ok=True)
        if relative == ".env":
            destination.write_text(
                sanitize_public_env(source.read_text(encoding="utf-8")),
                encoding="utf-8",
                newline="",
            )
        elif relative == "gallery.yml":
            # Never publish user-entered local media paths or the selected mode.
            destination.write_text(
                "# ==============================================================================\n"
                "# 运行模式：cpu（通用兼容）、gpu（Linux Intel 核显）、cuda（NVIDIA 显卡）\n"
                "# ==============================================================================\n"
                "模式: cpu\n\n"
                "# ==============================================================================\n"
                "# 媒体目录：一行一个完整绝对路径（支持多个目录，不支持网络映射盘和 UNC 共享）\n"
                "# 目录结构：每个目录下的第一层子文件夹会自动识别为一个画师。\n"
                "#\n"
                "# 格式示例：\n"
                "# Windows：\n"
                "#   - D:/Pictures\n"
                "#   - E:/Art/Collections\n"
                "# Linux：\n"
                "#   - /home/user/pictures\n"
                "#   - /mnt/storage/art\n"
                "# ==============================================================================\n"
                "目录:\n"
                "  - D:/Pictures\n",
                encoding="utf-8",
            )
        elif relative in _TEST_MOD_FILES:
            # Strip the declarations of the removed test modules so the
            # public tree compiles (and `cargo test` passes) out of the box.
            text = source.read_text(encoding="utf-8")
            destination.write_text(
                strip_test_mod_declarations(relative, text),
                encoding="utf-8",
                newline="",
            )
        else:
            shutil.copy2(source, destination)
    return selected


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="empty directory to populate")
    parser.add_argument(
        "--allow-dirty",
        action="store_true",
        help=(
            "stage even when public files have uncommitted changes. The result is a "
            "preview: it may contain code no commit has, and it may not compile."
        ),
    )
    args = parser.parse_args()
    selected = stage_public_release(args.output, allow_dirty=args.allow_dirty)
    if args.allow_dirty:
        print("[build_public_release] WARNING: staged with changes that are not committed")
    print(f"staged {len(selected)} public files in {args.output.resolve()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
