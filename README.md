<div align="center">

# Gallery

**基于纯 Rust 的本地媒体库与图库管理系统**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS (原生 FPK)](https://img.shields.io/badge/Platform-fnOS%20FPK-brightgreen.svg)](#部署指南)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#项目结构)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#核心特性)

[简体中文](README.md) • [English](README.en.md)

[核心特性](#核心特性) • [快速部署](#部署指南) • [配置参数](#配置参数) • [源码构建](#源码构建) • [项目结构](#项目结构) • [开源协议](#开源许可证)

</div>

---

## 项目简介

**Gallery** 是一个面向本地媒体收藏的私有化数字资产管理系统。后端以纯 Rust 编写（Axum + rusqlite + Tokio），前端为响应式单页 Web 界面。媒体库以「画师」为顶层组织边界（目录即画师），提供多维标签、AI 角色识别、BLAKE3 哈希查重与规范化目录整理能力。

---

## 核心特性

### 1. 多格式媒体浏览与流畅播放
- 图片支持 JPG/PNG/WebP/AVIF/BMP，GIF 动图悬停预览；视频支持 MP4/WebM/MOV/MKV 流播（ffmpeg 转码 / HLS）；同时管理 .txt/.md/.html 文本与 PSD/CLIP/PSB/ZIP/RAR 源文件和压缩包。
- 卡片比例可选：维护页「状态」提供 4:3 完整（默认）与 3:4 竖版两种网格缩略图比例；选择保存在当前浏览器。
- 灯箱：全屏高清、滚轮缩放、拖拽平移、移动端捏合；←/→ 切换、Esc 退出，支持下载、收藏、删除快捷键。
- URL 状态同步：画师/文件夹/标签/日期/排序/搜索实时写入地址栏，支持前进后退与分享。

### 2. 画师分区与多维标签管理
- 以画师为顶层边界（文件夹即画师），内部按子目录与多维标签组合筛选。
- 批量标注：单选、框选或全选文件夹批量加/移除标签；标签支持默认/名称/数量排序与过滤；原生拼音检索画师与标签。
- 链接索引：自动提取画师目录内文本/网盘链接与提取码；集中维护 Pixiv/Fanbox/Patreon/Twitter/X/Bilibili 等外部主页。

### 3. AI 智能角色识别（CCIP & OpenVINO）
- CCIP 角色识别默认开启，推理后端按 CUDA → OpenVINO → CPU 自动选择，模型与 CUDA 运行时自动下载；GPU 不可用时回退 CPU。
- 基于单角色作品建立参考特征库，捕捉不同画风/服装特征；内置语义去重（相似度 ≥0.95）与离群清理。
- 人工复核：AI 仅在编辑模式给出建议，经你点击确认后才写入数据库。

### 4. 内容哈希查重与失效路径追踪
- 以 BLAKE3 指纹精准定位重复文件，侧栏提供重复文件夹/文件分组视图。
- 文件重命名或移动后，通过哈希与 inode 快速重新关联；同画师明确移动自动保留标签，跨画师歧义项转入维护页待判断。

### 5. 规范化文件夹整理与归档规则引擎
- 可编辑 Default 模板（初始 {year}/{date} {tags}），先预览再执行。
- 安全保障：执行前自动 SQLite 在线备份；预演并重校验源/目标/授权边界，目标被占用时不覆盖。
- 执行记录：成功执行的计划在同一事务中移除记录，不可撤销。
- 受控自动整理：维护页「自动整理」默认关闭；开启后仅在全库扫描成功结束时自动执行高置信项，单画师/单文件夹扫描不触发。

### 6. 存储边界与数据安全
- 元数据与索引存于 SQLite；活动媒体仅在授权目录内流转，整理/归档不越界。
- 安全回收站：删除优先移入系统回收站，必要时回退 gallery/data/recycle，支持一键还原。

> [!WARNING]
> **网络与安全须知**：Gallery 默认监听 `8899` 端口，**不提供内置用户认证系统**。建议仅在家庭或可信局域网环境中使用。如需公网访问，请务必配合 fnOS 反向代理或前置 Nginx/Caddy 配置身份认证。

---

## 部署指南

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

### 方案二：Docker 容器部署（NAS / 本地电脑）

生产部署优先使用 fnOS 原生 FPK。Docker 提供两种部署方式，任选一种：

| 配置文件 | 定位与说明 |
| :--- | :--- |
| `docker-compose.yml` | **通用配置（推荐）**。单文件内置 CPU、Intel 核显、NVIDIA 三种模式切换，默认 CPU 模式，填写路径即可启动。 |
| `docker-compose.gpu.yml` | **Intel 核显独立预设**。已配置 `/dev/dri` 设备直通，填写路径直接启动。 |
| `docker-compose.cuda.yml` | **NVIDIA 显卡独立预设**。已配置 GPU 资源分配与 CUDA 镜像，填写路径直接启动。 |
| `gallery.yml` | **极简启动清单**。仅含模式和目录两项，供本地运行 `start.cmd` / `start.sh` 使用。 |
| `.env` | **运行参数配置**。修改端口、扫描周期、备份策略、AI 识别等高级参数，启动时自动读取。 |
| `docker-compose.launcher.yml` | **启动器底层模板**。已内置于项目目录供启动脚本调用，无需单独下载或编辑。 |

#### 方式 A：NAS 图形界面部署（无需 SSH 或 Python）

适用于群晖 Container Manager、飞牛 Docker、威联通 Container Station、绿联 UGOS 等 NAS 系统：

1. **创建项目**：在 NAS Docker 界面中创建“项目 / Compose”。
2. **填写路径与确认模式**：
   - 打开 `docker-compose.yml`，在 `volumes` 中将 `/volume1/photo` 修改为实际媒体目录完整路径。每个子文件夹识别为一个画师。
   - 默认启用 CPU 模式（无需显卡，兼容所有设备）。若需启用 Intel 核显或 NVIDIA 显卡，取消对应模式的注释并注释 CPU 模式（也可直接选用预设文件 `docker-compose.gpu.yml` 或 `docker-compose.cuda.yml`）。
3. **参数配置（可选）**：端口（默认 8899）、扫描与备份周期等已内置最优默认值；如需修改，在同一项目目录下编辑 `.env` 即可。
4. **启动与使用**：完成创建并启动，浏览器访问 `http://<NAS_IP>:8899/`，点击「扫描全库」。

> [!NOTE]
> - **硬件加速**：Intel 核显需要 NAS 支持 `/dev/dri` 设备直通；若遇权限不足，可在 `.env` 填入 NAS 对应的 `GALLERY_RENDER_GID` 与 `GALLERY_VIDEO_GID`。NVIDIA 需要宿主机已安装显卡驱动和 NVIDIA Container Toolkit。镜像适用 x86_64 架构。
> - **媒体安全与只读**：媒体目录默认只读挂载（`:ro`）；需要使用移入回收站或物理整理文件夹时，将挂载行尾的 `:ro` 删除后重新部署。
> - **数据持久化**：数据库、日志、备份、缓存和模型保存在 `gallery-storage` 数据卷中。升级或切换模式时保留该数据卷，切勿执行带 `-v` 的删除命令。

#### 方式 B：脚本一键启动（适合 Windows / Linux 本地电脑）

需要 Docker Desktop 或 Docker Engine 与 Docker Compose，以及 Python 3.10+（仅使用标准库，无需安装任何额外 Python 包）：

1. **获取项目**：下载仓库 ZIP 压缩包并解压（或执行 `git clone`），保留完整目录结构（启动脚本依赖的 `docker-compose.launcher.yml` 与 `tools/` 已内置其中，无需单独下载）。
2. **填写配置**：打开 `gallery.yml`，填写运行模式与本地媒体目录：

   ```yaml
   模式: cpu
   目录:
     - D:/Pictures
     - E:/Art
   ```

   一行一个目录，数量不限。Linux 请填写 `/home/user/pictures` 这样的绝对路径。Windows 路径使用正斜杠 `/`，不支持映射网络盘和 UNC 路径。每个目录下的子文件夹识别为画师。
3. **一键启动**：Windows 用户双击 `start.cmd`；Linux 用户在项目根目录下运行 `sh start.sh`。
4. **启动访问**：浏览器打开 `http://localhost:8899/`（局域网其他设备使用宿主机 IP），点击「扫描全库」。模型将在后台自动下载。

| 模式 | 适用硬件与环境 |
| :--- | :--- |
| `cpu` | 默认模式，使用 CPU 进行推理，通用兼容。 |
| `gpu` | 原生 Linux 下的 Intel 核显，自动读取并配置设备组权限（Windows Docker Desktop 不支持）。 |
| `cuda` | NVIDIA 显卡。Linux 需安装驱动与 Container Toolkit；Windows Docker Desktop 需开启 WSL2 后端并安装对应驱动。无需安装 CUDA Toolkit。 |

增加目录或切换模式后，再次运行启动脚本即可，脚本会自动更新挂载并重启服务；模式和目录以 `gallery.yml` 为准。GPU 初始化失败时默认回退 CPU，维护页显示实际 GPU/CPU 状态。

**数据与升级**：

- 数据库、日志、备份、回收站、缓存和模型保存在 `gallery-storage` 数据卷的 `data/`、`cache/`、`models/`。切换模式保留数据；`docker compose down` 不要加 `-v`，它会删除数据卷。
- 保留 `.gallery-launcher`，它记录目录映射，调整目录顺序不会改变已有路径。旧 `.env` 的媒体映射会在首次使用启动器时继承。
- 媒体默认只读。浏览、标签和识别可用；「移入回收站」和整理执行需要写权限，运行 `start.cmd --writable` 或 `sh start.sh --writable` 可启用。每次启动不带此参数会恢复只读。执行仍会校验源/目标并创建 SQLite 在线备份；数据库备份不包含媒体文件。
- 离线模型位置：数据卷内 `models/character/ccip-caformer_b36-24/model_feat.onnx`。CUDA 运行时缓存位于 `models/ort/cuda-1.24.1`，CUDA 镜像包含 CUDA 12.x / cuDNN 9.x 用户态库。
- 升级前保留独立数据库备份。更新镜像运行 `start.cmd --pull` 或 `sh start.sh --pull`；从当前源码构建运行 `start.cmd --build` 或 `sh start.sh --build`。
- 只检查配置、不启动容器：`start.cmd --check` 或 `sh start.sh --check`。

**fnOS 参数说明**：FPK 没有通用环境变量编辑界面，Docker `.env` 不适用于 FPK。`TRIM_*` 路径和端口由 fnOS 注入；其他默认参数需修改 `fnpack/cmd/main`，执行 `tools/build_rust_accel.py` 和 `tools/build_fnpack.py` 重建、安装 FPK 后重启 Gallery。

---

## 配置参数

一般无需调整。NAS 图形界面直接修改 Compose 的端口或 environment；脚本方式的端口及扫描、备份等高级参数仍可在 `.env` 修改，再运行启动脚本；模式和目录以 `gallery.yml` 为准。只有 Compose 明确引用的变量会传入容器，运行模式决定推理后端；内部数据和模型路径由镜像设置。fnOS 使用上面的重建流程。

| 环境变量 | 默认值 | 详细说明 |
| :--- | :--- | :--- |
| `CHARACTER_RECOGNITION_ENABLED` | `1` | 是否启用 AI 角色识别（1 开 / 0 关）。 |
| `CHARACTER_RECOGNITION_PROVIDER` | `auto` | NAS 配置文件和启动器按所选模式设置 cpu、openvino 或 cuda。 |
| `CHARACTER_ALLOW_CPU_FALLBACK` | `1` | GPU 初始化失败时是否回退 CPU（0 关；旧 `CHARACTER_OPENVINO_ALLOW_CPU_FALLBACK` 仍兼容）。 |
| `ONNXRUNTIME_AUTO_DOWNLOAD` | `1` | 启动后后台自动下载缺失 CCIP 模型与 CUDA 运行时（0 关，离线用）。 |
| `CHARACTER_MODEL_IDLE_TIMEOUT_SECONDS` | `600` | 模型空闲自动卸载秒数（0 常驻）。 |
| `SCAN_INTERVAL` | `21600` | 后台全库扫描周期（秒，默认 6 小时；0 关）。 |
| `HASH_INTERVAL` | `30` | 内容哈希计算轮询间隔（秒）。 |
| `DB_BACKUP_INTERVAL` | `43200` | 数据库自动备份周期（秒，默认 12 小时）。 |
| `DB_BACKUP_RETENTION` | `8` | 历史备份快照最大保留份数。 |

更多运行时变量（含默认值）见 `fnpack/cmd/main` 与 `Dockerfile`。

---

## 源码构建

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

## 项目结构

```text
gallery/
├── rust/gallery_accel/   # 核心 Rust 运行时：Axum API、SQLite 数据层、扫描/哈希流水线、CCIP 图像特征提取
├── app/static/           # 单页 Web 应用前端：HTML5 结构、styles/ 分层样式与 ES 模块 JavaScript
├── fnpack/               # fnOS 原生应用打包描述（package.json）、启动引导脚本（cmd/main）及权限配置
├── Dockerfile            # 纯 Rust Docker 镜像构建定义（默认构建 runtime 阶段）
├── Dockerfile.cuda       # CUDA 镜像构建变体（构建 runtime-cuda 阶段，供 CI 使用）
├── docker-compose.yml    # Docker Compose 编排配置（支持 CPU / 核显 / CUDA 多模式）
└── tools/                # 自动化工具链：FPK 构建、Rust 交叉编译、工作日志记录及开源公共源码生成器
```

---

## 开源许可证

本项目基于 [GNU General Public License v3.0 only](LICENSE)（`GPL-3.0-only`）开源许可协议发布。
Copyright (C) 2026 h-void.

重新分发本项目或其衍生修改版本时，必须完整提供对应源代码，并保持以 GPL-3.0-only 协议授权。

## 第三方开源许可声明

随 FPK 安装包与 Docker 镜像分发的 ONNX Runtime、OpenVINO 运行时组件以及 Rust 第三方开源依赖项的完整许可证通知请参阅 `fnpack/app/licenses/` 目录。
