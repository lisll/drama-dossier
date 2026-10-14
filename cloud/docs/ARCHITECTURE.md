# Architecture · 云端短剧视频制作与多平台适配系统

> 对应设计文档《云端短剧视频制作与多平台适配系统——场景说明、架构设计与技术实现方案》附录 A 交付物之一。
> 本文描述的是**本仓库里真实可运行的实现**，不是理想蓝图；与设计文档的差异集中列在 §7。

**边界声明**：本项目仅用于技术学习与处理**自有素材**，不用于绕过任何平台的识别机制或版权保护。

## 1. 逻辑架构

```text
┌──────────────────────────────────────────────────────────────────────────┐
│ 客户端层                                                                  │
│   Web 客户端（src/drama_cloud/web/index.html）                            │
│     · 令牌登录 · 直传素材 · 选配方建任务 · 轮询状态 · 取产物下载            │
│   CLI（drama-cloud ...）                                                  │
│     · initdb / seed / submit --wait / task / status / demo                │
└───────────────────────────────┬──────────────────────────────────────────┘
                                │ HTTPS + Bearer 令牌
┌───────────────────────────────▼──────────────────────────────────────────┐
│ 控制面 API（api.py，FastAPI）                                             │
│   认证与角色（viewer < operator < admin）· 主体级限流 · 幂等键             │
│   上传会话（短时签名令牌）· 配方发布/解析 · 任务创建/查询/取消/重试         │
│   —— 只做校验与状态迁移，不跑任何媒体处理                                   │
└───────────────────────────────┬──────────────────────────────────────────┘
                                │ 同一进程内调用 service 层
┌───────────────────────────────▼──────────────────────────────────────────┐
│ 任务与调度（tasks/）                                                      │
│   state.py    状态机（服务端强校验，客户端不能任意覆盖）                    │
│   queue.py    持久化队列 · 优先级 · 租约 · 心跳 · 指数退避 · 失联回收        │
│   service.py  配方版本策略 · 任务快照 · 幂等键 · 产物登记 · 保留期清理       │
│   runner.py   Worker：取任务 → 建工作目录 → 按计划跑模块 → 检查点 → 登记产物 │
└───────────┬───────────────────────────────────────┬──────────────────────┘
            │ 读取计划快照（tasks.plan_json）       │ 读写对象
┌───────────▼───────────────────────┐   ┌───────────▼──────────────────────┐
│ 处理管线（modules/）               │   │ 存储（storage.py）               │
│   13 个白名单模块，见 §4           │   │  本地文件系统实现                │
│   计划型模块只写 ctx.plan          │   │  · 对象键不透明，调用方不拼路径    │
│   只有 media.encode 物化文件       │   │  · 上传/下载用短时 HMAC 令牌      │
└───────────┬───────────────────────┘   └──────────────────────────────────┘
            │ 调用外部进程
┌───────────▼──────────────────────────────────────────────────────────────┐
│ 媒体引擎：ffmpeg 4.2.4 / ffprobe（本机实测版本）                           │
└──────────────────────────────────────────────────────────────────────────┘
```

## 2. 部署架构（本机实际形态）

单机四进程模型，全部在同一台 `openEuler 22.03 / aarch64 / 无 GPU` 机器上：

| 进程 | 启动方式 | 职责 |
| --- | --- | --- |
| API 服务 | `drama-cloud serve` | HTTP 控制面 + Web 客户端静态页 |
| Worker | `drama-cloud run-worker` | 抢任务、跑 ffmpeg、登记产物；可起多个，队列保证不重复 |
| 调度回收 | `drama-cloud cleanup`（可 cron） | 回收过期租约、清理过期产物 |
| CLI | `drama-cloud <cmd>` | 运维与演示；与 API 共用同一套 service 层 |

API 与 Worker **通过数据库解耦**：Worker 不反向调用 API，API 也不需要知道有多少个 Worker。
因此 Worker 可以单独重启、扩缩容，客户端升级不会影响媒体处理环境（对应设计文档 §8.2 的要求）。

`cloud/var/` 是运行目录，全部可删（删掉即回到出厂状态）：

```text
cloud/var/
├── drama_cloud.sqlite3    # 数据库（WAL 模式）
├── secret.key             # 签名密钥（0600，首次运行自动生成）
├── storage/               # 对象存储根：assets/ 与 artifacts/
├── work/                  # Worker 执行期临时目录，按 task_id 分目录
└── exports/               # CLI 导出用
```

## 3. 模块职责与依赖关系

依赖方向是**单向**的，没有环：

```text
config ── errors ── db ── storage
                        │
        recipes/registry ┴─ recipes/schema ── plan
                        │                     │
                     modules/* ───────────────┤
                        │                     │
                  tasks/state ── tasks/queue ── tasks/service ── tasks/runner
                                                      │
                                                    api / cli / demo
```

