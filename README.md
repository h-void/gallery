<div align="center">

# 🖼️ Gallery

**基于纯 Rust 打造的高性能、轻量级私有化媒体库与图库管理系统**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS (原生 FPK)](https://img.shields.io/badge/Platform-fnOS%20FPK-brightgreen.svg)](#-部署指南)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#-项目结构)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#-核心特性)

[简体中文](README.md) • [English](README.en.md)

[✨ 核心特性](#-核心特性) • [🚀 快速部署](#-部署指南) • [⚙️ 配置参数](#️-配置参数) • [🛠️ 源码构建](#️-源码构建) • [📂 项目结构](#-项目结构) • [📄 开源协议](#-开源许可证)

</div>

---

## 📖 项目简介

**Gallery** 是一个面向本地媒体收藏的私有化数字资产管理系统。后端以纯 Rust 编写（Axum + rusqlite + Tokio），前端为响应式单页 Web 界面。媒体库以「画师」为顶层组织边界（目录即画师），提供多维标签、AI 角色识别、BLAKE3 哈希查重与规范化目录整理能力。

---

## ✨ 核心特性

### 1. 🎬 多格式媒体浏览与流畅播放
- 图片支持 JPG/PNG/WebP/AVIF/BMP，GIF 动图悬停预览；视频支持 MP4/WebM/MOV/MKV 流播（ffmpeg 转码 / HLS）；同时管理 .txt/.md/.html 文本与 PSD/CLIP/PSB/ZIP/RAR 源文件和压缩包。
- 沉浸式灯箱：全屏高清、滚轮缩放、拖拽平移、移动端捏合；←/→ 切换、Esc 退出，支持下载、收藏、删除快捷键。
- URL 状态同步：画师/文件夹/标签/日期/排序/搜索实时写入地址栏，支持前进后退与分享。

### 2. 🎨 画师分区与多维标签管理
- 以画师为顶层边界（文件夹即画师），内部按子目录与多维标签组合筛选。
- 批量标注：单选、框选或全选文件夹批量加/移除标签；标签支持默认/名称/数量排序与过滤；原生拼音检索画师与标签。
- 链接索引：自动提取画师目录内文本/网盘链接与提取码；集中维护 Pixiv/Fanbox/Patreon/Twitter/X/Bilibili 等外部主页。

### 3. 🧠 AI 智能角色识别（CCIP & OpenVINO）
- CCIP 角色识别默认开启，推理后端按 CUDA → OpenVINO → CPU 自动选择，模型与 CUDA 运行时自动下载；GPU 不可用时回退 CPU。
- 基于单角色作品建立参考特征库，捕捉不同画风/服装特征；内置语义去重（相似度 ≥0.95）与离群清理。
- 人工复核：AI 仅在编辑模式给出建议，经你点击确认后才写入数据库。

### 4. 🔍 内容哈希查重与失效路径追踪
- 以 BLAKE3 指纹精准定位重复文件，侧栏提供重复文件夹/文件分组视图。
- 文件重命名或移动后，通过哈希与 inode 快速重新关联；同画师明确移动自动保留标签，跨画师歧义项转入维护页待判断。

### 5. 📁 规范化文件夹整理与归档规则引擎
- 可编辑 Default 模板（初始 {year}/{date} {tags}），先预览再执行。
- 安全保障：执行前自动 SQLite 在线备份；预演并重校验源/目标/授权边界，目标被占用时不覆盖。
- 执行记录：成功执行的计划在同一事务中移除记录，不可撤销。
- 受控自动整理：维护页「自动整理」默认关闭；开启后仅在全库扫描成功结束时自动执行高置信项，单画师/单文件夹扫描不触发。

### 6. 🛡️ 存储边界与数据安全
- 元数据与索引存于 SQLite；活动媒体仅在授权目录内流转，整理/归档不越界。
- 安全回收站：删除优先移入系统回收站，必要时回退 gallery/data/recycle，支持一键还原。

> [!WARNING]
> **网络与安全须知**：Gallery 默认监听 `8899` 端口，**不提供内置用户认证系统**。建议仅在家庭或可信局域网环境中使用。如需公网访问，请务必配合 fnOS 反向代理或前置 Nginx/Caddy 配置身份认证。

---

## 🚀 部署指南

### 方案一：fnOS 原生 FPK 应用安装（推荐）

Gallery 为 fnOS 提供原生应用安装包（FPK）：单一 Rust 二进制，无 Python 运行时依赖，资源占用低。

1. **获取安装包**：从 [Releases](https://github.com/h-void/gallery/releases) 下载最新发布的 `gallery_<version>_x86_64.fpk` 文件。
2. **应用中心安装**：进入 fnOS Web 管理界面，打开「应用中心」选择手动安装，上传 `.fpk` 文件。
3. **媒体目录授权**：在安装向导中为 Gallery 勾选并授权你要管理的媒体文件夹（系统将自动挂载映射）。
4. **持久化存储结构**：安装后，系统会在 `@appshare/gallery` 下自动维护持久化数据：
   ```text
   @appshare/gallery/
   ├── data/      # 核心数据库 gallery.db、运行日志 logs/、在线备份 db-backups/、回收站 recycle/
   ├── cache/     # 缩略图缓存 transcode-cache/ 及视频切片
   └── models/    # AI 角色识别模型及 OpenVINO 缓存
   ```
5. **配置 AI 角色模型**：CCIP 模型默认在启动后后台自动下载；离线环境可手动下载并放置到以下路径（目录不存在可创建）：
   ```text
   gallery/models/character/ccip-caformer_b36-24/model_feat.onnx
   ```
6. **启动使用**：访问 `http://<NAS_IP>:8899/`，点击顶栏「扫描全库」即可开始初次索引。

---

### 方案二：Docker / Docker Compose 部署（持续测试中）

> ⚠️ **生产部署请优先使用方案一（fnOS 原生 FPK）**。Docker 方式仍在持续测试与维护；部署前请按当前 `Dockerfile` 与 Compose 配置验证环境。

1. **准备环境**：安装 Docker Desktop 或 Docker Engine。
2. **启动容器**：在项目根目录设定媒体目录并启动：

```powershell
$env:GALLERY_MEDIA_DIR = 'D:\Pictures'
docker compose up -d
```

本地没有镜像时，Compose 自动从镜像仓库拉取已构建的 `stable` 镜像，无需本地编译；修改源码后才需要 `--build` 重建，升级到新发布的镜像用 `docker compose pull` 后再 `docker compose up -d`。

普通 NAS 或 Windows 部署只使用默认的 `gallery` 服务；它不要求 `/dev/dri` 或 GPU 权限。`gallery-gpu` 与 `gallery-cuda` 均为可选 profile，不影响普通部署。

**GPU 支持矩阵**：

| 宿主机环境 | 可用方案 | 宿主机需要 |
| :--- | :--- | :--- |
| 原生 Linux + NVIDIA | `cuda` profile | NV 驱动 + NVIDIA Container Toolkit |
| Windows Docker Desktop + NVIDIA | `cuda` profile | 仅 NV 驱动（WSL2 支持内置于驱动） |
| 原生 Linux + Intel 核显 | `gpu` profile | `/dev/dri` 直通与正确的组 GID |
| Windows + 仅 Intel 核显 | 不支持 GPU | 使用默认 `gallery`（CPU 推理） |

**Intel 核显（`gpu` profile，仅原生 Linux）**：该 profile 把宿主机 `/dev/dri` 设备映射进容器，并把进程加入设备所属的 `render`/`video` Linux 组。`GALLERY_RENDER_GID` 和 `GALLERY_VIDEO_GID` 是这两个组的数字 GID，不是 GPU 型号，不同宿主机可能不同：先在 Linux 宿主机执行 `stat -c '%n gid=%g' /dev/dri/renderD128 /dev/dri/card0` 查询，再把结果写入项目 `.env`（例如 `GALLERY_RENDER_GID=109`、`GALLERY_VIDEO_GID=44`）。Windows Docker Desktop（WSL2 后端）没有 `/dev/dri` 设备，核显在该环境不可用。

```powershell
docker compose stop gallery
docker compose --profile gpu up -d --build gallery-gpu
```

**NVIDIA CUDA（`cuda` profile）**：不需要手动填写 GPU 编号。宿主机需要 NVIDIA Container Toolkit（Windows Docker Desktop 需要正常的 WSL2 NVIDIA 支持）：

```powershell
docker compose stop gallery
docker compose --profile cuda up -d --build gallery-cuda
```

`gallery-cuda` 使用独立的 `runtime-cuda` 镜像目标，内置 CUDA 12.x 与 cuDNN 9.x 用户态库，宿主机只需兼容的 NVIDIA 驱动，无需安装 CUDA Toolkit。镜像只打包 ONNX Runtime CUDA 提供器实际链接的五个 NVIDIA 库（cudart、cublas、cuDNN、cufft、curand）；仓库根目录的 `Dockerfile.cuda` 与 `Dockerfile` 逐 stage 一致、仅最后一个构建目标互换，供固定构建最后一个 stage 的 CI 服务使用。GPU 检测同时支持原生 `/dev/nvidia*` 节点与 WSL2/Docker Desktop 的 `/dev/dxg` 加注入驱动方式。Gallery 按 fnOS 相同顺序自动选择 CUDA → OpenVINO → CPU，并把校验过的 ONNX Runtime CUDA 运行时下载到 `gallery-storage` 的 `models/ort/cuda-1.24.1`。镜像仓库与源码同步发布 `stable-cuda` 标签；该标签可用时，上述命令可省略 `--build` 直接拉取。没有 NVIDIA GPU 时不要启动此 profile，直接使用默认 `gallery`。

**持久化与运行参数**：
- 数据库、缓存与模型统一持久化于 `gallery-storage` 数据卷（对应 `data/`、`cache/`、`models/`）。
- 媒体目录默认只读（`:ro`）挂载；应用只读取原文件并在自身索引/数据库中整理，**不会物理移动或重命名你的原文件**。
- 常用运行参数已写入 `docker-compose.yml` 的 `environment`，可直接改 yml；也可在项目根目录 `.env` 或主机环境中用同名变量覆盖，例如 `CHARACTER_RECOGNITION_PROVIDER=cpu`、`SCAN_INTERVAL=0`。`DATA_DIR` 固定为容器内 `/gallery/data`，回收站固定为 `/gallery/data/recycle`。
- 模型放置于 `models/character/ccip-caformer_b36-24/model_feat.onnx`；如需在 fnOS 上管理媒体，请走方案一的授权目录方式。

**fnOS 参数说明**：当前 FPK 没有通用的环境变量编辑界面，Docker 的 `.env` 不能直接用于 FPK。`TRIM_*` 路径和端口变量由 fnOS 注入；要修改其他默认运行参数（例如识别后端、扫描周期、备份周期），需要修改 `fnpack/cmd/main` 的默认值，重新执行 `tools/build_rust_accel.py` 和 `tools/build_fnpack.py`，安装新的 FPK 后重启 Gallery。

---

## ⚙️ 配置参数

以下参数适用于运行时：Docker 可在 Compose 的 `environment`、项目 `.env` 或宿主机环境中覆盖；fnOS FPK 当前没有通用变量编辑入口，需按上面的流程重建 FPK 后生效：

| 环境变量 | 默认值 | 详细说明 |
| :--- | :--- | :--- |
| `SERVICE_PORT` | `8899` | HTTP 服务监听端口。 |
| `DATA_DIR` | `data` | 数据持久化目录（gallery.db、日志、备份）。 |
| `IMAGE_PREVIEW_CACHE_DIR` | `cache` | 缩略图与转码切片缓存目录。 |
| `CHARACTER_RECOGNITION_ENABLED` | `1` | 是否启用 AI 角色识别（1 开 / 0 关）。 |
| `CHARACTER_RECOGNITION_PROVIDER` | `auto` | 推理后端自动选择：CUDA → OpenVINO → CPU。 |
| `CHARACTER_ALLOW_CPU_FALLBACK` | `1` | GPU 初始化失败时是否回退 CPU（0 关；旧 `CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK` 仍兼容）。 |
| `ONNXRUNTIME_AUTO_DOWNLOAD` | `1` | 启动后后台自动下载缺失 CCIP 模型与 CUDA 运行时（0 关，离线用）。 |
| `CHARACTER_MODEL_IDLE_TIMEOUT_SECONDS` | `600` | 模型空闲自动卸载秒数（0 常驻）。 |
| `SCAN_INTERVAL` | `21600` | 后台全库扫描周期（秒，默认 6 小时；0 关）。 |
| `HASH_INTERVAL` | `30` | 内容哈希计算轮询间隔（秒）。 |
| `DB_BACKUP_INTERVAL` | `43200` | 数据库自动备份周期（秒，默认 12 小时）。 |
| `DB_BACKUP_RETENTION` | `8` | 历史备份快照最大保留份数。 |

---

## 🛠️ 源码构建

### 1. 构建前置要求（Windows 环境）
- 启用 **WSL 2**，并在 WSL 2 内部安装 **Podman**（构建脚本将调用隔离的 Debian Bookworm 容器进行 Linux 交叉编译）。
- 准备好官方 `fnpack` 工具（如未安装，可下载 `output/fnpack/fnpack-1.2.3-windows-amd64.exe`）。

### 2. 一键编译与 FPK 打包
在 Windows PowerShell 终端中执行一键发布脚本（将自动完成 Linux 纯 Rust 运行时交叉编译与 FPK 标准打包）：

```powershell
python tools/build_release.py --fnpack .\output\fnpack\fnpack-1.2.3-windows-amd64.exe
```

### 3. 仅编译 Linux Rust 运行时
若仅需编译适用于 fnOS 的底层 Linux 可执行程序：

```powershell
python tools/build_rust_accel.py
```

编译生成的二进制文件将输出至 `app/bin/gallery-accel`。

---

## 📂 项目结构

```text
gallery/
├── rust/gallery_accel/   # 核心 Rust 运行时：Axum API、SQLite 数据层、扫描/哈希流水线、CCIP 图像特征提取
├── app/static/           # 单页 Web 应用前端：HTML5 结构、全局样式（style.css）与模块化 JavaScript 脚本
├── fnpack/               # fnOS 原生应用打包描述（package.json）、启动引导脚本（cmd/main）及权限配置
├── Dockerfile            # 纯 Rust Docker 镜像构建定义（持续测试中；默认目标为普通 runtime）
├── Dockerfile.cuda       # CUDA 镜像构建变体（与 Dockerfile 仅最后一个构建目标互换，供 CI 使用）
├── docker-compose.yml    # Docker 编排配置（持续测试中；生产优先使用 fnOS FPK）
└── tools/                # 自动化工具链：FPK 构建、Rust 交叉编译、工作日志记录及开源公共源码生成器
```

---

## 📄 开源许可证

本项目基于 [GNU General Public License v3.0 only](LICENSE)（`GPL-3.0-only`）开源许可协议发布。
Copyright (C) 2026 h-void.

重新分发本项目或其衍生修改版本时，必须完整提供对应源代码，并保持以 GPL-3.0-only 协议授权。

## 📜 第三方开源许可声明

随 FPK 安装包与 Docker 镜像分发的 ONNX Runtime、OpenVINO 运行时组件以及 Rust 第三方开源依赖项的完整许可证通知请参阅 `fnpack/app/licenses/` 目录。
