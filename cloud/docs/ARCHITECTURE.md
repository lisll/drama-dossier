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
│     · initdb / seed / submit --wait / task / status / cleanup / demo       │
└───────────────────────────────┬──────────────────────────────────────────┘
                                │ HTTPS（可开） + Bearer 令牌
┌───────────────────────────────▼──────────────────────────────────────────┐
│ 控制面 API（api.py，FastAPI）                                             │
│   identity.py  拿访问令牌向 commerce 的 /internal/introspect 换出用户身份  │
│   认证与角色（viewer < operator < admin，角色只由本地白名单决定）            │
│   主体级限流 · 幂等键 · 分片上传（uploads.py）                             │
│   配方发布/解析 · 任务创建/查询/取消/重试                                  │
│   credit_client.py  建任务前向 commerce 预扣，终态时结算或退还             │
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
│   21 个白名单模块，见 §4           │   │  本地文件系统实现                │
│   计划型模块只写 ctx.plan          │   │  · 对象键不透明，调用方不拼路径    │
│   只有 media.encode 物化文件       │   │  · 上传/下载用短时 HMAC 令牌      │
└───────────┬───────────────────────┘   └──────────────────────────────────┘
            │ 调用外部进程
┌───────────▼──────────────────────────────────────────────────────────────┐
│ 媒体引擎：ffmpeg 4.2.4 / ffprobe（本机实测版本）                           │
│ 后期能力：`dossier-media`（合成渲染/变速变调/音源分离/ASR/OCR/镜头切点）   │
│ 擦除能力：`dossier-inpaint`（含 `--progress-json` 逐帧进度）                │
│ 运行时核对：`dossier-runtime check`｜戳记核对：`dossier-artifacts check`    │
└──────────────────────────────────────────────────────────────────────────┘
```

## 2. 部署架构（本机实际形态）

单机四进程模型，全部在同一台 `openEuler 22.03 / aarch64 / 无 GPU` 机器上：

| 进程 | 启动方式 | 职责 |
| --- | --- | --- |
| API 服务 | `drama-cloud serve`（可加 `--tls-cert` / `--tls-key`） | HTTP(S) 控制面 + Web 客户端静态页 |
| Worker | `drama-cloud run-worker` | 抢任务、跑 ffmpeg、登记产物；可起多个，队列保证不重复 |
| 调度回收 | `drama-cloud cleanup`（可 cron） | 回收过期租约、清理过期产物 / 过期素材 / 放弃的分片上传 |
| CLI | `drama-cloud <cmd>` | 运维与演示；与 API 共用同一套 service 层 |

API 与 Worker **通过数据库解耦**：Worker 不反向调用 API，API 也不需要知道有多少个 Worker。
因此 Worker 可以单独重启、扩缩容，客户端升级不会影响媒体处理环境（对应设计文档 §8.2 的要求）。

这四个进程之外还有一个**独立的账号服务**（`drama_cloud.commerce`），它自己的部署与数据库见
`COMMERCE.md`；媒体控制面只在登录内省与扣费两处调用它，分工与代价见 §10。

`cloud/var/` 是运行目录，全部可删（删掉即回到出厂状态）。M5b 起数据库搬到 PostgreSQL，
由 `DRAMA_CLOUD_DSN` 指向，不再落在 `cloud/var/` 里：

```text
cloud/var/
├── secret.key             # 签名密钥（0600，首次运行自动生成）
├── storage/               # 对象存储根：assets/ 与 artifacts/
├── uploads/               # 分片上传的临时目录，按 upload_id 分目录（完成或放弃后删掉）
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
| `db.py` | PostgreSQL 连接、编号 SQL 迁移与 `schema_migrations` 记账、事务、审计 | 不做业务校验 |
| `migrations/*.sql` | 媒体控制面的表结构（编号迁移，见 `DATABASE_SCHEMA.md`） | 不含业务逻辑 |
| `identity.py` | 拿访问令牌向 commerce 内省换身份；区分「无效」与「问不到」 | 不判断角色，不建用户 |
| `credit_client.py` | 调 commerce 的内部积分接口做预扣 / 结算 / 退还 | 不记账、不判断该不该收费 |
| `uploads.py` | 分片上传会话、写分片、缺块查询、拼接与整文件校验、放弃 | 不建任务、不扣费 |
| `tls.py` | 启动时算 TLS 配置，半开 / 文件不可读 / 要求 TLS 却没给参数一律失败 | 不碰 uvicorn 之外的运行时 |
| `storage.py` | 对象读写、短时签名令牌 | 不判断素材是否可用 |
| `recipes/registry.py` | 模块白名单与参数模型 | 不执行任何模块 |
| `recipes/schema.py` | 配方结构 / 白名单 / 参数 / 依赖闭包 / 策略校验 | 不碰磁盘 |
| `plan.py` | 配方 → 可执行计划（能力检查、输入角色、指纹） | 不跑 ffmpeg |
| `modules/*` | 各能力的具体实现 | 只有 `media.encode` 物化文件 |
| `tasks/state.py` | 状态机与迁移校验 | 不写业务字段 |
| `tasks/queue.py` | 抢占、租约、心跳、退避、失联回收 | 不执行任务 |
| `tasks/service.py` | 配方版本、任务、产物、审计的业务规则；归属隔离；保留期清理 | 不跑 ffmpeg |
| `tasks/runner.py` | 执行、检查点、质量类重试与复核入队 | 不改变业务规则 |
| `tasks/review.py` | 人工复核队列（认领/裁决/修正重跑） | 不跑 ffmpeg |
| `api.py` | HTTP 边界：认证、授权、限流、错误码映射 | 不实现业务规则 |
| `cli.py` | 命令行薄封装 | 不实现业务规则 |