| 文件 | 职责 | 明确**不**做的事 |
| --- | --- | --- |
| `config.py` | 路径布局、心跳/租约/超时/并发/保留期 | 不做业务判断 |
| `errors.py` | 错误分类（决定是否重试）、退避公式 | 不决定任务最终状态 |
| `db.py` | DDL、连接、事务、审计、旧库补列 | 不做业务校验 |
| `storage.py` | 对象读写、短时签名令牌 | 不判断素材是否可用 |
| `recipes/registry.py` | 模块白名单与参数模型 | 不执行任何模块 |
| `recipes/schema.py` | 配方结构 / 白名单 / 参数 / 依赖闭包 / 策略校验 | 不碰磁盘 |
| `plan.py` | 配方 → 可执行计划（能力检查、输入角色、指纹） | 不跑 ffmpeg |
| `modules/*` | 各能力的具体实现 | 只有 `media.encode` 物化文件 |
| `tasks/state.py` | 状态机与迁移校验 | 不写业务字段 |
| `tasks/queue.py` | 抢占、租约、心跳、退避、失联回收 | 不执行任务 |
| `tasks/service.py` | 配方版本、任务、产物、审计的业务规则 | 不跑 ffmpeg |
| `tasks/runner.py` | 执行与检查点 | 不改变业务规则 |
| `api.py` | HTTP 边界：认证、授权、限流、错误码映射 | 不实现业务规则 |
| `cli.py` | 命令行薄封装 | 不实现业务规则 |

## 4. 处理模块白名单（13 个）

注册表是**代码里的字面量**，配方改不动它；配方只能引用已注册的名字。
`import drama_cloud.modules` 即完成注册，`registry.ensure_modules_loaded()` 保证 CLI/API/Worker
三条入口都拿到同一份完整的注册表。

| 模块 | 能力标签 | 依赖角色 | 产出角色 | C 编号 |
| --- | --- | --- | --- | --- |
| `media.probe` | `ffprobe` | — | video / audio / metadata | 基础 |
| `video.quality` | `video_filters` | video | video_filters | C4 |
| `frame.fps_convert` | `video_filters` | video | video_filters | C9 |
| `frame.deflicker` | `video_filters` | video | video_filters | C9 |
| `frame.inspect` | `signalstats` | video | frame_report | C9 |
| `timeline.user_edits` | `ffmpeg` | video, audio | timeline | C5 |
| `chapters.reorg` | `ffmpeg` | timeline | chapters | C10 |
| `commentary.rebuild` | `ffmpeg` | audio | timeline | C11 |
| `audio.quality` | `audio_filters` | audio | audio_filters | C7 |
| `subtitle.mux` | `subtitles` | video | subtitle_plan | C3 |
| `metadata.policy` | `ffmpeg` | metadata | metadata_policy | C8 |
| `media.encode` | `h264_encode` | video | encoded | C6 |
| `media.quality_gate` | `ffprobe` | encoded | quality_report | §10 门禁 |

**依赖闭包校验**：每一步的 `requires` 必须已被输入素材（video/audio/metadata）或前序步骤满足，
否则配方在**发布时**就被拒绝——不会跑到一半才报错。

**能力检查分两层**：配方级 `compatibility.required_capabilities` 一次性快速失败；
模块级 `capability` 逐步检查，让「这个 Worker 到底能不能跑」在创建任务时就有答案。

## 5. 关键设计决策

| 决策 | 原因 |
| --- | --- |
| 模块「先写计划、后统一物化」 | C4/C5/C7/C8/C9 全部变成纯函数，可单测、不碰磁盘；只有 `media.encode` 会调 ffmpeg 出片 |
| 配方快照冻结在任务里 | 发布新版本不会改变正在跑或已排队的任务（设计文档 §6.1 的硬要求） |
| 状态机集中在一处 | 「不能由客户端任意覆盖」只能靠单一实现点保证，API 层不写 UPDATE tasks.state |
| 队列用数据库而不是 Redis | 本机无 Redis；文档本身也要求「关键任务状态仍以数据库为准」，避免两套真相 |
| 检查点只做阶段级 | 文档明确「不尝试恢复任意 FFmpeg 进程的内部状态」；只有产出文件的阶段才允许复用 |
| 取消是协作式的 | 在阶段边界检查 `cancel_requested`，不用信号打断 ffmpeg，避免留下半个文件 |

## 6. C1–C11 覆盖情况

