# Gallery HTTP API 调用规范与工作流指南（AI Agent 专用）

> **用途说明**：本文档专为**用户侧的多模态 / 编码 AI Agent**（如 Claude Code、Codex、Cursor、Antigravity 等）编写。用户可将本文档直接作为上下文或 Skill 提供给自己的 Agent，使 Agent 能够通过 HTTP REST API 安全、自主地完成画师作品检索、压缩包解压、BLAKE3 去重、多模态看图角色识别与聚类打标、目录归档以及网盘任务派发。

---

## 1. Agent 核心操作铁律（Operational Guardrails）

Agent 在调用 Gallery API 时**必须**严格遵守以下规则：

1. **API 优先与主键不变量**：
   - 所有状态查询、解压、打标、删除与归档优先通过 HTTP API（默认 `http://<NAS_IP>:8899`，或读取环境变量 `GALLERY_API_URL`）完成。
   - `items.id` 是作品的**永久稳定主键**；文件重命名或目录移动仅改变 `items.file_path`，已绑定的角色标签与收藏状态始终跟随 `items.id`。
   - 获取作品预览图时，始终优先使用 `GET /api/file/preview?item_id={item_id}&max_edge=800`，避免手动拼接物理文件路径。
2. **并发写锁与 `HTTP 409 Conflict` 自动等待**：
   - 扫描（`/api/scan*`）、解压（`/api/archives/extract`）、删除（`/api/file/delete`）与目录归档执行（`/api/folder-renames/execute*`）共享服务端互斥写锁。
   - 当任何请求返回 `HTTP 409 Conflict` 时，**禁止直接报错退出**；必须轮询 `GET /api/scan/state`（间隔 `2s`），直到返回的 `state`（或 `status`）变为 `"idle"` 后再重试原请求。
   - 若服务刚启动返回 `HTTP 503 Service Unavailable`，轮询 `GET /api/health` 直至 `initializing == false`。
3. **危险与批量写操作审批门（`[STOP & ASK]`）**：
   - 在执行以下会修改用户媒体库或元数据的批量操作前，Agent **必须先输出预览摘要表并等待用户在当前回合明确批准**：
     - `DELETE /api/file/delete` 或 `POST /api/folders/delete`（移入回收站）
     - `PUT /api/items/tags`、`PUT /api/items/tags-by-name`、`PUT /api/folders/tags*`（批量修改角色标签）
     - `DELETE /api/tags/{tag_id}`（删除标签）
     - `DELETE /api/characters/{character_id}`（删除角色）
     - `POST /api/characters/clusters/commit`（提交角色聚类裁决）
     - `POST /api/folder-renames/execute` / `/execute-all`（执行物理目录重命名归档）
4. **安全删除原则（严禁绕过回收站）**：
   - 删除画廊内的重复或冗余文件时，必须调用 `DELETE /api/file/delete?path={file_path}` 移入回收站（自动保留标签与收藏快照，可随时通过 `/api/recycle/{entry_id}/restore` 原路还原）。
   - Agent 下载到本地临时目录（如 `.tmp/`）的 JPEG 预览缩略图，在看图核验完毕后可直接清理。
5. **压缩包冷存储保护策略**：
   - 批量解压画师目录下的压缩包时，仅解压 `.zip`、`.rar` 以及**有密码保护（`encrypted: true`）的 `.7z`**。
   - **无密码的 `.7z` 压缩包**通常用于 PSD/CLIP 工程源文件或游戏的冷存储归档，除非用户明确指定解压某一个 `.7z`，否则批量整理时应跳过无密码 `.7z`。
6. **查询参数布尔值格式**：
   - 所有 URL 查询参数中的布尔值（`duplicates_only`、`untagged`、`inbox`、`undated`、`favorite_only`、`search_tags_only`）均支持 `true`/`false`、`1`/`0`、`yes`/`no`。

---

## 2. 视觉核验与角色打标准则（Multimodal Visual Ground Truth）

当 Agent 具备多模态看图能力并执行角色打标或聚类复核时，必须遵循以下准则：

