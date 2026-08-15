# Gallery

Gallery 是一个本地图库，后端用 Rust 编写，可作为 fnOS 原生 FPK 包或 Docker 容器运行。它索引接入的媒体目录，按画师划分图库，并在每位画师下按文件夹与标签整理图片、视频和源文件。

## 功能

- **浏览与预览**：支持图片、视频（可预览并直接播放）、源文件、文本（`.txt`/`.md`/`.html`）以及压缩包的查看，并可按日期范围、名称、大小或最近入库排序筛选；浏览条件会写入 URL，支持分享和浏览器前进后退。
- **整理方式**：图库按画师划分（文件夹即画师）。选定一位画师后，可在其图库内按文件夹与标签浏览和筛选。
- **角色识别**：默认开启 AI 角色识别（基于 OpenVINO 在 GPU 上运行，可显式启用 CPU 回退）；从 Hugging Face 的 `deepghs/ccip_onnx` 获取模型，并放入 `gallery/models/character/ccip-caformer_b36-24/model_feat.onnx`。画师识别默认关闭——文件夹本身已按画师划分，无需额外识别。
- **查重与路径监测**：基于内容哈希检测重复文件；监测媒体路径是否失效，对同画师范围内的明确移动自动重新关联。跨画师候选仅在整组缺失源与唯一目标具有相同非空哈希时自动迁移并合并标签，其余不确定情况待人工确认。
- **归档计划**：可将散落的内容归拢为归档计划，确认后再执行（执行前会先创建备份）。自动执行默认关闭，仅在开启开关且一次完整扫描成功完成后触发。
- **索引与文件归属**：索引存于 SQLite。活动媒体位于你授权的 fnOS 媒体目录内；整理与归档只在这些目录内移动文件。删除时优先使用 fnOS 回收站，必要时回退到 `gallery/data/recycle`。

服务默认监听 `8899` 端口，**不提供内置登录认证**。请仅在可信局域网内使用；如需暴露到公网，应在 fnOS 或反向代理层先加上认证。标签与画师搜索支持拼音。

## 安装

从 [Releases](https://github.com/hczhr/gallery-archive/releases) 下载最新发布的 `gallery_<version>_x86_64.fpk`，通过 fnOS 应用安装器安装。

源码仓库和 FPK **不包含**角色模型、数据库与缓存；FPK 只提供运行时，模型需自行取得并放到上述路径。

### Docker

Windows、Linux 和 macOS 可使用 Docker Desktop 或 Docker Engine 运行同一份镜像。先准备媒体目录，然后在仓库根目录执行：

```powershell
$env:GALLERY_MEDIA_DIR = 'D:\Pictures'
docker compose up -d --build
```

容器默认监听 `http://localhost:8899/`，数据、预览缓存和模型分别保存到 Docker volumes。Docker 默认使用 CPU 角色识别；把模型放到 `gallery-models` volume 的 `character/ccip-caformer_b36-24/model_feat.onnx`。媒体目录默认以只读方式挂载；需要整理、归档或删除文件时，将 `docker-compose.yml` 中的 `/media:ro` 改为 `/media`，并确认这是有意授权。

## 构建

Windows 发布构建需要 WSL 2，并在 WSL 内安装 Podman；构建过程可能联网下载固定构建镜像与 ONNX Runtime 库。

一键构建（先编译 Rust 运行时，再打包 FPK）：

```powershell
python tools/build_release.py --fnpack .\output\fnpack\fnpack-1.2.3-windows-amd64.exe
```

仅编译用于 fnOS 的 Linux Rust 运行时：

```powershell
python tools/build_rust_accel.py
```

## 目录结构

| 路径 | 说明 |
| --- | --- |
| `rust/gallery_accel/` | Rust 运行时与 Axum API |
| `app/static/` | 网页界面 |
| `fnpack/` | fnOS 安装包配置与启动脚本 |
| `Dockerfile` / `docker-compose.yml` | Docker 镜像与本地编排配置 |
| `tools/` | 构建 Rust、打包 FPK、生成公开源码树的工具 |

## 许可证

本项目以 [GNU General Public License v3.0 only](LICENSE)（`GPL-3.0-only`）发布。
Copyright (C) 2026 hczhr。
重新分发本项目或其修改版本时，必须同时提供对应源码，并继续以 GPL-3.0-only 授权。

## 第三方许可证

FPK 和 Docker 镜像随附的 ONNX Runtime 及 Rust 依赖许可证和通知位于
`fnpack/app/licenses/`。