| 编号 | 功能 | 实现位置 | 状态 |
| --- | --- | --- | --- |
| C1 | 云端配方下发 | `recipes/` + `GET /api/v1/recipes/resolve` | 完成 |
| C2 | 配方版本策略（不可变/审核/灰度/撤回/回滚） | `tasks/service.py` + `recipe_versions` 表 | 完成 |
| C3 | 三档模式 no_edit / subtitle / full_edit | `preset.mode` + 配方校验强制约束 | 完成 |
| C4 | 画面处理 | `modules/filters.py` `video.quality`（分辨率/e.q/unsharp/hqdn3d） | 完成 |
| C5 | 时间线 | `modules/timeline.py` `timeline.user_edits`（含场景检测候选） | 完成 |
| C6 | 编码 | `modules/encode.py` `media.encode`（容器×编码矩阵、时间线编译、元数据、章节） | 完成 |
| C7 | 音频 | `modules/audio.py` `audio.quality`（loudnorm/compressor/EQ/dynaudnorm + 响度测量） | 完成 |
| C8 | 元数据 | `modules/encode.py` `metadata.policy`（keep/strip/edit + 定位字段清理） | 完成 |
| C9 | 帧级（细化后） | `frame.fps_convert` / `frame.deflicker` / `frame.inspect` | 部分：**插帧未实现**（需 GPU + 模型），配方写不存在的模块会被白名单拒绝 |
| C10 | 分集重组 | `modules/timeline.py` `chapters.reorg` + `split_chapters` | 完成 |
| C11 | 解说重剪（需人工审核） | `modules/timeline.py` `commentary.rebuild` | 部分：脚本↔转写匹配与候选时间线完成；**本机未内置 ASR**，转写需作为参数提供 |

## 7. 与设计文档的差异

| 设计文档建议 | 本实现 | 原因与影响 |
| --- | --- | --- |
| PostgreSQL | SQLite（WAL + `BEGIN IMMEDIATE`） | 本机无 PG 服务。差异被限制在 `db.py`：只用 TEXT/INTEGER/REAL，JSON 存 TEXT。迁移点：`AUTOINCREMENT`→`GENERATED AS IDENTITY`、`?`→`%s` |
| Celery + Redis | SQLite 持久化队列 + 租约/心跳 | 本机无 Redis。抢占语义用事务实现：`UPDATE ... WHERE state IN (...) AND id = ?` 的 rowcount 决定谁抢到 |
| S3 兼容对象存储 | 本地文件系统，保留同一套接口语义 | 本机无对象存储。`storage.py` 是唯一替换点；上传会话与下载授权已经是短时签名令牌 |
| Tauri / Electron 桌面客户端 | API 自带的 Web 客户端 | 本机 glib2 版本低于 Tauri 2 要求，装不了。见 `PACKAGING_GUIDE.md` |
| Prometheus / Grafana 监控（可选） | `GET /api/v1/system/status` + `audit_events` 表 + `drama-cloud status` | 学习原型不引入监控栈；指标口径已在状态端点里固定下来 |
| 阶段 3–5 的完整交付 | 阶段 1/2 完成，阶段 3/4 的功能模块已实现并可组合执行 | 见 §8 |

## 8. 实施路线与当前进度

- **阶段 1 基础闭环** —— 完成。登录/令牌 → 上传 → 素材探测 → 配方读取 → 任务创建 → 转码 → 质量门禁 → 取产物；
  一条任务可从提交走到成功或明确失败，配方版本与产物哈希可追踪（`README.md` 的验收项由 `cloud/demo.sh` 覆盖）。
- **阶段 2 任务可靠性** —— 完成。持久化队列、并发配额（多 Worker）、重试分类、取消、心跳与租约回收、
  阶段检查点、保留期清理。`Worker` 被 `kill -9` 后任务会在租约过期后被回收重排队。
- **阶段 3 编辑模块** —— 模块已实现并组合可用；各模块有单测，组合执行时质量门禁校验音画同步与目标规格。
- **阶段 4 复杂编排** —— 分集重组、解说候选时间线、C9 三个明确子能力已实现；解说需人工审核（`approve=false` 只出候选）。
- **阶段 5 跨平台发布** —— 未做（需要 Windows/macOS 构建机与签名证书）。方案与验收清单见 `PACKAGING_GUIDE.md`。

## 9. 已知限制

1. **无 GPU**：光流插帧、模型类修复（E2FGVI 一类）不在本原型范围内，也没有对应的注册模块。
2. **单机存储**：`storage.py` 用本地磁盘，没有分片上传、没有副本，也没有多机共享。
3. **无 TLS**：`serve` 默认监听 `127.0.0.1` 的明文 HTTP。对外暴露必须放到反向代理后面并启用 TLS（设计文档 §10 的硬要求）。
4. **限流是单进程内存**：多实例部署要换成集中式计数器。
5. **无 ASR**：C11 需要外部提供转写时间戳。
6. **时间线不含音频**：`_segment_args` 对每个片段同时做 `[i:a]`，所以多片段剪辑要求素材带音轨。