1. **像素级视觉真值（禁止仅凭标题盲打标）**：
   - 文件夹名、帖子标题或文件名仅作为文本线索（`text_hints`），**绝不能不看图直接根据文件名打标**。
   - 以下情况必须下载预览图（`GET /api/file/preview?item_id={id}&max_edge=800`）进行人工/多模态视觉核验：
     - 确认或修改任何角色标签前；
     - 单图 CCIP 最高候选置信度 `< 0.7174`，或前两名候选分差接近（`< 0.05`）；
     - 文件名/目录名不含明确角色名；
     - 作品为漫画、多图差分集或多人同框场景。
2. **单图物理出场判定标准（Per-Image Physical Presence）**：
   - 仅当某个角色的**物理身体**（含正侧面、可辨识背影、剪影，或经同系列上下文确认的标志性肢体/尾巴/耳朵）实际出现在该张画面中时，才为该图打上该角色标签。
   - 若单张画面中同时出现 $N$ 名可识别角色，则同时打上这 $N$ 名角色的标签。
   - 仅在台词气泡中提及名字、或画面中仅有脱离身体的孤立道具/玩偶而角色本人未出场时，**不打**该角色标签。
3. **角色标准参考图（Reference Art）入库标准**：
   - 调用 `POST /api/characters/{character_id}/references/upload` 上传角色参考图时，图片必须为**清晰的单人竖版立绘或全身/半身设定图**（建议分辨率 `width >= 350`、`height >= 500` 且 `width <= height * 1.05`），严禁将多人同框图、纯头像截屏或 Q 版表情包上传为标准参考图。

---

## 3. Agent 标准作业工作流（Step-by-Step Workflows）

### 工作流 A：画师盘点、密码提取与压缩包解压

1. **查找目标画师**：
   - 调用 `GET /api/artists` 获取画师列表，匹配目标画师的 `id`、`name` 与根目录 `path`。
   - 调用 `GET /api/artists/{artist_id}/stats` 与 `GET /api/folders?artist_id={artist_id}` 查看该画师的作品总数、目录树结构与归档状态。
2. **检索压缩包与同伴文本**：
   - 调用 `GET /api/items?artist_id={artist_id}&media_type=archive&limit=500` 列出该画师下所有压缩包。
   - 若需查找解压密码，可调用：
     - `GET /api/artists/{artist_id}/links`（查看服务端已从 `.txt`/`.html` 自动提取出的链接与提取码/密码）；
     - 或检索文本项 `GET /api/items?artist_id={artist_id}&media_type=text&limit=200`，再调用 `GET /api/file/text?path={file_path}` 读取文本内容。
3. **预检压缩包（自动试密）**：
   ```http
   POST /api/archives/inspect
   Content-Type: application/json

   {
     "item_id": 2048,
     "password": "可选密码"
   }
   ```
   - 服务端会自动尝试传入的 `password`、同目录 `.txt` 文本中的候选密码、网盘提取码以及该画师的历史成功密码。
   - 检查返回的 `encrypted`、`needs_password` 与 `entries`。若为无密码 `.7z`（`file_path` 以 `.7z` 结尾且 `encrypted == false`），按冷存储策略默认跳过。
4. **原子解压并回收源压缩包**：
   ```http
   POST /api/archives/extract
   Content-Type: application/json

   {
     "item_id": 2048,
     "password": "已验证密码",
     "target_mode": "current_folder",
     "recycle_source": true
   }
   ```
   - `target_mode`：`"current_folder"`（解压至当前目录）或 `"new_folder"`（解压至同名新建子目录）。
   - `recycle_source: true`：解压成功后自动将源压缩包移入回收站。
5. **触发增量扫描与哈希计算**：
   - 解压完成后调用 `POST /api/scan/folder`（传入 `{"path": "<画师根目录或子目录>"}`），待 `GET /api/scan/state` 回到 `"idle"` 后调用 `POST /api/hash/run` 计算新文件 BLAKE3 哈希。

---

### 工作流 B：基于 BLAKE3 内容哈希的精准去重

1. **确保哈希计算完毕**：
   - 调用 `GET /api/hash/status`，确认 `items.remaining == 0` 且 `scan_candidates.remaining == 0`（若大于 `0`，调用 `POST /api/hash/run` 并等待完成）。
2. **分页拉取重复作品**：
   - 循环调用 `GET /api/items?artist_id={artist_id}&duplicates_only=true&limit=500&offset={offset}` 直至拉取完该画师全部重复项。
