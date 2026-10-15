<div align="center">

# Gallery

**基于纯 Rust 的私有化本地媒体库与画师作品管理系统**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS (原生 FPK)](https://img.shields.io/badge/Platform-fnOS%20FPK-brightgreen.svg)](#部署指南)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#项目结构)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#核心特性)

[简体中文](README.md) • [English](README.en.md)

[核心特性](#核心特性) • [部署指南](#部署指南) • [配置参数](#配置参数) • [源码构建](#源码构建) • [项目结构](#项目结构) • [开源协议](#开源许可证)

</div>

---

## 项目简介

**Gallery** 是一款面向本地创作者作品与插画收藏的私有化数字资产管理系统。后端完全采用 Rust 编写（Axum + rusqlite + Tokio），无 Python 运行时依赖，搭配响应式单页 Web 界面。系统以**「画师」为顶层组织边界（目录即画师）**，集成了多维标签管理、AI 角色识别、BLAKE3 内容哈希查重、规范化目录归档以及创作者订阅与网盘派发能力。

---

## 核心特性

- **多格式浏览与中英文双语**
  - **全格式支持**：原生支持 JPG/PNG/WebP/AVIF/BMP 图片、GIF 悬停动图预览、MP4/WebM/MOV/MKV 视频流播（`ffmpeg` 转码 / HLS），同时索引 `.txt`/`.md`/`.html` 文本及 PSD/CLIP/PSB 等源文件。
  - **压缩包免解压预览与批量解压**：点击 ZIP/RAR/7Z 卡片即可直接查看包内目录树、文件大小并预览封面（单条目严格内存上限防护，支持键盘翻页与序号）；支持多选或按画师一键批量解压，提供独立目录模式、解压密码自动尝试与会话记忆，原压缩包自动移入回收站（采用 7z 静默多线程与顶部三列扫描/解压进度栏联动）。
  - **视图交互与拖拽整理**：支持 `4:3 完整` 与 `3:4 竖版` 网格、高密度单行列表，以及具备 1:1 零延迟平移、缩放重置与一键定位文件夹的全屏灯箱。卡片支持直接拖拽至左侧文件夹树移动或拖拽至角色行打标；文件夹树支持父级折叠展开与拼音快速过滤；全站支持 `Shift + 点击` 连续范围多选。所有筛选、排序与浏览状态实时同步至 URL 地址栏。
  - **中英文即时切换**：全站原生接入多语言体系，界面、维护页、下载管理与弹窗支持中英文一键无刷新切换，偏好持久化保存。
- **画师分区与多维标签**
  - **目录即画师**：媒体根目录下的第一层子文件夹自动识别为画师，内部支持按子目录树、多标签组合及「全部 / 待整理 / 已归档」状态筛选。
  - **高效标注与检索**：支持单选、框选、按文件夹批量加减标签，侧栏置顶「未加角色」待办入口；画师与标签均支持原生拼音搜索。
  - **链接自动索引**：自动提取画师目录内文本中的网页链接、网盘地址与提取码，并支持集中管理 Pixiv、Fanbox、Patreon、Twitter/X、Bilibili 等画师主页。
- **AI 智能角色识别（CCIP）**
  - **自动硬件加速**：默认启用 CCIP 角色识别，启动后后台自动准备模型与运行时，按 `CUDA → OpenVINO → CPU` 自动选择最佳推理后端（GPU 不可用时自动回退 CPU）。
  - **特征库与人工复核**：基于单角色作品建立参考特征库（内置 ≥0.95 语义去重与离群清理），支持手动新建角色与上传参考图；AI 仅在选中作品时提供建议，经用户确认后写入数据库。
- **内容哈希查重与移动追踪**
  - **BLAKE3 精准查重**：提供重复文件夹与重复文件分组视图，后台完成后静默刷新列表。
  - **移动自动重连**：文件在库外重命名或移动后，结合 BLAKE3 哈希与 inode 自动恢复关联；同画师移动自动保留标签，跨画师冲突转入维护页人工复核。
- **规范化文件夹整理与安全回收站**
  - **规则引擎与就地归档**：内置可编辑 `Default` 命名模板（默认 `{year}/{date} {tags}`，缺失日期自动按 Twitter/X 雪花 ID 反算发帖日）。支持侧栏就地补齐角色与日期、编辑栏一键「整理归档」当前目录、多选文件原子「打包文件夹」，以及命名规则迁移预览与显式版本切换（CAS 并发保护）。
  - **多重数据安全**：整理执行前自动创建 SQLite 在线备份并二次校验路径授权，绝不覆盖已有文件，失败自动回滚；支持开启全库扫描后「自动整理」。
  - **两阶段回收站与策略自选**：支持在维护页自选「飞牛系统回收站」或「应用内部回收站」（附优缺点对比卡片），支持文件夹级删除回收、网格/表格双视图切换、一键批量还原、单条彻底删除与全量清空。
- **创作者订阅与网盘批量派发**
  - **订阅调度与覆盖率日历**：订阅创作者主页后按画师分轮次自动发现并下载新作品；订阅卡片内置 53 周下载覆盖率日历，直观展示各周缺口。
  - **全部作品与 JDownloader 桥接**：提供跨画师「全部作品」筛选与快照下载；外链可通过 Event Scripter 本地桥接一键批量投递至 JDownloader，自动入库与握手加固。

> [!WARNING]
> **网络安全须知**：Gallery 默认监听 `8899` 端口，**不包含内置用户登录认证**，仅适用于家庭或可信局域网（LAN）。如需公网访问，请务必通过 fnOS 反向代理或前置 Nginx / Caddy 配置身份认证。

---

## 部署指南

### 方案一：飞牛 fnOS 原生应用安装（推荐）

Gallery 提供原生 fnOS 安装包（FPK），单一 Rust 二进制直接运行于宿主机，资源占用最低且开箱内置 `7zz` 解压引擎。

1. **下载并安装**：从 [Releases](https://github.com/h-void/gallery/releases) 下载 `gallery_<version>_x86_64.fpk`，在 fnOS「应用中心」选择手动安装并上传。
2. **授权媒体目录**：在安装向导中勾选需要交由 Gallery 管理的媒体文件夹。
3. **启动并扫描**：浏览器访问 `http://<NAS_IP>:8899/`，点击顶栏「扫描全库」建立初始索引。

安装后，应用数据会自动持久化在 `@appshare/gallery` 下（与媒体目录严格隔离）：

```text
@appshare/gallery/
├── data/      # 核心数据库 gallery.db、运行日志 logs/、在线备份 db-backups/、应用回收站 recycle/
├── cache/     # 缩略图缓存、视频抽帧与 HLS 转码切片 transcode-cache/
└── models/    # AI 角色识别模型、OpenVINO 缓存与 CUDA 运行时缓存
```

> [!TIP]
> **离线环境模型放置**：CCIP 模型默认在启动后后台自动下载。若 NAS 处于纯离线环境，可将模型手动放置于 `gallery/models/character/ccip-caformer_b36-24/model_feat.onnx`（详见 [docs/FNOS_NATIVE.md](docs/FNOS_NATIVE.md)）。

---

### 方案二：Docker 部署（NAS 图形界面 / 本地电脑）

适用于群晖 Container Manager、威联通 Container Station、绿联 UGOS、飞牛 Docker 以及 Windows / Linux 本地电脑。提供以下两种启动方式：

#### 方式 A：本地电脑一键脚本启动（Windows / Linux）

仅需安装 Docker（含 Docker Compose）与 Python 3.10+（仅用标准库，无需安装任何第三方包）：

1. **配置目录与模式**：解压仓库后打开 `gallery.yml`，填入运行模式（`cpu` / `gpu` / `cuda`）与本地媒体目录绝对路径（Windows 路径统一用 `/`，每个目录下的第一层子文件夹会被识别为画师）：
   ```yaml
   模式: cpu
   目录:
     - D:/Pictures
     - E:/Art
   ```
2. **一键启动**：Windows 双击 `start.cmd`，Linux 运行 `sh start.sh`，随后访问 `http://localhost:8899/`。
3. **常用启动参数**（调整目录或切换模式后直接重跑脚本即可自动更新挂载）：
   - `start.cmd --writable`（或 `sh start.sh --writable`）：开启媒体目录写权限（用于移入回收站和文件夹整理；不带此参数启动会自动恢复默认只读）
   - `start.cmd --pull` / `--build`：拉取最新镜像 / 从本地源码构建镜像
   - `start.cmd --check`：仅校验配置，不启动容器

#### 方式 B：NAS 图形界面 Compose 部署（无需 SSH）

1. **选用配置文件**：项目内置了三套开箱即用的 Compose 文件：
   - `docker-compose.yml`：通用配置（默认启用 CPU，文件内含 Intel 核显与 NVIDIA 切换注释）
   - `docker-compose.gpu.yml`：Intel 核显独立预设（已配置 `/dev/dri` 直通）
   - `docker-compose.cuda.yml`：NVIDIA 显卡独立预设（已配置 GPU 资源与 CUDA 运行时）
2. **修改挂载路径**：将 Compose 文件 `volumes` 下的 `/volume1/photo` 替换为 NAS 上的真实媒体目录路径（如需修改默认 `8899` 端口或扫描周期，可在同目录 `.env` 中配置）。
3. **启动服务**：在 NAS Docker 界面启动项目，浏览器访问 `http://<NAS_IP>:8899/` 并点击「扫描全库」。

| 运行模式 | 适用硬件与运行环境要求 |
| :--- | :--- |
| `cpu` | 默认模式，使用 CPU 推理，兼容所有 `x86_64` 环境。 |
| `gpu` | 原生 Linux 下的 Intel 核显（需 `/dev/dri` 直通；NAS 若遇权限不足可在 `.env` 设置 `GALLERY_RENDER_GID` 与 `GALLERY_VIDEO_GID`；Windows Docker Desktop 不支持）。 |
| `cuda` | NVIDIA 显卡（Linux 需宿主机驱动与 NVIDIA Container Toolkit；Windows Docker Desktop 需开启 WSL2 后端与对应驱动；镜像已内置 CUDA 12.x / cuDNN 9.x 用户态库）。 |

> [!IMPORTANT]
> **Docker 数据安全与读写说明**
> - **切勿删除数据卷**：数据库、日志、备份、缓存与 AI 模型均保存在 `gallery-storage` 数据卷中（离线模型位于卷内 `models/character/ccip-caformer_b36-24/model_feat.onnx`）。升级或切换模式时**切勿执行 `docker compose down -v`**。脚本用户请同时保留项目下的 `.gallery-launcher` 目录以固定 `/mediaN` 槽位映射。
> - **媒体默认只读**：为保护宿主文件，Compose 默认以 `:ro` 只读挂载媒体目录（脚本默认同样只读）。需要移入回收站或归档整理时，请去掉 Compose 挂载行尾的 `:ro` 重新部署，或使用 `--writable` 启动脚本。
> - **压缩包工具**：Docker 容器内如需使用压缩包预览与解压，请确保容器环境已安装 7-Zip（`7zz` 或 `p7zip-full`），或通过 `GALLERY_7Z_PATH` 指定可执行文件路径。

---

## 配置参数

日常使用无需额外配置。Docker 环境可通过 `.env` 或 Compose 环境变量调整；fnOS 原生 FPK 无通用环境变量编辑界面（`.env` 不适用于 FPK，`TRIM_*` 由系统注入，其余默认参数定义在 `fnpack/cmd/main`）。

| 环境变量 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `CHARACTER_RECOGNITION_ENABLED` | `1` | 是否启用 AI 角色识别（`1` 开启 / `0` 关闭）。 |
| `CHARACTER_RECOGNITION_PROVIDER` | `auto` | 推理后端（`auto` 按 `CUDA → OpenVINO → CPU` 自动选择；Docker 启动器会按模式设为 `cpu`、`openvino` 或 `cuda`）。 |
| `CHARACTER_ALLOW_CPU_FALLBACK` | `1` | GPU 初始化失败时是否自动回退至 CPU（`0` 关闭）。 |
| `ONNXRUNTIME_AUTO_DOWNLOAD` | `1` | 启动后是否在后台自动下载缺失的 CCIP 模型与 CUDA 运行时（`0` 关闭，离线部署用）。 |
| `CHARACTER_MODEL_IDLE_TIMEOUT_SECONDS` | `600` | AI 模型空闲自动卸载秒数（`0` 常驻内存）。 |
| `SCAN_INTERVAL` | `21600` | 后台全库扫描周期（秒，默认 6 小时；`0` 关闭）。 |
| `HASH_INTERVAL` | `30` | 后台内容哈希计算轮询间隔（秒）。 |
| `DB_BACKUP_INTERVAL` | `43200` | SQLite 在线备份周期（秒，默认 12 小时）。 |
| `DB_BACKUP_RETENTION` | `8` | 在线备份快照最大保留份数。 |

---

## 源码构建

在 Windows 环境下交叉编译 Linux `x86_64` 二进制并打包 fnOS FPK，需启用 **WSL 2** 并安装 **Podman**，同时将官方打包工具 [`fnpack-1.2.3-windows-amd64.exe`](https://static2.fnnas.com/fnpack/fnpack-1.2.3-windows-amd64) 放入 `output/fnpack/`：

```powershell
# 一键交叉编译 Rust 运行时并生成校验 FPK 安装包
python tools/build_release.py --fnpack .\output\fnpack\fnpack-1.2.3-windows-amd64.exe

# 仅编译 Linux Rust 二进制（输出至 app/bin/gallery-accel）
python tools/build_rust_accel.py
```

更多接口文档、运维排障与打包细节请参阅 [`docs/API.md`](docs/API.md)、[`docs/OPERATIONS.md`](docs/OPERATIONS.md) 与 [`docs/FNOS_NATIVE.md`](docs/FNOS_NATIVE.md)。

---

## 项目结构

```text
gallery/
├── rust/gallery_accel/   # 纯 Rust 后端：Axum HTTP API、SQLite 连接池、扫描/哈希工作流、CCIP 推理与订阅引擎
├── app/static/           # 响应式 Web 前端：HTML5 单页外壳、styles/ 四层样式体系与原生 ES 模块脚本
├── fnpack/               # fnOS 原生应用定义（package.json）、启停生命周期脚本（cmd/main）与权限配置
├── Dockerfile            # 纯 Rust Docker 镜像构建定义（Dockerfile.cuda 为 CUDA 变体）
├── docker-compose.yml    # Docker Compose 编排文件（配套 .gpu.yml / .cuda.yml / gallery.yml）
├── docs/                 # API 契约、fnOS 原生打包规范、线上运维手册与版本发布说明
└── tools/                # 自动化脚本：Rust 交叉编译、FPK 打包、启动器与公开源码树生成工具
```

---

## 开源许可证

本项目基于 [GNU General Public License v3.0 only](LICENSE)（`GPL-3.0-only`）开源发布，Copyright (C) 2026 h-void。重新分发本项目或其修改版本时，必须保持 `GPL-3.0-only` 授权并提供完整对应源码。

随安装包与镜像分发的 ONNX Runtime、OpenVINO、7-Zip (`7zz`, © Igor Pavlov, LGPL-2.1+) 及 Rust 第三方依赖许可证声明均位于 `fnpack/app/licenses/`。
