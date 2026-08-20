<div align="center">

# 🖼️ Gallery

**基于纯 Rust 打造的高性能、轻量级私有化媒体库与图库管理系统**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS (原生 FPK)](https://img.shields.io/badge/Platform-fnOS%20FPK-brightgreen.svg)](#-部署指南)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#-项目结构)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#-核心特性)

[✨ 核心特性](#-核心特性) • [🚀 快速部署](#-部署指南) • [⚙️ 配置参数](#️-配置参数) • [🛠️ 源码构建](#️-源码构建) • [📂 项目结构](#-项目结构) • [📄 开源协议](#-开源许可证)

</div>

---

## 📖 项目简介

**Gallery** 是一个专为本地媒体收藏打造的私有化数字资产管理系统。后端采用纯 Rust 编写（Axum + rusqlite + Tokio），前端为响应式单页 Web 界面；以「画师」为顶层组织边界（目录即画师），支持多维标签、AI 角色识别、BLAKE3 哈希查重与规范化目录整理。

---

## ✨ 核心特性

### 1. 🎬 多格式媒体浏览与流畅播放
- 支持 JPG/PNG/WebP/AVIF/BMP 等图片、GIF 动图悬停预览、MP4/WebM/MOV/MKV 视频流播（ffmpeg 转码/HLS），以及 .txt/.md/.html 文本与 PSD/CLIP/PSB/ZIP/RAR 源文件与压缩包。
- 沉浸式灯箱：全屏高清、滚轮缩放、拖拽平移、移动端捏合；支持 ←/→ 切换、Esc 退出、下载、收藏、删除快捷键。
- URL 状态同步：画师/文件夹/标签/日期/排序/搜索实时写入地址栏，支持前进后退与分享。

### 2. 🎨 画师分区与多维标签管理
- 以画师为顶层边界（文件夹即画师），内部按子目录与多维标签组合筛选。
- 批量标注：单选、框选或全选文件夹批量加/移除标签；标签支持默认/名称/数量排序与过滤；原生拼音检索画师与标签。
- 链接索引：自动提取画师目录内文本/网盘链接与提取码；集中维护 Pixiv/Fanbox/Patreon/Twitter/X/Bilibili 等外部主页。

### 3. 🧠 AI 智能角色识别（CCIP & OpenVINO）
- 默认开启 CCIP 角色识别，推理后端自动选择 CUDA → OpenVINO → CPU，并自动下载模型与 CUDA 运行时；GPU 不可用时回退 CPU。
- 基于单角色作品建参考特征库，捕捉不同画风/服装特征；内置语义去重（相似度 ≥0.95）与离群清理。
- 人工复核：AI 仅在编辑模式给建议，须你点击确认才落库。

### 4. 🔍 内容哈希查重与失效路径追踪
- BLAKE3 指纹精准定位重复文件，侧栏提供重复文件夹/文件分组视图。
- 文件重命名/移动后通过哈希与 inode 快速重新关联；同画师明确移动自动保留标签，跨画师歧义项转入维护待判断。

### 5. 📁 规范化文件夹整理与归档规则引擎
- 可编辑 Default 模板（初始 {year}/{date} {tags}），预览后再执行。
- 安全保障：执行前自动 SQLite 在线备份；预演并重校验源/目标/授权边界，目标占用不覆盖。
- 执行记录：当前成功计划会在同一事务中删除且无法撤销。
- 受控自动整理：维护页「自动整理」默认关闭，仅全库扫描成功结束自动执行高置信项；单画师/单文件夹扫描不触发。

### 6. 🛡️ 存储边界与数据安全
- 元数据与索引存于 SQLite；活动媒体仅在授权目录内流转，整理/归档不越界。
- 安全回收站：删除优先移入系统回收站，必要时回退 gallery/data/recycle，支持一键还原。

> [!WARNING]
> **网络与安全须知**：Gallery 默认监听 `8899` 端口，**不提供内置用户认证系统**。建议仅在家庭或可信局域网环境中使用。如需公网访问，请务必配合 fnOS 反向代理或前置 Nginx/Caddy 配置身份认证。

---

## 🚀 部署指南

### 方案一：fnOS 原生 FPK 应用安装（推荐）

Gallery 为 fnOS 提供了经过深度优化的原生应用安装包，具备极高的执行性能与低资源开销。

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
5. **配置 AI 角色模型**：下载 CCIP 模型文件并放置在以下路径（如目录不存在可手动创建）：
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
docker compose up -d --build
```

3. **说明**：
   - 数据库、缓存与模型统一持久化于 `gallery-storage` 数据卷（对应 `data/`、`cache/`、`models/`）。
   - 媒体目录默认只读（`:ro`）挂载；应用只读取原文件并在自身索引/数据库中整理，**不会物理移动或重命名你的原文件**。
   - 模型放置于 `models/character/ccip-caformer_b36-24/model_feat.onnx`；如需在 fnOS 上管理媒体，请走方案一的授权目录方式。

---

## ⚙️ 配置参数

可在系统环境变量、fnOS 应用配置或 Docker Compose 中按需覆盖以下配置项：

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
├── Dockerfile            # 纯 Rust Docker 镜像构建定义（持续测试中）
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
