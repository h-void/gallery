#!/usr/bin/env python3
"""Extract OpenVINO-capable ORT + GPU plugin libs for FPK packaging.

Uses the manylinux onnxruntime-openvino 1.24.1 wheel (same major stack as
Python requirements.txt). Skips the large OpenVINO CPU plugin (~67MB) and
the Python pybind .so. Creates SONAME symlinks via WSL so the FPK only
stores one copy of each real shared object.
"""
from __future__ import annotations

import argparse
import platform
import shutil
import subprocess
import urllib.request
import zipfile
from pathlib import Path

WHEEL_URL = (
    "https://files.pythonhosted.org/packages/50/cf/17ba72de2df0fcba349937d2788f154397bbc2d1a2d67772a97e26f6bc5f/"
    "onnxruntime_openvino-1.24.1-cp312-cp312-manylinux_2_28_x86_64.whl"
)
WHEEL_NAME = "onnxruntime_openvino-1.24.1-cp312-cp312-manylinux_2_28_x86_64.whl"

# One real file per library (SONAME links created afterward).
KEEP_EXACT = {
    "libonnxruntime.so.1.24.1",
    "libonnxruntime_providers_openvino.so",
    "libonnxruntime_providers_shared.so",
    "libopenvino.so.2025.4.1",
    "libopenvino_c.so.2025.4.1",
    "libopenvino_onnx_frontend.so.2025.4.1",
    "libopenvino_intel_gpu_plugin.so",
    "libopenvino_auto_plugin.so",
    "libopenvino_hetero_plugin.so",
    "libtbb.so.12",
    "libtbbmalloc.so",
}
SKIP_SUBSTRINGS = (
    "intel_cpu_plugin",
    "intel_npu_plugin",
    "pybind11",
    "cpython",
)

# (real_name, soname_or_alias)
SONAME_LINKS = (
    ("libonnxruntime.so.1.24.1", "libonnxruntime.so.1"),
    ("libonnxruntime.so.1", "libonnxruntime.so"),
    ("libopenvino.so.2025.4.1", "libopenvino.so.2541"),
    ("libopenvino.so.2541", "libopenvino.so"),
    ("libopenvino_c.so.2025.4.1", "libopenvino_c.so.2541"),
    ("libopenvino_c.so.2541", "libopenvino_c.so"),
    ("libopenvino_onnx_frontend.so.2025.4.1", "libopenvino_onnx_frontend.so.2541"),
    ("libopenvino_onnx_frontend.so.2541", "libopenvino_onnx_frontend.so"),
)


def _should_keep(name: str) -> bool:
    base = Path(name).name
    low = base.lower()
    if any(s in low for s in SKIP_SUBSTRINGS):
        return False
    return base in KEEP_EXACT


def _windows_path_as_wsl(path: Path) -> str:
    resolved = path.resolve()
    drive = resolved.drive.rstrip(":").lower()
    parts = [p for p in resolved.parts[1:] if p not in ("\\", "/")]
    return "/mnt/" + drive + "/" + "/".join(p.replace("\\", "/") for p in parts)


def _make_soname_links(dest: Path) -> None:
    """Create SONAME symlinks with WSL (real Linux links for FPK)."""
    if platform.system().lower() == "windows":
        wsl_dest = _windows_path_as_wsl(dest)
        # Avoid `echo a -> b` which bash treats as redirect and truncates libs!
        script_lines = ["set -eu", f"cd '{wsl_dest}'"]
        for real, link in SONAME_LINKS:
            script_lines.append(
                f"if [ -e '{real}' ] || [ -L '{real}' ]; then "
                f"ln -sfn '{real}' '{link}'; "
                f"printf 'linked %s to %s\\n' '{link}' '{real}'; fi"
            )
        subprocess.run(["wsl", "bash", "-lc", "\n".join(script_lines)], check=True)
        return

    for real, link in SONAME_LINKS:
        src = dest / real
        dst = dest / link
        if not (src.exists() or src.is_symlink()):
            continue
        if dst.exists() or dst.is_symlink():
            dst.unlink()
        dst.symlink_to(real)
        print(f"linked {link} to {real}")


def extract(dest: Path, cache_dir: Path) -> list[Path]:
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True, exist_ok=True)
    cache_dir.mkdir(parents=True, exist_ok=True)
    whl = cache_dir / WHEEL_NAME
    if not whl.is_file() or whl.stat().st_size < 1_000_000:
        print(f"downloading {WHEEL_URL}")
        urllib.request.urlretrieve(WHEEL_URL, whl)
    print(f"wheel {whl} ({whl.stat().st_size} bytes)")

    written: list[Path] = []
    with zipfile.ZipFile(whl) as zf:
        members = [
            n
            for n in zf.namelist()
            if n.startswith("onnxruntime/capi/") and _should_keep(n)
        ]
        for member in members:
            base = Path(member).name
            out = dest / base
            with zf.open(member) as src, open(out, "wb") as dst:
                shutil.copyfileobj(src, dst)
            out.chmod(out.stat().st_mode | 0o755)
            written.append(out)
            print(f"extracted {out.name} ({out.stat().st_size})")

    _make_soname_links(dest)

    (dest / "VERSION.txt").write_text(
        "source=onnxruntime-openvino==1.24.1 manylinux_2_28\n"
        "gpu=libopenvino_intel_gpu_plugin.so\n"
        "skipped=intel_cpu_plugin,intel_npu_plugin,pybind\n",
        encoding="utf-8",
    )
    return written


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument("--dest", default=str(root / "output" / "rust" / "ort-libs"))
    parser.add_argument("--cache", default=str(root / "output" / "rust" / "ortov-wheel"))
    args = parser.parse_args()
    files = extract(Path(args.dest), Path(args.cache))
    total = sum(p.stat().st_size for p in Path(args.dest).iterdir() if p.is_file() and not p.is_symlink())
    print(f"done: {len(files)} real libs, unique payload ~{total // (1024 * 1024)} MB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