3. **按 `content_hash` 分组并挑选保留项（Canonical Selection）**：
   - 将返回的 `items` 按 `content_hash` 分组（仅处理组内数量 `>= 2` 的组）。
   - 在每个哈希组中挑选 **1 个保留项（Keep）**，其余作为 **待回收项（Delete）**，优先保留规则如下：
     1. 优先保留路径/目录名**不含** `(1)`、`(2)`、`(3)`、`副本`、`copy` 等重复后缀的文件；
     2. 优先保留已整理归档目录或 `display_date` / `file_mtime` 最早的原始文件；
     3. 默认跳过同一文件夹内部的重复文件（除非用户明确要求清理同目录内重复）。
4. **汇报审批并移入回收站（`[STOP & ASK]`）**：
   - 向用户展示去重预览表（保留路径 vs 待删路径、哈希值、释放空间）。
   - 获得用户批准后，对每个待删项调用：
     ```http
     DELETE /api/file/delete?path=<url_encoded_file_path>
     ```

---

### 工作流 C：多模态角色聚类发现与批量裁决（推荐用于未整理画师）

针对存在大量未打标作品的画师，优先使用服务端完全连接聚类接口，每次只需看 1~2 张簇代表图即可完成整簇打标与参考图入库：

#### 步骤 1：发现待整理聚类簇
```http
POST /api/characters/clusters/discover
Content-Type: application/json

{
  "artist_id": 104,
  "min_similarity": 0.7174,
  "max_clusters": 30
}
```
**响应结构要点**：
- `clusters[]`：每个簇包含：
  - `cluster_id`（整数标识）
  - `item_ids`（该簇包含的所有作品 ID 列表）
  - `representative_samples[]`（1~2 张代表图：`item_id`、`role` 为 `"medoid"` 或 `"diverse"`、`preview_url` 如 `"/api/file/preview?item_id=1024&max_edge=768"`）
  - `text_hints[]`（服务端从该簇作品文件名与目录名中提取的文本词条）

#### 步骤 2：下载代表图并按多模态规则核验
Agent 下载每个簇的 `representative_samples[].preview_url` 并结合 `text_hints` 出具结构化裁决：

- **裁决枚举（`verdict`）**：
  - `"CONFIRMED_CANON"`：确认为已知官方版权角色；
  - `"ORIGINAL_CHARACTER"`：画师原创自设（OC）/ 看板娘 / 无法对应任何官方作品的原创人物；
  - `"UNKNOWN_CLUSTER"`：画面模糊、线索不足或无法确认身份（服务端将跳过写入，留待后续处理）。
- **防幻觉与服务端防投毒门（Server Anti-Poisoning Gates）**：
  - **严禁盲猜套用**：不认识的冷门角色必须归为 `"UNKNOWN_CLUSTER"`，绝不可根据发色/瞳色强行套用热门角色名。
  - **文本线索门（`matched_text_hint`）**：若判定为 `"CONFIRMED_CANON"`，必须检查 `text_hints` 数组中是否有对应原词。若有，将 `matched_text_hint` 设为该**完全一致的原词**（此时特征向量以 `verified=1` 正式入库）；若 `text_hints` 中无匹配词，`matched_text_hint` 必须填 `null`（服务端仍会完成作品打标，但会将特征向量降级为 `verified=0` 候选状态，防止无文本印证的视觉误判污染检索底库）。
  - **多人画面门（`is_multi_character_shot`）**：若代表图中包含 `>= 2` 名独立角色，必须将 `is_multi_character_shot` 设为 `true`，并在 `characters` 数组中列出所有出场角色（服务端将只对作品打标，不将多人图写入单人参考向量库）。
  - **OC 门**：`verdict` 为 `"ORIGINAL_CHARACTER"` 时，服务端仅创建角色标签并打标，不将向量写入通用版权角色底库。