## 4. 处理模块白名单（21 个）

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
| `video.inpaint` | `video_inpaint` | video | video | C12 |
| `video.erase_verify` | `video_inpaint` | video | erase_verification | C12 |
| `media.encode` | `h264_encode` | video | encoded | C6 |
| `media.quality_gate` | `ffprobe` | encoded | quality_report | §10 门禁 |
| `video.render` | `media.render.compose` | video | video | Tier 2 合成渲染 |
| `audio.separate` | `media.audio.separate` | audio | stems | Tier 2 音源分离 |
| `audio.timestretch` | `media.audio.timestretch` / `media.audio.tempo_only` | audio | audio | Tier 2 变速变调 |
| `subtitle.asr` | `media.asr` | audio | subtitle_text | Tier 2 语音识别 |
| `subtitle.ocr` | `media.ocr` | video | ocr_report | Tier 2 区域识别 |
| `timeline.shots` | `media.shot.detect` | video | shots | Tier 2 镜头切点 |

后六个模块（Tier 2 后期能力）的能力标签来自 **`dossier-media probe --json`**
（`modules/media_cli.py`，结果按「工具路径 + 大小 + mtime」缓存），不靠环境变量声明：
哪台机器装了 rubberband / demucs / whisper / tesseract，由工具自己说了算。
可执行文件的查找顺序由 `toolchain.py` 统一规定（部署配置 → PATH → 仓库 `target/release|debug`），
三个 Rust 工具（`dossier-inpaint` / `dossier-media` / `dossier-artifacts`）走同一条规则。

**依赖闭包校验**：每一步的 `requires` 必须已被输入素材（video/audio/metadata）或前序步骤满足，
否则配方在**发布时**就被拒绝——不会跑到一半才报错。

**能力检查分两层**：配方级 `compatibility.required_capabilities` 一次性快速失败；
模块级 `capability` 逐步检查，让「这个 Worker 到底能不能跑」在创建任务时就有答案。

**能力可以被探测**（`ModuleSpec.capability_probe`，见 `docs/PROJECT_REVIEW.md` C2）：
`video.inpaint` 只有在**本机真的找得到 `dossier-inpaint` 可执行文件**时才声明
`video_inpaint`，否则不兼容的任务在计划阶段就被路由到别的 Worker，而不是领了任务再失败。
模型修复后端另用 `video_inpaint_model` 标签。它**不是**手工声明，而是一条注册进
`registry.register_capability` 的**探测函数**（`modules/inpaint.py::model_capability_available`）：
必须配了模型 worker（`DRAMA_CLOUD_INPAINT_WORKER`）；配了运行时清单
（`DRAMA_CLOUD_RUNTIME_MANIFEST`）时还要 `dossier-runtime check --manifest` 通过——
清单里少一个权重、摘要对不上，能力就如实缺席。`DRAMA_CLOUD_CAPABILITIES` 仍可补充声明，
但它不再是模型能力的唯一真相。

