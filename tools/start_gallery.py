"""Turn the two-field gallery.yml into a validated Compose configuration.

Python standard library only. Compose itself parses YAML, so paths and quoting
follow real YAML rules rather than a second, partial YAML implementation.
"""
from __future__ import annotations

import argparse
import copy
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SERVICES = {"cpu": "gallery", "gpu": "gallery-gpu", "cuda": "gallery-cuda"}


def docker(args, *, cwd=ROOT, input_text=None, capture=True):
    env = {k: v for k, v in os.environ.items() if not k.startswith("COMPOSE_")}
    result = subprocess.run(
        ["docker", *args], cwd=cwd, env=env, input=input_text,
        text=True, encoding="utf-8", capture_output=capture,
    )
    if result.returncode:
        raise ValueError((result.stderr or "Docker command failed").strip())
    return result.stdout


def parse_settings(text, run=docker):
    # Extension fields accept arbitrary YAML data and survive Compose's JSON output.
    wrapped = "name: gallery\nx-gallery:\n" + "".join(
        "  " + line + "\n" for line in text.splitlines()
    ) + "services: {}\n"
    parsed = json.loads(run(
        ["compose", "--env-file", os.devnull, "-f", "-", "config",
         "--no-interpolate", "--format", "json"], input_text=wrapped,
    ))["x-gallery"]
    if not isinstance(parsed, dict) or set(parsed) != {"模式", "目录"}:
        raise ValueError("gallery.yml 只填写“模式”和“目录”。")
    mode, directories = parsed["模式"], parsed["目录"]
    if not isinstance(mode, str) or mode not in SERVICES:
        raise ValueError("模式填写 cpu、gpu 或 cuda。")
    if not isinstance(directories, list) or not directories:
        raise ValueError("至少填写一个目录，一行一个。")
    if any(not isinstance(p, str) or not p.strip() for p in directories):
        raise ValueError("目录必须是非空路径。")
    return mode, directories


def validate_directories(directories):
    result = []
    for raw in directories:
        if raw.startswith(("\\\\", "//")):
            raise ValueError("请使用本地目录，不支持 UNC 或映射网络盘。")
        path = Path(raw).expanduser()
        if not path.is_absolute():
            raise ValueError(f"请填写完整目录路径：{raw}")
        if os.name == "nt":
            import ctypes
            if ctypes.windll.kernel32.GetDriveTypeW(str(path.anchor)) == 4:
                raise ValueError(f"不支持映射网络盘：{raw}")
        if not path.is_dir():
            raise ValueError(f"目录不存在：{raw}")
        value = str(path.resolve())
        if os.path.normcase(value) in {os.path.normcase(p) for p in result}:
            raise ValueError(f"目录重复：{raw}")
        result.append(value)
    return result


def assign_targets(directories, previous):
    """Keep removed roots reserved, so edits never reuse an old DB path alias."""
    mapping = dict(previous)
    if any(not isinstance(k, str) or not isinstance(v, str)
           or not re.fullmatch(r"/media(?:[2-9]|[1-9][0-9]+)?", v)
           for k, v in mapping.items()) or len(set(mapping.values())) != len(mapping):
        raise ValueError("目录映射记录无效，请保留 .gallery-launcher 并检查内容。")
    used = set(mapping.values())
    slot = 1
    for directory in directories:
        same = next((key for key in mapping if os.path.normcase(key)
                     == os.path.normcase(directory)), None)
        if same is not None:
            if same != directory:
                mapping[directory] = mapping.pop(same)
            continue
        while ("/media" if slot == 1 else f"/media{slot}") in used:
            slot += 1
        mapping[directory] = "/media" if slot == 1 else f"/media{slot}"
        used.add(mapping[directory])
    return mapping


def intel_groups():
    if not sys.platform.startswith("linux"):
        raise ValueError("Intel 核显模式仅支持原生 Linux；Windows 请用 cpu 或 cuda。")
    devices = list(Path("/dev/dri").glob("renderD*"))
    if not devices:
        raise ValueError("未找到 Intel 核显设备 /dev/dri/renderD*。")
    return sorted({str(p.stat().st_gid) for p in devices + list(Path("/dev/dri").glob("card*"))})


def render_compose(base, directories, mapping, mode, groups=None, writable=False):
    config = copy.deepcopy(base)
    config["name"] = "gallery"
    # Do not retain the YAML anchor extension with its unused legacy mounts.
    config.pop("x-gallery-base", None)
    targets = [mapping[p] for p in directories]
    for profile, name in SERVICES.items():
        service = config["services"][name]
        service["profiles"] = [profile]
        # Only replace the legacy /mediaN slots, retaining persistent state and
        # any explicitly configured non-media mounts.
        retained = [v for v in service.get("volumes", [])
                    if not re.fullmatch(r"/media[0-9]*", v["target"])]
        service["volumes"] = retained + [
            {"type": "bind", "source": p, "target": mapping[p], "read_only": not writable,
             "bind": {"create_host_path": False}} for p in directories
        ]
        service.setdefault("environment", {}).update({
            "PICTURES_ROOT": ",".join(targets),
            "PICTURES_ROOT_REAL_PATHS": ",".join(targets),
            "PICTURES_ROOT_LABELS": ",".join(t.removeprefix("/") for t in targets),
            "CHARACTER_RECOGNITION_PROVIDER": {"cpu": "cpu", "gpu": "openvino", "cuda": "cuda"}[profile],
        })
    if mode == "gpu":
        config["services"][SERVICES[mode]]["group_add"] = groups
    return config