#### 步骤 3：向用户展示裁决预览并原子提交（`[STOP & ASK]`）
经用户确认后，调用 `POST /api/characters/clusters/commit` 一次性原子提交：
```http
POST /api/characters/clusters/commit
Content-Type: application/json

{
  "artist_id": 104,
  "decisions": [
    {
      "cluster_id": 1,
      "item_ids": [1024, 1025, 1026, 1039],
      "verdict": "CONFIRMED_CANON",
      "matched_text_hint": "雷电将军",
      "is_multi_character_shot": false,
      "characters": [
        { "name": "雷电将军", "series": "原神" }
      ],
      "enroll_prototypes": []
    }
  ]
}
```
> **说明**：`enroll_prototypes` 传空数组 `[]` 时，服务端默认将簇内的 `medoid` 与 `diverse` 代表项注册为角色原型特征；请勿传入非代表项的 `item_id`（否则会因 `invalid_enroll_prototype` 被拒绝）。

---

### 工作流 D：单作品 CCIP 识别、角色建档与按名批量打标

当处理零散作品或聚类未覆盖的单图时：

1. **对单件作品运行 CCIP 角色识别**：
   ```http
   POST /api/items/1024/character-recognition?top_k=5
   Content-Type: application/json

   { "top_k": 5 }
   ```
   - 默认高置信度命中阈值为 `0.7174`，待复核区间为 `0.5739 ~ 0.7174`。
2. **角色建档与上传标准立绘（当角色尚未入库时）**：
   - 搜索角色是否已存在：`GET /api/characters?search=角色名`
   - 若不存在则新建角色：
     ```http
     POST /api/characters
     Content-Type: application/json

     { "name": "雷电将军" }
     ```
   - 上传合格的单人竖版参考立绘（二进制原始图片字节）：
     ```http
     POST /api/characters/{character_id}/references/upload
     Content-Type: application/octet-stream

     <raw image binary bytes>
     ```
3. **按角色名称批量打标（`[STOP & ASK]`）**：
   推荐使用 `PUT /api/items/tags-by-name`（无需预先查询 `tag_id`，不存在的标签会自动创建，且自动同步至同画师同 BLAKE3 哈希的所有副本）：
   ```http
   PUT /api/items/tags-by-name
   Content-Type: application/json

   {
     "item_ids": [1024, 1025],
     "tag_names": ["雷电将军", "八重神子"],
     "mode": "add"
   }
   ```
   - `mode` 说明：
     - `"add"`：在同画师同哈希组上追加标签并集；
     - `"set"`：用 `tag_names` 覆盖同画师同哈希组的全部原有标签；
     - `"remove"`：从同画师同哈希组移除指定标签。
4. **清理冗余或错别字标签（`[STOP & ASK]`）**：
   - 先通过 `PUT /api/items/tags-by-name` 将旧标签作品迁移至标准角色名，再调用 `DELETE /api/tags/{tag_id}?artist_id={artist_id}` 删除冗余标签。

---

### 工作流 E：日期补全、文件夹原位标注与规则化目录归档

1. **批量修改作品日期**：
   ```http
   PUT /api/items/date
   Content-Type: application/json

   {
     "artist_id": 104,
     "item_ids": [1024, 1025],
     "manual_date": "2026-05-15"
   }
   ```
   *(支持 `YYYY-MM-DD` 或 `YYYY-MM`，传 `null` 清空手动日期。)*
2. **对整个子文件夹就地标注角色与日期**：
   ```http
   POST /api/folders/annotate
   Content-Type: application/json

   {
     "artist_id": 104,
     "folder": "2026-05-15 untitled",
     "tag_names": ["雷电将军"],
     "mode": "add",
     "manual_date": "2026-05-15"
   }
   ```
3. **生成并执行目录重命名归档计划（默认模板 `{year}/{date} {tags}`）**：
   - **第一步（生成计划）**：`POST /api/folder-renames/refresh`，请求体 `{"artist_id": 104}`。
   - **第二步（预览计划）**：`GET /api/folder-renames?artist_id=104`，向用户展示源目录与目标目录对照表（`[STOP & ASK]`）。
   - **第三步（确认并执行）**：
     ```http
     POST /api/folder-renames/confirm-all
     Content-Type: application/json

     { "artist_id": 104 }
     ```
     ```http
     POST /api/folder-renames/execute
     Content-Type: application/json

     { "artist_id": 104 }
     ```
     *(执行前服务端会自动创建 SQLite 在线备份并重校验路径安全性。)*

---

### 工作流 F：创作者订阅（Pawchive）与 JDownloader 网盘任务派发

