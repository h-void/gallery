<div align="center">

# 🖼️ Gallery

**基于纯 Rust 打造的高性能、轻量级私有化媒体库与图库管理系统**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS & Docker](https://img.shields.io/badge/Platform-fnOS%20%7C%20Docker-brightgreen.svg)](#-部署指南)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#-项目结构)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#-核心特性)

<p align="center">
  专为本地海量二次元插画、漫画、视频及原画源文件（PSD / CLIP / PSB）的收藏管理而打造。<br>
  以画师为顶层逻辑分区，支持 AI 角色识别、内容哈希查重、规范化自动整理与秒级检索。
</p>

[✨ 核心特性](#-核心特性) • [🚀 快速部署](#-部署指南) • [⚙️ 配置参数](#️-配置参数) • [🛠️ 源码构建](#️-源码构建) • [📂 项目结构](#-项目结构) • [📄 开源协议](#-开源许可证)

</div>

---

## 📖 项目简介

**Gallery** 是一个专为本地媒体收藏打造的私有化数字资产管理系统。后端采用纯 Rust 编写（基于 Axum + rusqlite + Tokio 构建），具备极致的响应速度与极低的内存占用；前端提供流畅现代的响应式单页 Web 界面，完美适配桌面宽屏与移动端触屏。

系统可直接索引用户授权的 NAS 或本地存储目录，以「画师」为顶层组织边界（目录即画师），在画师内部通过多层文件夹与多维标签进行系统化梳理，并提供基于 BLAKE3 内容哈希的精准查重、基于 OpenVINO 的 AI 角色识别建议以及具备在线备份保障的规范化目录整理引擎。

---

## ✨ 核心特性

### 1. 🎬 多格式媒体浏览与流畅播放
- **全媒体格式覆盖**：
  - **静态图像**：支持 JPG、PNG、WebP、AVIF、BMP 等主流格式；后台异步流水线生成缩略图并严格控制内存队列，实现海量图库秒级二次渲染。
  - **动态动图**：画廊网格卡片鼠标悬停自动播放 GIF 动图预览。
  - **视频流播**：支持 MP4、WebM、MOV、MKV 等视频格式；集成基于 `ffmpeg` 的流式预览、关键帧提取与 HLS 切片转码，支持在浏览器中即点即播。
  - **文本与笔记**：内置阅读器，可直接查阅与预览 `.txt`、`.md`、`.html`、`.htm` 等文本文件内容。
  - **工程源文件与压缩包**：全面索引并识别 PSD、CLIP、PSB、TGA 等创作源工程及 ZIP、RAR 压缩包。
- **全功能沉浸式灯箱（Lightbox）**：
  - 支持高清全屏大图查看、滚轮平滑缩放、鼠标拖拽平移、移动端触屏捏合手势缩放。
  - 提供全套键盘快捷键操作（`←` / `→` 切换、`Esc` 退出、快捷下载、收藏与删除）。
- **双向同步的 URL 状态机制**：
  - 画师选择、目录导航、多标签组合、日期区间、排序规则（文件名/大小/日期/最近入库）及搜索关键字均实时同步至 URL 地址栏，支持完整的浏览器前进/后退历史与一键复制分享。

### 2. 🎨 画师分区与多维标签管理
- **画师顶层逻辑隔离**：以画师作为顶层管理边界（文件夹即画师）。选中画师后，可在其专属图库内按子目录层级与多维标签进行组合筛选。
- **高效批量标注**：
  - 支持单选、鼠标拉框矩形框选（Marquee Selection）或全选文件夹，批量添加/移除标签。
  - 侧边栏标签支持按系统默认、名称字典序或作品数量降序实时排列与快速过滤。
  - 原生集成拼音检索算法，支持通过全拼、简拼快速模糊搜索画师名称与标签。
- **链接索引与主页管理**：
  - **文本与网盘链接自动提取**：自动扫描并解析画师目录内的文本文件（`.txt` / `.html` / `.htm`），智能提取其中的下载链接、网盘分享链接及配套提取码（Passcode），支持在弹窗中一键检索、分类与复制。
  - **画师社交与订阅主页管理**：提供专属的「社交与订阅」面板，方便集中记录与维护画师的外部个人主页（如 Pixiv、Fanbox、Patreon、Twitter/X、Bilibili 等平台链接）。

### 3. 🧠 AI 智能角色识别（基于 CCIP & OpenVINO）
- **硬件加速与智能建议**：
  - 默认开启基于 OpenVINO 的 AI 角色特征识别，优先调用 Intel 核显/独显 GPU 硬件加速，支持显式开启 CPU 回退模式。
  - 采用来自 Hugging Face `deepghs/ccip_onnx` 仓库的 CCIP 特征模型，下载后置于 `gallery/models/character/ccip-caformer_b36-24/model_feat.onnx`。
- **多风格特征库与质量清洗**：
  - 支持基于单角色作品建立参考特征库，有效捕捉同一角色在不同画风、服装下的特征表达。
  - 内置语义去重（SemDeDup，相似度 $\ge 0.95$）与低置信度离群点清理机制，保持特征库高质精简。
- **人工复核安全机制**：
  - AI 识别仅在编辑模式下提供建议候选，必须经由用户主动确认后方可落库，确保元数据绝对可控。

### 4. 🔍 内容哈希查重与失效路径追踪
- **BLAKE3 指纹查重**：采用高性能内容哈希算法提取文件指纹，精准定位重复文件，并在侧边栏提供重复文件夹与重复文件分组视图。
- **路径变动无感重新关联**：
  - 文件在磁盘中重命名或移动后，系统可通过哈希与 inode 快速识别。同画师内的明确移动将自动重新关联并保留标签。
  - 跨画师移动仅在整组缺失源文件与唯一目标文件哈希完全一致时自动合并标签；其余歧义项转入「维护 - 待判断」看板供人工核对。

### 5. 📁 规范化文件夹整理与归档规则引擎
- **结构化模板命名**：提供一个可编辑的 `Default` 模板，初始值为 `{year}/{date} {tags}`；预览目标后再确认执行。
- **执行安全保障**：
  - **自动备份**：执行任何文件移动前，系统均会自动调用 SQLite 在线备份 API 创建数据库快照。
  - **执行校验**：预演目标，并在执行时重新校验源路径、目标路径与授权边界；目标已占用时不会覆盖。
  - **执行记录**：当前成功计划会在同一事务中删除且无法撤销；只有保留的历史已执行计划仍支持撤销。
- **受控自动整理**：维护页面提供「自动整理」全局开关（默认关闭）。开启后，仅在一次成功的「全库扫描」结束后自动触发高置信度整理操作；单画师或单文件夹扫描绝不触发。

### 6. 🛡️ 存储边界与数据安全
- **严密存储隔离**：元数据与索引安全存储于 SQLite。活动媒体文件仅在用户授权的存储目录内流转，整理与归档操作绝不越界。
- **安全回收站**：删除作品时优先移入系统回收站，必要时安全回退至 `gallery/data/recycle`，支持一键还原至原始路径。

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
6. **启动使用**：点击 fnOS 桌面的 Gallery 图标或访问 `http://<NAS_IP>:8899/`，点击顶栏「扫描全库」即可开始初次索引。

---

### 方案二：Docker / Docker Compose 部署

在 Windows、Linux 或 macOS 环境下，可使用 Docker 进行容器化部署。

1. **准备环境**：确保本地已安装 Docker Desktop 或 Docker Engine。
2. **启动容器**：在项目根目录下设定媒体目录路径并启动服务：

```powershell
$env:GALLERY_MEDIA_DIR = 'D:\Pictures'
docker compose up -d --build
```

3. **数据持久化与权限说明**：
   - 数据库、图片缓存和模型文件将统一持久化在名为 `gallery-storage` 的 Docker 数据卷中，分别对应 `data/`、`cache/` 和 `models/` 子目录。
   - Docker 镜像默认尝试使用 OpenVINO 调用 Intel 核显 GPU 加速角色识别， GPU 不可用时自动回退到 CPU。若需显式启用 GPU 透传，请使用 `docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d --build`。
   - 请将模型文件放置在数据卷的 `models/character/ccip-caformer_b36-24/model_feat.onnx` 路径下。
   - 媒体目录默认以只读模式（`:ro`）挂载；若需要启用文件整理、归档或删除功能，请在 `docker-compose.yml` 中将挂载选项 `/media:ro` 修改为 `/media`。

---

## ⚙️ 配置参数

可在系统环境变量或 Docker Compose 中按需覆盖以下配置项：

| 环境变量 | 默认值 | 详细说明 |
| :--- | :--- | :--- |
| `SERVICE_PORT` | `8899` | HTTP Web 服务监听端口 |
| `DATA_DIR` | `data` | 数据持久化目录（存储 `gallery.db`、日志与数据库快照） |
| `IMAGE_PREVIEW_CACHE_DIR` | `cache` | 缩略图生成与视频转码切片缓存目录 |
| `CHARACTER_RECOGNITION_ENABLED` | `1` | 是否启用 AI 角色识别功能（`1` 开启 / `0` 关闭） |
| `CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK` | `1` | Docker 模式下 GPU 初始化失败时自动回退到 CPU（`0` 关闭） |
| `CHARACTER_MODEL_IDLE_TIMEOUT_SECONDS` | `600` | 角色模型空闲自动卸载超时秒数（`0` 为常驻内存） |
| `SCAN_INTERVAL` | `21600` | 后台定时全库扫描周期（秒，默认 6 小时；`0` 关闭定时扫描） |
| `HASH_INTERVAL` | `30` | 后台内容哈希计算轮询间隔（秒） |
| `DB_BACKUP_INTERVAL` | `43200` | 数据库自动在线备份周期（秒，默认 12 小时） |
| `DB_BACKUP_RETENTION` | `8` | 数据库历史备份快照最大保留份数 |

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
├── Dockerfile            # 纯 Rust Docker 容器镜像构建定义
├── docker-compose.yml    # 本地 Docker Compose 多平台编排配置
└── tools/                # 自动化工具链：FPK 构建、Rust 交叉编译、工作日志记录及开源公共源码生成器
```

---

## 📄 开源许可证

本项目基于 [GNU General Public License v3.0 only](LICENSE)（`GPL-3.0-only`）开源许可协议发布。
Copyright (C) 2026 h-void.

重新分发本项目或其衍生修改版本时，必须完整提供对应源代码，并保持以 GPL-3.0-only 协议授权。

## 📜 第三方开源许可声明

随 FPK 安装包与 Docker 镜像分发的 ONNX Runtime、OpenVINO 运行时组件以及 Rust 第三方开源依赖项的完整许可证通知请参阅 `fnpack/app/licenses/` 目录。