**「选哪个后端」也是能力**（`ModuleSpec.dynamic_capabilities`）：`video.inpaint` 的参数里
`inpainter=worker` 或 `detector=worker` 会额外要求 `video_inpaint_model`，这条在
**计划阶段**就检查（`plan.build_plan`）：缺了就 `capability_missing` 拒绝建任务，
而不是派给一台没有模型的 Worker、跑到一半才发现起不来。

**全部 21 个模块都有探测**（`modules/ffmpeg_caps.py` + `modules/media_cli.py`）：`ffmpeg` / `ffprobe` 按
`which` 语义探测（以前是无条件写死的，没装 ffmpeg 的机器也会声明「我会 ffmpeg」）；
依赖滤镜的模块按 `ffmpeg -filters` 判断（如 `video.quality` 需要
`scale/eq/unsharp/hqdn3d`），`h264_encode` 则要求 `libx264` **加上**拼时间线用的基础滤镜
（`trim/atrim/setpts/asetpts/settb/asettb/concat`）。
解析规则与 Rust 侧 `crates/dossier-runtime/src/probe.rs` **刻意保持镜像**，
避免同一台机器给出两个答案；探测失败（程序不在、非零退出、超时）一律**按缺能力处理**。

配方真正选用的编码器（h265/vp9/av1、mp3/opus、软字幕的 `mov_text`/`srt`）无法在
能力标签里穷举，所以在 `media.encode` 里做**编码前预检**（`ensure_encoders`）：
缺哪个就以 `capability_missing` 列出「编码器名 + 用途」，而不是等 ffmpeg 跑到一半
报 `Unknown encoder`（那会被归类成「参数不支持」，把排查方向带偏）。

**参数按能力拼**：控制面与外部工具是两个会各自发布的产物。`resolve_tool` 只保证「工具在」，
所以拼参数前还会读一次工具的 `--help`（结果按「路径 + 大小 + mtime」缓存，只问一次），
不认识的开关就不传，并把降级写进处理日志（`modules/inpaint.py::tool_flags`）。
这样「工具还是旧版」只会少一个校验维度，不会让整条擦除流程解析失败。

## 5. 关键设计决策

| 决策 | 原因 |
| --- | --- |
| 模块「先写计划、后统一物化」 | C4/C5/C7/C8/C9 全部变成纯函数，可单测、不碰磁盘；只有 `media.encode` 会调 ffmpeg 出片 |
| 配方快照冻结在任务里 | 发布新版本不会改变正在跑或已排队的任务（设计文档 §6.1 的硬要求） |
| 状态机集中在一处 | 「不能由客户端任意覆盖」只能靠单一实现点保证，API 层不写 UPDATE tasks.state |
| 队列用数据库而不是 Redis | 本机无 Redis；文档本身也要求「关键任务状态仍以数据库为准」，避免两套真相 |
| 检查点只做阶段级 | 文档明确「不尝试恢复任意 FFmpeg 进程的内部状态」；只有产出文件的阶段才允许复用 |
| 取消是协作式的 | 在阶段边界检查 `cancel_requested`，不用信号打断 ffmpeg，避免留下半个文件 |
| 长任务进度流两端都要接 | 只有生产端（工具写 JSONL）或只有消费端（界面读）都不算能力：`run_streaming` 按字节偏移增量读并丢弃半行，回调线程自建数据库连接写进度 |
| 产物一致性规则只写一份 | 擦除产出 `<输出>.stamp.json`，由 Rust 侧 `dossier-artifacts check` 汇总判定；云端 `artifacts check` 只搬运戳记并**透传退出码**，不重写第二套规则 |

