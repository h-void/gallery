<div align="center">

# Gallery

**基于纯 Rust 的私有化本地媒体库与画师作品管理系统**

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Platform: fnOS (原生 FPK)](https://img.shields.io/badge/Platform-fnOS%20FPK-brightgreen.svg)](#部署指南)
[![Backend: Pure Rust](https://img.shields.io/badge/Backend-Pure%20Rust%20%28Axum%29-orange.svg)](#项目结构)
[![Frontend: Web SPA](https://img.shields.io/badge/Frontend-Responsive%20SPA-blueviolet.svg)](#核心特性)

[简体中文](README.md) • [English](README.en.md)

[核心特性](#核心特性) • [部署指南](#部署指南) • [配置参数](#配置参数) • [AI Agent 接入](#ai-agent-与-http-api) • [源码构建](#源码构建) • [开源协议](#开源许可证)

</div>

---

## 项目简介

**Gallery** 是面向插画收藏与创作者作品集管理的私有化本地媒体库。后端采用纯 Rust 编写（Axum + rusqlite + Tokio），无 Python 运行时依赖，前端为无框架原生响应式单页应用（SPA）。系统以**「目录即画师」**组织媒体资产，集成多维组合筛选、CCIP 角色识别与聚类发现、BLAKE3 内容查重、规则化目录归档、创作者订阅同步以及 JDownloader 网盘桥接。

---

## 核心特性

- **多格式浏览与 Studio Folio 视觉体验**
  - **全格式支持**：原生支持 JPG/PNG/WebP/AVIF/BMP 图片、GIF 悬停动图预览（未记录尺寸的动图加载时自动探测宽高比）、MP4/WebM/MOV/MKV 视频流播（`ffmpeg` 转码 / HLS），同时索引 `.txt`/`.md`/`.html` 文本及 PSD/CLIP/PSB 等创作源文件。
  - **压缩包免解压快览与智能解压**：点击 ZIP/RAR/7Z 卡片即可浏览包内目录树、文件大小并预览图片（单条目内存受控，支持方向键翻页与序号指示）。支持多选或按画师一键批量解压（默认跳过无密码 `.7z` 冷存储归档，仅解压 `.zip`、`.rar` 与加密 `.7z`），自动尝试同伴 `.txt`、网盘提取码与画师历史密码并持久化记忆，解压成功后原包自动移入回收站。
  - **高效交互与拖拽整理**：提供 `4:3 完整`、`3:4 竖版` 网格与高密度单行列表，搭配宣纸浅色与「墨色」炭黑深色主题及中英文无刷新切换。全屏灯箱支持自由缩放平移、规格参数查看、点击标签筛选与一键「定位文件夹」；卡片可直接拖拽至左侧目录树移动或拖至角色行打标；全站支持 `Shift + 点击` 连续多选，筛选、排序与滚动位置实时同步至地址栏。
- **画师分区与多维组合检索**
  - **单画师与全库跨画师浏览**：媒体根目录下的第一层子文件夹自动映射为画师，既可按画师独立浏览，也可切入「全库作品」跨画师检索；支持子目录树（含折叠与拼音过滤）、角色标签与媒体类型叠加筛选、日期区间与「无日期」筛选，以及「全部 / 待整理 / 已归档」状态视图。
  - **批量标注与拼音搜索**：支持单选、框选及按文件夹批量加减标签（标签区分大小写，变更自动同步同哈希副本），侧栏置顶「未加角色」待办入口；画师、目录与标签均支持拼音匹配与「仅搜角色」模式。
  - **链接自动索引**：自动提取画师目录内文本中的网页链接、网盘地址与提取码，并集中管理 Pixiv、Fanbox、Patreon、Twitter/X、Bilibili 等画师主页。
- **AI 智能角色识别（CCIP）与聚类发现**
  - **自动硬件加速**：默认启用 CCIP 角色识别（支持图片与视频抽帧识别，自动跳过片头纯黑/纯白淡入帧），启动后后台自动准备模型与运行时，按 `CUDA → OpenVINO → CPU` 选择最佳推理后端（GPU 不可用时自动回退 CPU）。
  - **特征库构建、候选挖掘与角色聚类**：从单角色作品提取参考特征（自动剔除 ≥0.95 近重复与离群图），支持后台批量导入角色、手动上传参考图、一键「挖掘候选参考图」并按「仅看待审核」逐张或批量转正；支持对未标注作品按 64 位差分哈希折重与特征聚类发现潜在角色簇，AI 建议与聚类结果均在确认后落库。
- **内容哈希查重与移动追踪**
  - **BLAKE3 精准查重**：提供重复文件夹与重复文件分组视图，后台哈希计算完成后静默刷新列表。
  - **库外移动自动重连**：文件在库外重命名或移动后，结合 BLAKE3 哈希与 inode 自动恢复关联；同画师移动自动保留标签，跨画师冲突转入维护页人工复核。
- **规范化文件夹整理与双模式回收站**
  - **规则引擎与就地消解冲突**：内置可编辑 `Default` 命名模板（默认 `{year}/{date} {tags}`，缺失日期自动从文件名或 Twitter/X 雪花 ID 提取）。支持侧栏就地补齐角色与日期、编辑栏一键「整理归档」当前目录、多选文件「打包文件夹」、维护页就地化解同目录多日期冲突（一键统一日期或切换按角色拆分归档），以及命名规则变更前的全库迁移预览。
  - **事务与备份保护**：整理执行前自动创建 SQLite 在线备份；目标已存在时保留原文件，中途失败自动回滚；支持开启全库扫描后「自动整理」。
  - **双模式回收站**：可在维护页切换「飞牛系统回收站」或「应用内部回收站」（附对比卡片），支持文件与整目录移入回收站、图片/视频/压缩包封面缩略图预览、网格/列表视图切换、一键批量还原（含标签与收藏快照）及彻底删除。
- **创作者订阅与网盘批量派发**
  - **订阅同步与覆盖率日历**：按画师订阅创作者主页，支持自动或手动模式按轮次发现、核对本地目录并断点续传新作品；订阅卡片内置全宽 53 周覆盖率日历，按日查看缺口与作品详情。
  - **全部作品与 JDownloader 桥接**：提供跨画师「全部作品」筛选、状态核对与手动导入；帖内网盘链接或外部直投链接可通过 Event Scripter 本地桥接批量派发至 JDownloader，下载完成后自动解压入库。

> [!WARNING]
> **网络安全**：Gallery 默认监听 `8899`，不含内置登录认证，适用于家庭或可信局域网。公网访问请通过 fnOS 反向代理或前置 Nginx / Caddy 配置身份认证。

---

## 部署指南

### 方案一：飞牛 fnOS 原生应用安装（推荐）

Gallery 提供原生 fnOS 安装包（FPK），单一 Rust 二进制直接运行于宿主机，启动即监听端口并在后台完成初始化，内置 `jemalloc` 内存回收与 `7zz` 解压引擎。

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
> **离线环境模型放置**：CCIP 模型（`deepghs/ccip_onnx`）默认在启动后后台自动下载。若 NAS 处于纯离线环境，可将模型手动放置于 `gallery/models/character/ccip-caformer_b36-24/model_feat.onnx`。

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
> - **保留数据卷**：数据库、日志、备份、缓存与 AI 模型均保存在 `gallery-storage` 数据卷中（离线模型位于卷内 `models/character/ccip-caformer_b36-24/model_feat.onnx`）。升级或切换模式时不要执行 `docker compose down -v`。脚本用户请同时保留项目下的 `.gallery-launcher` 目录以固定 `/mediaN` 槽位映射。
> - **媒体默认只读**：Compose 默认以 `:ro` 挂载媒体目录，脚本默认同样只读。需要移入回收站或归档整理时，去掉挂载行尾的 `:ro` 重新部署，或使用 `--writable` 启动脚本。
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

## AI Agent 与 HTTP API

Gallery 原生提供完整的 REST API，并随仓库附带专为多模态与编码 AI Agent（Claude Code、Codex、Cursor、Antigravity 等）编写的行动指南 [`docs/API_USAGE.md`](docs/API_USAGE.md)。将 [`docs/API_USAGE.md`](docs/API_USAGE.md) 作为上下文或 Skill 提供给你的 Agent，即可让其按内置安全护栏自动执行画师盘点、压缩包试密解压、BLAKE3 去重、多模态看图角色聚类与打标、目录归档及 JDownloader 网盘任务投递。

---

## 源码构建

在 Windows 环境下交叉编译 Linux `x86_64` 二进制并打包 fnOS FPK，需启用 **WSL 2** 并安装 **Podman**，同时将官方打包工具 [`fnpack-1.2.3-windows-amd64.exe`](https://static2.fnnas.com/fnpack/fnpack-1.2.3-windows-amd64) 放入 `output/fnpack/`：

```powershell
# 一键交叉编译 Rust 运行时并生成校验 FPK 安装包
python tools/build_release.py --fnpack .\output\fnpack\fnpack-1.2.3-windows-amd64.exe

# 仅编译 Linux Rust 二进制（输出至 app/bin/gallery-accel）
python tools/build_rust_accel.py
```

---

## 项目结构

```text
gallery/
├── rust/gallery_accel/   # 纯 Rust 后端：Axum HTTP API、SQLite 连接池、扫描/哈希工作流、CCIP 推理与订阅引擎
├── app/static/           # 响应式 Web 前端：HTML5 单页外壳、styles/ 四层样式体系与原生 ES 模块脚本
├── fnpack/               # fnOS 原生应用定义（package.json）、启停生命周期脚本（cmd/main）与权限配置
├── Dockerfile            # 纯 Rust Docker 镜像构建定义（Dockerfile.cuda 为 CUDA 变体）
├── docker-compose.yml    # Docker Compose 编排文件（配套 .gpu.yml / .cuda.yml / gallery.yml）
├── docs/API_USAGE.md     # AI Agent 专用 HTTP REST API 工作流指南与全量路由速查表
└── tools/                # 自动化脚本：Rust 交叉编译、FPK 打包、启动器与公开源码树生成工具
```

---

## 开源许可证

本项目基于 [GNU General Public License v3.0 only](LICENSE)（`GPL-3.0-only`）开源发布，Copyright (C) 2026 h-void。重新分发本项目或其修改版本时，必须保持 `GPL-3.0-only` 授权并提供完整对应源码。

随安装包与镜像分发的 ONNX Runtime、OpenVINO、7-Zip (`7zz`, © Igor Pavlov, LGPL-2.1+) 及 Rust 第三方依赖许可证声明均位于 `fnpack/app/licenses/`。