def compose_dollars(value, *, decode=False):
    """Compose config re-escapes dollars for round trips; decode before editing."""
    if isinstance(value, str):
        return value.replace("$$", "$") if decode else value.replace("$", "$$")
    if isinstance(value, list):
        return [compose_dollars(v, decode=decode) for v in value]
    if isinstance(value, dict):
        return {k: compose_dollars(v, decode=decode) for k, v in value.items()}
    return value


def launch(compose_path, mode, *, build=False, pull=False, run=docker):
    command = ["compose", "--env-file", os.devnull, "-p", "gallery", "-f", str(compose_path)]
    service = SERVICES[mode]
    # Prepare the image before stopping another mode. Failures leave it running.
    run(command + (["build", service] if build else ["pull", "--policy", "always" if pull else "missing", service]), capture=False)
    run(command + ["stop", *[s for s in SERVICES.values() if s != service]], capture=False)
    run(command + ["up", "-d", "--no-build", service], capture=False)


def main(argv=None):
    parser = argparse.ArgumentParser(description="读取 gallery.yml 并启动 Gallery")
    parser.add_argument("--check", action="store_true", help="只检查配置")
    image_options = parser.add_mutually_exclusive_group()
    image_options.add_argument("--build", action="store_true", help="从源码构建镜像")
    image_options.add_argument("--pull", action="store_true", help="更新镜像")
    parser.add_argument("--writable", action="store_true", help="允许移入回收站和整理操作写入媒体目录")
    parser.add_argument("--config", type=Path, default=ROOT / "gallery.yml")
    args = parser.parse_args(argv)
    state_dir = ROOT / ".gallery-launcher"
    state_dir.mkdir(exist_ok=True)
    lock = state_dir / "lock"
    try:
        lock.mkdir()
    except FileExistsError:
        print("启动器已在运行；若上次被强制关闭，请删除 .gallery-launcher/lock 后重试。", file=sys.stderr)
        return 1
    candidate = state_dir / "compose.next.json"
    try:
        mode, raw = parse_settings(args.config.read_text(encoding="utf-8-sig"))
        directories = validate_directories(raw)
        groups = intel_groups() if mode == "gpu" else None
        base = compose_dollars(json.loads(docker(["compose", "-p", "gallery", "--profile", "*",
                                  "-f", str(ROOT / "docker-compose.launcher.yml"), "config", "--format", "json"])), decode=True)
        mapping_path = state_dir / "roots.json"
        if mapping_path.exists():
            previous = json.loads(mapping_path.read_text(encoding="utf-8"))
            if not isinstance(previous, dict):
                raise ValueError("目录映射记录无效：.gallery-launcher/roots.json")
        else:
            # Seed existing Compose slot assignments for users migrating from .env.
            previous = {}
            for mount in base["services"]["gallery"].get("volumes", []):
                if mount["type"] == "bind" and re.fullmatch(r"/media(?:[2-9]|[1-9][0-9]+)?", mount["target"]):
                    # Compose already emits absolute paths. Do not probe unused
                    # legacy roots (which might be slow network mappings).
                    source = os.path.normpath(mount["source"])
                    previous[source] = mount["target"]
        mapping = assign_targets(directories, previous)
        generated = render_compose(base, directories, mapping, mode, groups, args.writable)
        candidate.write_text(json.dumps(compose_dollars(generated), ensure_ascii=False, indent=2), encoding="utf-8")
        docker(["compose", "--env-file", os.devnull, "-p", "gallery", "-f", str(candidate),
                "--profile", mode, "config", "--quiet"])
        if args.check:
            print(f"配置通过：{mode}，{len(directories)} 个目录。")
            return 0
        # Persist alias reservations before any container can use them.
        pending = state_dir / "roots.next.json"
        pending.write_text(json.dumps(mapping, ensure_ascii=False, indent=2), encoding="utf-8")
        pending.replace(mapping_path)
        compose_path = state_dir / "compose.json"
        candidate.replace(compose_path)
        launch(compose_path, mode, build=args.build, pull=args.pull)
        ports = generated["services"][SERVICES[mode]].get("ports", [])
        port = next((p.get("published", "8899") for p in ports if str(p["target"]) == "8899"), "8899")
        print(f"Gallery 已启动：http://localhost:{port}/")
        return 0
    except (OSError, ValueError, KeyError) as exc:
        print(str(exc), file=sys.stderr)
        return 1
    finally:
        candidate.unlink(missing_ok=True)
        lock.rmdir()


if __name__ == "__main__":
    raise SystemExit(main())