## 6. C1–C12 覆盖情况

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
| C11 | 解说重剪（需人工审核） | `modules/timeline.py` `commentary.rebuild` + `modules/late_stage.py` `subtitle.asr` | 部分：脚本↔转写匹配与候选时间线完成；**ASR 已接线**（Whisper → SRT，产物可被 `subtitle.mux` 直接使用），但本机没有 whisper 与权重，真实转写未跑通，也可继续由参数直接提供转写 |
| C12 | 硬字幕/角标擦除 + 确认流 | `modules/inpaint.py` `video.inpaint` / `video.erase_verify`、`tasks/review.py`、Rust `crates/dossier-inpaint` | 完成：擦除、四项指标自动校验、质量类自动重试（加力/换后端）、人工复核队列与 mask 修正重跑、产物戳记与跨产物一致性校验（`dossier-artifacts check`）；**模型后端（DBNet/E2FGVI）只是接线**，需 GPU + 权重 |
| Tier 2 | 后期能力（渲染/分离/变速/ASR/OCR/镜头切点） | `modules/late_stage.py` + Rust `crates/dossier-media` | 完成接线：模块可被配方引用，能力按 `dossier-media probe` 如实缺席；**本机没有 demucs/whisper/tesseract/rubberband**，真实推理未跑通（用假工具测穿参数与产物链路）。`timeline.shots` 与 `video.render` 在本机**真的跑通**（ffmpeg 有 `select` 与 libass） |

## 7. 与设计文档的差异

| 设计文档建议 | 本实现 | 原因与影响 |
| --- | --- | --- |
| PostgreSQL | 已迁到 PostgreSQL（psycopg 3 + 编号 SQL 迁移） | M5b 起要支持多人、按用户隔离与可靠的并发事务，SQLite 的 `BEGIN IMMEDIATE` 不够用。差异仍集中在 `db.py`：JSON 与时间继续以 TEXT 存，换 `jsonb`/`timestamptz` 不需要改调用点 |
| Celery + Redis | 数据库持久化队列 + 租约/心跳 | 本机无 Redis。抢占语义用事务实现：`UPDATE ... WHERE state IN (...) AND id = %s` 的 rowcount 决定谁抢到 |
| S3 兼容对象存储 | 本地文件系统，保留同一套接口语义 | 本机无对象存储。`storage.py` 是唯一替换点；上传会话与下载授权已经是短时签名令牌 |
| 一次性上传整个文件 | 分片上传 + 断点续传（默认 8 MiB 一块，最多 10000 块） | 一部剧的素材动辄几个 GB，一次性 PUT 断了就得从头再来。分片落在 `home/uploads/<upload_id>/`，完成时按序拼接并校验整文件 sha256 |
| 只走 HTTPS | `serve` 默认明文，给 `--tls-cert` / `--tls-key` 就能直接起 HTTPS | 本机开发用自签证书，所以两个开关都不能省。配错（只给一个、文件读不到、要求 TLS 却没给）在**启动时**就失败，不悄悄退回明文 |
| 账号自己做在控制面里 | 媒体控制面不存账号，用桌面端的 commerce 访问令牌向 `/internal/introspect` 换身份 | 只有一处登录，两边 `user_id` 天然一致；角色仍由本库的 `media_admins` / `media_viewers` 白名单决定，不看 commerce 的角色字段 |
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

1. **无 GPU**：光流插帧没有对应模块；`video.inpaint` 的 `inpainter=worker` / `detector=worker`
   只完成了接线，本机没有 onnxruntime 与模型权重，模型路径无法端到端验证（`column` 基线可跑通）。
2. **存储仍是单机本地磁盘**：`storage.py` 用本地文件系统，没有副本，也没有多机共享。
   分片上传与断点续传已有（`uploads.py`），但分片同样落在这台机器的 `home/uploads/` 下，
   换机器就续不上。
3. **TLS 要显式开启**：`serve` 默认监听 `127.0.0.1` 的明文 HTTP。给了 `--tls-cert` / `--tls-key`
   就直接起 HTTPS，但证书本身不在这里签发、也不做 ACME 续期；部署方要求必须 TLS 时用
   `DRAMA_CLOUD_REQUIRE_TLS=1`，此时不给证书会拒绝启动。对外暴露仍建议放到反向代理后面。
4. **限流是单进程内存**：多实例部署要换成集中式计数器（commerce 那边的限流在数据库里，两侧口径不同）。
5. **ASR 已接线但本机跑不通**：`subtitle.asr`（Whisper → SRT）已实现并有假工具测试，
   但本机没有 whisper 与模型权重，能力标签会如实缺席——C11 的转写目前仍可由参数直接提供。
   同理，`audio.separate`（demucs）、`subtitle.ocr`（tesseract）、`audio.timestretch` 的
   「变速不变调」（rubberband）、`video.render`（需 `libass`/`drawtext`）都只在有对应外部
   程序的机器上才声明能力。
