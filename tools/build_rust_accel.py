from __future__ import annotations

import argparse
import os
import platform
import shlex
import shutil
import stat
import struct
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MANIFEST_RELATIVE = "rust/gallery_accel/Cargo.toml"
# Character CCIP needs official ONNX Runtime (tract cannot parse If ops).
# Binary: linux-gnu via bookworm (glibc 2.36). ORT: Microsoft manylinux
# lib (glibc ≤2.27) loaded at runtime with load-dynamic.
GNU_BINARY_TARGETED = (
    ROOT / "rust/gallery_accel/target/x86_64-unknown-linux-gnu/release/gallery_accel"
)
GNU_BINARY_NATIVE = ROOT / "rust/gallery_accel/target/release/gallery_accel"
OUTPUT_RELATIVE = "output/rust/gallery-accel"
ORT_LIBS_DIR = ROOT / "output" / "rust" / "ort-libs"
MANIFEST = ROOT / MANIFEST_RELATIVE
DEFAULT_OUTPUT = ROOT / OUTPUT_RELATIVE
# Deliberately pinned release builder. To update: resolve the desired `rust:*-bookworm`
# manifest digest, replace this value, then re-run tests/test_rust_binary_validation.py.
PODMAN_IMAGE = "docker.io/library/rust:1-bookworm@sha256:7d0723df719e7f213b69dc7c8c595985c3f4b060cfbee4f7bc0e347a86fe3b6a"
CARGO_PATH_ENV = (
    "PATH=/usr/local/cargo/bin:/usr/local/rustup/bin:"
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
)


def _windows_root_as_wsl_path() -> str:
    drive = ROOT.drive.rstrip(":").lower()
    if not drive:
        raise RuntimeError(f"cannot map Windows path to WSL path: {ROOT}")
    parts = [part for part in ROOT.parts[1:] if part not in ("\\", "/")]
    return "/mnt/" + drive + "/" + "/".join(part.replace("\\", "/") for part in parts)


def _assert_linux_x86_64_elf(path: Path) -> None:
    """Refuse to stage binaries that are not real x86_64 Linux ELF files.

    Staging a text file, a zip, a 32-bit ELF, or a binary built for the wrong
    architecture would silently produce a broken FPK. Fail closed so a release
    can never ship a stale / wrong-arch / non-ELF artifact.
    """
    if not path.is_file():
        raise FileNotFoundError(f"rust binary missing: {path}")
    data = path.read_bytes()
    if len(data) < 20:
        raise ValueError(
            f"{path.name} is only {len(data)} bytes; too small to be a valid ELF binary"
        )
    if data[0:4] != b"\x7fELF":
        raise ValueError(
            f"{path.name} is not an ELF binary (bad magic {data[0:4]!r}); "
            "refusing to stage a non-ELF artifact"
        )
    ei_class = data[4]
    ei_data = data[5]
    if ei_class != 2:  # ELFCLASS64
        raise ValueError(
            f"{path.name} is not a 64-bit ELF (EI_CLASS={ei_class}); wrong architecture"
        )
    if ei_data != 1:  # ELFDATA2LSB (little-endian)
        raise ValueError(f"{path.name} is not little-endian; unexpected ELF encoding")
    e_machine = struct.unpack_from("<H", data, 18)[0]
    if e_machine != 62:  # EM_X86_64
        raise ValueError(
            f"{path.name} targets e_machine={e_machine}, not x86_64 (62); "
            "FPK requires a Linux x86_64 binary"
        )


def _resolve_binary(*, used_target: str | None = None) -> Path:
    """Return the exact product binary for the target we just built.

    No silent fallback to another target or the host-native build: shipping a
    binary built for the wrong target produces an invalid FPK. Callers must pass
    the same ``used_target`` returned by the build step.
    """
    if used_target == "x86_64-unknown-linux-gnu":
        candidate = GNU_BINARY_TARGETED
    elif used_target == "host":
        candidate = GNU_BINARY_NATIVE
    else:
        raise ValueError(
            f"unknown build target {used_target!r}; refusing to guess a binary"
        )
    if not candidate.is_file():
        raise FileNotFoundError(
            f"cargo did not create the expected binary at {candidate} "
            f"for target {used_target}"
        )
    return candidate


def _ensure_ort_libs() -> None:
    """Extract OpenVINO-enabled ORT + GPU plugin from onnxruntime-openvino wheel."""
    so = ORT_LIBS_DIR / "libonnxruntime.so.1.24.1"
    gpu = ORT_LIBS_DIR / "libopenvino_intel_gpu_plugin.so"
    if so.is_file() and gpu.is_file():
        return
    script = ROOT / "tools" / "_fetch_ort_openvino_libs.py"
    if not script.is_file():
        raise FileNotFoundError(f"missing OpenVINO ORT extract script: {script}")
    subprocess.run([sys.executable, str(script)], check=True)
    if not so.is_file() or not gpu.is_file():
        raise FileNotFoundError(
            f"ORT/OpenVINO libs missing after fetch: need {so.name} and {gpu.name} in {ORT_LIBS_DIR}"
        )


def _wsl_has_podman() -> bool:
    probe = subprocess.run(
        ["wsl", "bash", "-lc", "command -v podman >/dev/null 2>&1"],
        check=False,
    )
    return probe.returncode == 0