1. **检查 JDownloader 桥接状态**：
   - 调用 `GET /api/netdisk/connection`，确认返回 `connected: true` 且 `handshaked: true`。
2. **检索待派发的网盘链接**：
   - **方式 1（来自创作者订阅帖子）**：
     - 调用 `GET /api/pawchive/subscriptions` 查找画师对应的 `subscription_id`；
     - 调用 `GET /api/pawchive/posts?subscription_id={id}&state=external&limit=200`（使用 `next_cursor` 翻页）获取所有包含外部网盘链接的帖子 `id`。
   - **方式 2（来自画师本地 `.txt` 文档）**：
     - 调用 `GET /api/artists/{artist_id}/links` 或 `GET /api/file/text?path=...` 提取网盘 URL 与提取码/解压密码。
3. **派发下载任务至 JDownloader**：
   - **按订阅帖子 ID 批量派发**：
     ```http
     POST /api/netdisk/jobs
     Content-Type: application/json

     {
       "post_ids": [501, 502],
       "force": false
     }
     ```
   - **直接派发外部网盘链接与密码**：
     ```http
     POST /api/netdisk/jobs
     Content-Type: application/json

     {
       "link": "https://mega.nz/file/xxxx#yyyy",
       "password": "optional_passcode",
       "force": false
     }
     ```
4. **查询任务状态与入库**：
   - 调用 `GET /api/netdisk/jobs` 查看任务状态（`registered`、`submitted`、`settled`、`confirmed`）。
   - 对已下载完成（`settled`）的任务，可调用 `POST /api/netdisk/jobs/{task_id}/import` 自动解压并导入画师作品目录。

---

## 4. Python 零依赖客户端模板（内置 `409` 写锁等待）

Agent 可直接复用以下 Python 标准库函数调用任意接口：

```python
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request

BASE_URL = os.environ.get("GALLERY_API_URL", "http://127.0.0.1:8899").rstrip("/")


def gallery_api(method: str, path: str, data=None, params=None, timeout: int = 60, max_retries: int = 3):
    url = f"{BASE_URL}{path}"
    if params:
        qs = urllib.parse.urlencode({k: v for k, v in params.items() if v is not None})
        if qs:
            url = f"{url}?{qs}"

    body = json.dumps(data).encode("utf-8") if data is not None else None

    for attempt in range(1, max_retries + 1):
        req = urllib.request.Request(url, data=body, method=method)
        req.add_header("Accept", "application/json")
        if body is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                raw = resp.read().decode("utf-8")
                return json.loads(raw) if raw else None
        except urllib.error.HTTPError as err:
            # 写锁冲突（409）时自动轮询 /api/scan/state 直至 idle 后重试
            if err.code == 409 and attempt < max_retries:
                while True:
                    time.sleep(2.0)
                    state = gallery_api("GET", "/api/scan/state", max_retries=1) or {}
                    if state.get("state") in ("idle", None) and state.get("status") in ("idle", None):
                        break
                continue
            raise RuntimeError(f"HTTP {err.code} {path}: {err.read().decode('utf-8', 'replace')}")
```

---

## 5. 全量 API 路由速查表

### 5.1 健康检查、日志与系统维护
| 方法 | 路径 | 请求参数 / 载荷要点 |
| --- | --- | --- |
| `GET` | `/api/health` | 返回 `status`, `ok`, `degraded`, `initializing`, `hash_queue`, `backups` |
| `GET` | `/api/capabilities` | 返回 `read_only`, `write`, `media`, `ml` 布尔能力标识 |
| `GET` | `/api/logs/tail` | 查询参数：`source=ui\|gallery\|startup`, `lines=60`（上限 2000 行 / 256 KiB） |
| `POST` | `/api/backup` | 立即触发一次 SQLite 在线备份 |
| `GET` | `/api/operation-log`, `/api/operation-log/history` | 查询近期操作日志、历史记录与错误摘要 |