6. **时间线不含音频**：`_segment_args` 对每个片段同时做 `[i:a]`，所以多片段剪辑要求素材带音轨。
7. **桌面壳未接新能力**：`src-tauri` 因 glib 版本编译不过，本轮新增的模块与命令
   （`video.render` / `subtitle.asr` / … 与 `artifacts check`）只在 CLI 与 API 可用。
8. **服务端处理依赖另外两样东西**：没有 PostgreSQL（`DRAMA_CLOUD_DSN`）起不来，
   没有 `DRAMA_CLOUD_COMMERCE_URL` 与 `DRAMA_CLOUD_INTERNAL_SECRET` 时任何需要登录的请求都返回 503
   ——不会退回「不鉴权」。commerce 挂掉时媒体控制面也跟着不可用（见 §10）。

## 10. 两个服务的分工

M5b 起这一带有**两个服务、两套库**，它们各自负责不同的事：

| | commerce（`drama_cloud.commerce`） | 媒体控制面（`drama_cloud`） |
| --- | --- | --- |
| 管什么 | 账号（注册/登录/令牌）、卡密与使用权、积分账本、支付与邀请 | 任务、素材、产物、人工复核队列 |
| 库 | commerce 自己的 PostgreSQL schema（`commerce/migrations/`） | 另一套 PostgreSQL schema（`migrations/`） |
| 谁调谁 | 被调用方：只提供 `/internal/*` 给同机房服务用 | 调用方：靠 `identity.py` 内省令牌、靠 `credit_client.py` 扣费 |
| 起不来的后果 | 登录、充值、兑换全部不可用 | 需要登录的请求一律 503，不会退化成「不鉴权」 |

**为什么分成两个服务**：处理任务再重也不该拖慢登录，两者的部署方式、生命周期和备份策略本来就不一样；
而且合成一套会牵动 commerce 那一整套迁移与测试，风险大于收益（`db.py` 的模块注释）。

**为什么分成两套库**（而不是同一套 PostgreSQL 里的不同 schema 之外再合表）：两边的 `users` 不是同一种东西。
commerce 的 `users.id` 是 `uuid`，媒体控制面的 `users.id` 是 TEXT（应用层生成的字符串 id），
硬塞进一张表会直接撞在类型上——`cloud/tests/e2e_m5b.py` 里那条注释就是为了钉住这件事。
媒体控制面保留的 `users` 表现在只是 commerce 身份的**镜像**：`id` 用 commerce 的 `user_id`，
`name` 用邮箱，`token_hash` 填 `commerce:<user_id>` 这个永远不会被真实令牌命中的占位值（`auth.py`）。

**代价**，都是明确的：

1. **扣费不能进本地事务**。`hold` 要发 HTTP 到 commerce，那是另一个进程、另一个数据库，
   所以预扣只能放在**建任务之前**；代价是「扣到了但建任务失败」必须退还，
   `POST /api/v1/tasks` 用 `except BaseException` 兜住这一步（进程被取消时也不能把钱留下）。
   没扣到钱就不该占队列资源，这是把预扣前置的理由。
2. **钱还是同一本账**。服务端处理扣的是 commerce 的 `credit_ledger`，与报审包、本机处理版本共用一个余额；
   媒体库自己的 `billing_*` 表保留为「可独立运行的账本」（CLI 与既有测试在用），
   两条路互斥：任务上有 `credit_ref` 走 commerce，有 `billing_quote_id` 走本地，两者都空就不扣费。
3. **结算 / 退还失败只能留痕**。任务已经跑完时 commerce 可能连不上，这时**不把任务判成失败**
   （视频确实做出来了），而是把失败显式返回并写进审计（`credit.settle` / `credit.release`），
   由人工对账。静默吞掉会让钱悄悄消失。
4. **多一个必须配的环境变量**。`DRAMA_CLOUD_COMMERCE_URL` 与 `DRAMA_CLOUD_INTERNAL_SECRET` 两个都配了才算接上；
   少一个时内省与扣费都按「服务不可用」处理（503），而不是放行。
5. **两个服务都要备份**。账在 commerce，产物与素材在这里，只备份一边都还原不出完整状态。