def _run_via_podman_wsl(wsl_root: str) -> str:
    crate_wsl = f"{wsl_root}/rust/gallery_accel"
    image = PODMAN_IMAGE
    # Inline image name — nested PowerShell/WSL shells eat $IMAGE variables.
    command = (
        "set -eu; "
        f"podman image exists {shlex.quote(image)} || podman pull {shlex.quote(image)}; "
        "podman run --rm "
        f"-v {shlex.quote(crate_wsl)}:/src "
        "-w /src "
        f"-e {shlex.quote(CARGO_PATH_ENV)} "
        f"{shlex.quote(image)} "
        "cargo build --release --locked --target x86_64-unknown-linux-gnu"
    )
    subprocess.run(["wsl", "bash", "-lc", command], check=True)
    return "x86_64-unknown-linux-gnu"


def _run_via_host_wsl(wsl_root: str) -> str:
    command = "\n".join(
        (
            "set -eu",
            f"cd {shlex.quote(wsl_root)}",
            "export PATH=\"/root/.cargo/bin:$PATH\"",
            "cargo build --release --locked --manifest-path rust/gallery_accel/Cargo.toml",
        )
    )
    subprocess.run(["wsl", "bash", "-lc", command], check=True)
    return "host"


def _run_cargo_release_build() -> str:
    force_host = os.environ.get("GALLERY_RUST_BUILD_HOST", "").strip().lower() in (
        "1",
        "true",
        "yes",
    )

    if platform.system().lower() == "windows":
        wsl_root = _windows_root_as_wsl_path()
        if not force_host and _wsl_has_podman():
            return _run_via_podman_wsl(wsl_root)
        if force_host:
            # Explicit override only — host WSL may produce high-GLIBC binaries.
            return _run_via_host_wsl(wsl_root)
        raise RuntimeError(
            "refusing host WSL rust build (high GLIBC risk). "
            "Install podman in WSL, or set GALLERY_RUST_BUILD_HOST=1 to override."
        )

    if not force_host and shutil.which("podman"):
        src = ROOT / "rust" / "gallery_accel"
        image = PODMAN_IMAGE
        if subprocess.run(["podman", "image", "exists", image], check=False).returncode != 0:
            subprocess.run(["podman", "pull", image], check=True)
        subprocess.run(
            [
                "podman",
                "run",
                "--rm",
                "-v",
                f"{src}:/src",
                "-w",
                "/src",
                "-e",
                CARGO_PATH_ENV,
                image,
                "cargo",
                "build",
                "--release",
                "--locked",
                "--target",
                "x86_64-unknown-linux-gnu",
            ],
            check=True,
        )
        return "x86_64-unknown-linux-gnu"

    if force_host:
        cargo = os.environ.get("CARGO") or shutil.which("cargo") or "cargo"
        subprocess.run(
            [cargo, "build", "--release", "--locked", "--manifest-path", MANIFEST_RELATIVE],
            check=True,
            cwd=ROOT,
        )
        return "host"

    raise RuntimeError(
        "refusing host cargo build for product binary (use bookworm podman). "
        "Set GALLERY_RUST_BUILD_HOST=1 only for local non-FPK experiments."
    )


def _stage_ort_libs(output: Path) -> None:
    """Copy Microsoft ORT shared libs next to the staged binary. Fail if missing."""
    if not ORT_LIBS_DIR.is_dir():
        raise FileNotFoundError(f"no ORT libs dir at {ORT_LIBS_DIR}")
    staged = 0
    for lib in sorted(ORT_LIBS_DIR.iterdir()):
        if not lib.name.startswith("libonnxruntime"):
            continue
        # Copy files and preserve symlink structure when possible.
        dest = output.parent / lib.name
        if dest.exists() or dest.is_symlink():
            dest.unlink()
        if lib.is_symlink():
            # Resolve and copy real file + recreate simple name links.
            target = os.readlink(lib)
            # On Windows paths may be text targets.
            dest.symlink_to(target) if hasattr(dest, "symlink_to") else None
            try:
                if not dest.exists():
                    # Fallback: copy the resolved file as this name.
                    real = lib.resolve()
                    if real.is_file():
                        shutil.copy2(real, dest)
            except OSError:
                real = (lib.parent / target).resolve() if not Path(target).is_absolute() else Path(target)
                if real.is_file():
                    shutil.copy2(real, dest)
        elif lib.is_file():
            shutil.copy2(lib, dest)
        staged += 1
        print(f"staged ORT lib: {dest}")
    if staged == 0:
        raise FileNotFoundError(f"no libonnxruntime* files found under {ORT_LIBS_DIR}")


def build_rust_accel(output: Path = DEFAULT_OUTPUT) -> Path:
    if not MANIFEST.is_file():
        raise FileNotFoundError(f"missing Rust accel manifest: {MANIFEST}")

    _ensure_ort_libs()
    used_target = _run_cargo_release_build()
    binary = _resolve_binary(used_target=used_target)
    # Fail closed before copying/staging anything that is not the exact, valid
    # Linux x86_64 ELF we intended to build.
    _assert_linux_x86_64_elf(binary)

    output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, output)
    output.chmod(output.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    _stage_ort_libs(output)
    print(f"built artifact: {binary} (target={used_target}) -> {output}")
    return output


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Build Linux gallery-accel for fnOS FPK (gnu + Microsoft ORT for CCIP)."
    )
    parser.add_argument("--output", default=str(DEFAULT_OUTPUT))
    args = parser.parse_args()

    output = build_rust_accel(Path(args.output))
    print(f"wrote Rust accel binary: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