### 5.2 画师、作品、标签与目录
| 方法 | 路径 | 请求参数 / 载荷要点 |
| --- | --- | --- |
| `GET` | `/api/items` | 查询参数：`artist_id`, `limit`, `offset`, `cursor`, `sort`, `media_type`, `folder`, `tags`, `tag_id`, `untagged`, `undated`, `inbox`, `duplicates_only`, `favorite_only`, `date_from`, `date_to`, `search`, `search_tags_only` |
| `GET` | `/api/items/{item_id}` | 返回单件作品完整元数据与 `tags[]` |
| `PUT` | `/api/items/{item_id}/favorite` | JSON：`{"favorite": true\|false}` |
| `PUT` | `/api/items/tags-by-name` | JSON：`{"item_ids": [int], "tag_names": [str], "mode": "add"\|"set"\|"remove"}` |
| `PUT` | `/api/items/tags` | JSON：`{"artist_id": int, "item_ids": [int], "tag_ids": [int], "mode": "add"\|"set"\|"remove"}` |
| `PUT` | `/api/items/date` | JSON：`{"artist_id": int, "item_ids": [int], "manual_date": "YYYY-MM-DD"\|"YYYY-MM"\|null}` |
| `POST` | `/api/items/bundle` | JSON：`{"artist_id": int, "item_ids": [int], "target_folder": str}` |
| `POST` / `GET` | `/api/items/dimensions/backfill`, `/status` | 触发或查询缺失媒体宽高尺寸的后台补全任务 |
| `GET` / `POST` | `/api/tags`, `/api/tags/search` | 查询参数：`artist_id`, `search`, `limit`；新建传 `{"artist_id": int, "name": str}` |
| `PUT` / `DELETE` | `/api/tags/{tag_id}` | 删除传查询参数 `?artist_id={id}`；重命名传 `{"artist_id": int, "name": str}` |
| `POST` | `/api/tags/propagate-hash` | 将已有标签同步至同画师下相同 BLAKE3 哈希的副本 |
| `GET` | `/api/artists`, `/{artist_id}`, `/{artist_id}/stats`, `/{artist_id}/folder-paths`, `/api/artists/duplicates` | 画师列表、详情、统计覆盖率、目录路径列表与同名重复画师检测 |
| `GET` / `POST` | `/api/artists/{artist_id}/links`, `/links/reindex` | 获取或重新解析画师文本中的网盘链接与提取码 |
| `GET` / `POST` / `DELETE` | `/api/artists/{artist_id}/profile-links`, `/profile-links/{link_id}` | 管理画师社交主页与赞助订阅链接 |
| `GET` | `/api/folders` | 查询参数：`artist_id={id}`，返回层级目录树与归档状态 |
| `PUT` | `/api/folders/tags`, `/api/folders/tags-by-name` | JSON：`{"artist_id": int, "folder": str, "tag_names": [str], "mode": "add"\|"set"\|"remove"}` |
| `POST` | `/api/folders/annotate` | JSON：`{"artist_id": int, "folder": str, "tag_names"?: [str], "mode"?: str, "manual_date"?: str}` |
| `POST` / `DELETE` | `/api/folders/delete` | JSON：`{"artist_id": int, "folder": str}`，将整个文件夹内的媒体安全移入回收站 |

### 5.3 扫描、哈希、移动追踪与目录归档
| 方法 | 路径 | 请求参数 / 载荷要点 |
| --- | --- | --- |
| `POST` | `/api/scan`, `/api/scan/folder`, `/api/scan/stop` | 增量扫描传 `{"path": str}` |
| `GET` | `/api/scan/state`, `/ws/scan` | 返回 `"idle"` 或 `"scanning"` 及进度详情 |
| `GET` / `POST` | `/api/hash/status`, `/api/hash/run`, `/api/content-hash` | 查询哈希队列深度、触发后台哈希批次或计算单文件 BLAKE3 |
| `GET` / `POST` | `/api/move-candidates`, `/auto-resolve`, `/{id}/confirm`, `/{id}/ignore`, `/{id}/new` | 查询或处理库外文件移动/重命名候选记录 |
| `GET` / `POST` | `/api/folder-renames`, `/api/folder-renames/refresh` | 查询参数 `?artist_id={id}` 或 JSON `{"artist_id": int}` |
| `GET` / `PUT` / `POST` | `/api/folder-renames/settings`, `/preview`, `/apply-template` | 读取/修改命名模板（默认 `{year}/{date} {tags}`）、预览或执行迁移 |
| `POST` | `/api/folder-renames/confirm-all`, `/unconfirm-all`, `/execute`, `/execute-all` | JSON：`{"artist_id": int}`，确认或执行目录归档计划 |

### 5.4 媒体流、预览图、压缩包与回收站
| 方法 | 路径 | 请求参数 / 载荷要点 |
| --- | --- | --- |
| `GET` | `/api/file/preview` | 查询参数：`item_id={id}&max_edge=800`（或 `path={path}&max=800`） |
| `GET` | `/api/file/text` | 查询参数：`path={file_path}`（上限 `512 KiB`，返回 `{"content": str}`） |
| `GET` | `/api/file`, `/api/file/stream` | 原始媒体文件流（支持 `ETag`、`304` 与 `206` 分段请求） |
| `POST` | `/api/archives/inspect` | JSON：`{"item_id": int, "file_path"?: str, "password"?: str}` |
| `POST` | `/api/archives/extract` | JSON：`{"item_id": int, "password"?: str, "target_mode": "current_folder"\|"new_folder", "recycle_source": bool}` |
| `GET` | `/api/archives/entry` | 预览或下载压缩包内单个条目 |
| `DELETE` | `/api/file/delete` | 查询参数：`?path={file_path}`，两阶段安全移入回收站 |
| `GET` / `POST` | `/api/recycle`, `/api/recycle/{entry_id}/restore`, `/api/recycle/clear` | 查看回收站列表、原路还原文件及元数据快照或清空回收站 |

### 5.5 AI 角色识别与聚类整理
| 方法 | 路径 | 请求参数 / 载荷要点 |
| --- | --- | --- |
| `GET` / `POST` / `DELETE` | `/api/characters`, `/summary`, `/{character_id}` | 搜索：`?search={query}`；新建：`{"name": str}` |
| `POST` | `/api/characters/{character_id}/references/upload` | Header：`Content-Type: application/octet-stream`，Body 为图片原始字节 |
| `GET` / `POST` / `DELETE` | `/api/characters/{character_id}/references`, `/{ref_id}`, `/{ref_id}/verify`, `/verify-all`, `/discover-candidates` | 查看、删除、挖掘候选参考图（`verified=0`）或审核转正（`verified=1`） |
| `POST` / `GET` | `/api/characters/import-from-tags/jobs`, `/jobs/current`, `/jobs/{job_id}/cancel` | 后台从已有单角色标签批量构建特征库 |
| `POST` | `/api/items/{item_id}/character-recognition` | 查询参数 `?top_k=5` 及 JSON `{"top_k": 5}` |
| `POST` | `/api/characters/clusters/discover` | JSON：`{"artist_id": int, "min_similarity"?: 0.7174, "max_clusters"?: int}` |
| `POST` | `/api/characters/clusters/commit` | JSON：`{"artist_id": int, "decisions": [...]}` |
| `GET` / `PUT` / `POST` | `/api/ml-runtime/status`, `/api/ml-runtime/settings`, `/api/ml-runtime/retry` | 查询推理后端与模型下载进度、切换镜像源（`official`\|`china`）或重试 |

### 5.6 创作者订阅（Pawchive）与 JDownloader 网盘桥接
| 方法 | 路径 | 请求参数 / 载荷要点 |
| --- | --- | --- |
| `GET` / `POST` / `DELETE` | `/api/pawchive/subscriptions`, `/{id}`, `/{id}/toggle`, `/{id}/mode`, `/{id}/summary`, `/{id}/posts` | 创作者订阅增删改查、启停切换、覆盖率摘要与帖子列表 |
| `GET` | `/api/pawchive/posts` | 查询参数：`subscription_id`, `state`, `day`, `artist_id`, `search`, `cursor`, `limit`（≤200） |
| `POST` | `/api/pawchive/check`, `/reconcile`, `/sync` | 触发发现检查、本地目录核对或完整同步下载轮次 |
| `GET` | `/api/netdisk/connection` | 返回 `connected`, `handshaked`, `state`, `session_id` |
| `GET` / `POST` | `/api/netdisk/jobs` | 投递传 `{"post_ids": [int], "force": bool}` 或 `{"link": str, "password"?: str, "force": bool}` |
| `POST` | `/api/netdisk/jobs/{task_id}/{action}` | `action` 支持 `import`（入库）、`retry`（重试）、`cancel`（取消） |
