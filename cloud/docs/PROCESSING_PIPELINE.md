# 处理管线（Processing Pipeline）

> 交付物之三。讲清「一份配方快照 + 一组素材」是怎样一步步变成产物文件的：
> 计划怎么构造、模块怎样分工、什么时候写检查点、失败与取消如何收尾、门禁检查什么。
>
> 相关代码：`src/drama_cloud/plan.py`、`tasks/runner.py`、`tasks/queue.py`、`modules/*`。

## 1. 总览

```text
配方快照 + 输入素材
        │  plan.build_plan()          ← 能力检查 + 输入角色 + 参数冻结
        ▼
   ProcessingPlan（纯数据，可测、不碰磁盘）
        │  存进 tasks.plan_json，Worker 直接照它执行
        ▼
   Worker.execute()
        ├─ 装载输入到 work/  ──► MediaContext
        ├─ 逐步执行模块        ──► ctx.plan（要做什么）
        ├─ media.encode        ──► 一次 ffmpeg 调用物化文件
        ├─ media.quality_gate  ──► 通过 / QUALITY_GATE_FAILED
        └─ 登记产物            ──► artifacts + run_report.json
```

关键设计：**模块只声明「要做什么」，只有 `media.encode` 真的编码**。
C4/C5/C7/C8/C9 这些步骤把滤镜、时间线、元数据累积到 `ctx.plan` 的不同分区里，
最后由编码步骤编译成**一条** ffmpeg 命令。这样多步处理不会反复重编码，也不会产生中间世代的画质损失。

## 2. 计划构造（`plan.build_plan`）

输入：`Recipe` + `PlanInput[]` + Worker 能力集；输出：`ProcessingPlan`。

按顺序做四件事：

1. **配方级能力检查**：`compatibility.required_capabilities` 里有 Worker 缺失的能力 → `capability_missing`，
   并且 `min_worker_version` 高于当前 Worker 版本时同样拒绝。
2. **输入校验**：至少一份素材；角色必须在 `video`/`audio`/`subtitle`/`metadata` 内；
   必须存在 `video` 角色。
3. **逐步展开**：每个步骤解析成 `PlannedStep`，参数经模块模型校验后冻结成 plain dict
   （进而在任务里落盘，执行期不会再被改动）；
   同时做**模块级能力检查**——`spec.capability` 不在可用集合里同样失败。
4. **输入下标校验**：`subtitle.mux` 的外挂字幕下标必须指向 `subtitle` 角色的输入；
   `timeline.user_edits` / `commentary.rebuild` 引用的下标必须在范围内。

计划还带一个**指纹**（`fingerprint()`），由以下内容算 SHA-256：

- 配方内容哈希 `recipe_sha256`；
- Worker 版本 + 引擎版本（`WORKER_VERSION` / `ENGINE_VERSION`）；
- 每份输入的 `role:sha256`；
- 每个步骤的 `id`、`module` 和排序后的参数。

指纹是**检查点能不能复用**的判据：任何一项变了都必须重跑。

计划里出现 `capability_missing` 时任务在**创建阶段**就失败，不会跑到一半才发现。

## 3. 输入角色与装载

| 角色 | 用途 |
| --- | --- |
| `video` | 主视频；`ctx.primary` 指向它 |
| `audio` | 额外音轨（例如解说的配音素材） |
| `subtitle` | 外挂字幕文件（SRT/ASS） |
| `metadata` | 元数据来源 |

Worker 装载输入（`_stage_inputs`）：

1. 从数据库按 `asset_ids` 取出 `asset_id` / `storage_key` / `sha256` / `role`；
2. 对象不存在 → `storage_unavailable`（可重试）；
3. 用**硬链接**把素材链进 `work/<task_id>/input_00.mp4` 这类路径，失败时回退到复制——
   硬链接让「装载」几乎零成本，也不会因为工作目录清理而破坏原素材；
4. `ctx.primary` = 第一个 `video` 角色输入，全部输入按顺序放进 `ctx.inputs`
   （配方里的 `input_index` 就是这个列表的下标）。

## 4. `MediaContext` 与 `ctx.plan` 分区

`MediaContext` 是一次任务执行期间的共享状态：

| 字段 | 含义 |
| --- | --- |
| `settings` | 运行配置（含 ffmpeg/ffprobe 路径） |
| `work_dir` | 本次任务的临时目录，可整体删除 |
| `primary` | 主视频路径 |
| `inputs` | 全部输入路径（按 `input_index` 排列） |
| `probe` | `media.probe` 写入的摘要（时长/分辨率/帧率/轨道数…） |
| `plan` | 各模块累积的「要做什么」，见下表 |
| `encoded` | `media.encode` 产出的文件路径 |
| `artifacts` | 额外要登记的产物（如字幕文件、报告） |
| `notes` | 人类可读的执行说明，写进 `run_report.json` |

`ctx.plan` 的分区约定（各模块只写自己的键，互不覆盖）：

| 键 | 写入者 | 消费方 |
| --- | --- | --- |
| `quality_policy` | 任务创建时预置 | `media.quality_gate` |
| `video_filters` | `video.quality` / `frame.fps_convert` / `frame.deflicker` | `media.encode` |
| `audio_filters` | `audio.quality` | `media.encode` |
| `timeline` | `timeline.user_edits` / `commentary.rebuild` | `chapters.reorg`、`media.encode` |
| `chapters` | `chapters.reorg` | `media.encode`、产物登记 |
| `subtitle` | `subtitle.mux` | `media.encode`、门禁字幕轨检查 |
| `metadata_policy` | `metadata.policy` | `media.encode` |
| `audio_measurement` | `audio.quality` | 报告 |
| `encode` | `media.encode` | 门禁（校验编码器/容器） |
| `target_resolution` | `video.quality` | 门禁（校验分辨率） |
| `quality_report` | `media.quality_gate` | 产物登记、任务结果 |
| `scene_candidates` | `timeline.user_edits` | 人工审核 |
| `frame_report` | `frame.inspect` | 报告 |

## 5. 模块执行契约

每个模块注册一个 `implement(ctx, params) -> dict`：

```python
def implement_video_quality(ctx, params: VideoQualityParams) -> dict:
    chain = ctx.plan_section("video_filters", [])
    ...                     # 只往 ctx.plan 里追加
    return {...}            # 返回值只用于报告
```

约定：

- **只读 `ctx`，只写自己的分区**；不读别人的中间变量（顺序由配方显式声明）。
- **不直接调用 ffmpeg**（探测/抽检/测量这类只读工具是例外），编码一律交给 `media.encode`。
- 需要临时文件时用 `ctx.scratch("name")`，落在工作目录里，任务结束一起删。
- 参数模型是 `StrictModel`（`extra="forbid"`），未声明的字段直接拒绝。
- 抛 `WorkError(error_class, message)` 表达失败，错误分类决定是否重试。

模块的 `requires` / `produces` 角色就是数据依赖：`media.encode` 的 `video` 角色既可能来自
输入素材，也可能被 `video.quality` 覆盖（滤镜链），所以在 `ctx.plan` 里表现为「追加滤镜」而不是
「替换视频」。

## 6. 一次性编码：计划 → ffmpeg

`media.encode` 是唯一物化文件的模块，它把 `ctx.plan` 编译成一条 ffmpeg 命令：

1. **时间线编译**（`_segment_args`）：把 `timeline.segments` 展开成
   `trim`/`atrim` + `concat` 的 `filter_complex` 片段；空 `segments` 等价于整片。
2. **滤镜链**：`video_filters` / `audio_filters` 依次接上；软字幕走 `-c:s` 封装，
   烧字幕走 `subtitles` 滤镜。
3. **容器矩阵检查**（`CONTAINER_MATRIX`）：容器与编解码组合非法 → `target_unsupported`。
4. **元数据**：`metadata.policy` 决定 `-map_metadata` 与 `-metadata` 参数；
   所有 `-i` 输入集中放在输出选项**之前**（ffmpeg 对参数位置敏感）。
5. **章节**：`chapters` 产出 ffmetadata 文件，通过 `-i chapters.ffmetadata` 载入，
   `-map_metadata` 指向正确的输入序号。
6. 输出到 `work/<task_id>/output.<ext>`，作为 `ctx.encoded`。

超过 `quality_policy`/参数的时长上限（`timeline.max_output_seconds`）在计划阶段就被拒绝。

## 7. 阶段检查点与复用

每个步骤执行完都会写一条 `task_checkpoints`：

| 列 | 含义 |
| --- | --- |
| `stage` | 步骤 id |
| `input_sha256` | 计划指纹（配方 + Worker/引擎版本 + 输入 + 参数） |
| `recipe_version` | 配方版本 |
| `engine_version` | 引擎版本（换 ffmpeg 就重跑） |
| `params_digest` | 该步骤参数的 SHA-256 |
| `artifact_key` / `artifact_sha256` | 只有物化文件的步骤才有 |
| `completed` | 是否完成 |

**复用规则（`_try_reuse`）**：只有 `REUSABLE_MODULES = {media.encode}` 允许复用，
并且五要素全部一致才复用：

1. 计划指纹相同；
2. 配方版本相同；
3. 引擎版本相同；
4. 参数摘要相同；
5. 检查点记录的产物文件存在且 sha256 一致。

复用 `media.encode` 时，`ctx.encoded` 直接指向上一轮的产物文件，并从冻结的计划里重建
`ctx.plan["encode"]`（门禁要用它校验编码器/容器）——**复用路径没有跑过 `implement_encode`**，
所以这一步重建是必须的。

> 计划型模块（`media.probe`、`timeline.user_edits` 等）**不允许复用**：它们的产物是内存里的
> `ctx.plan`，没有可落盘状态；跳过它们会让后续步骤拿到空上下文。它们本身几乎不耗时，
> 重跑才是正确做法。

这对应设计文档「不尝试恢复任意 FFmpeg 进程的内部状态」——只做阶段级、可验证的复用。

## 8. 取消（协作式）

- API/CLI 调 `request_cancel`：运行中的任务 → `CANCEL_REQUESTED`（置 `cancel_requested=1`）；
  尚未开始的任务直接 → `CANCELLED`。
- Worker 在**每个步骤边界**检查 `cancel_requested`，命中就清理工作目录并迁到 `CANCELLED`。
- 不用信号打断正在跑的 ffmpeg：避免留下半个文件，也避免把「已生成但未校验」的产物标成成功。
- 取消是终态；`CANCELLED` 的任务不会被回收重排。

## 9. 失败、重试与退避

错误分类（`errors.ErrorClass`）决定是否重试：

| 分类 | 可重试 | 典型来源 |
| --- | --- | --- |
| `transient_io` | 是 | 临时 IO 抖动 |
| `worker_lost` | 是 | 心跳失败 / 租约过期 |
| `storage_unavailable` | 是 | 素材对象缺失、写盘失败、磁盘满 |
| `resource_contention` | 是 | 内存不足、命令超时 |
| `input_corrupt` | 否 | 素材损坏、没有视频轨 |
| `recipe_schema_invalid` | 否 | 配方结构/白名单/依赖不合法 |
| `param_out_of_range` | 否 | 参数越界、非法状态迁移 |
| `target_unsupported` | 否 | 容器/编码组合不支持、编码器不存在 |
| `capability_missing` | 否 | Worker 缺能力、缺可执行文件 |
| `quality_gate_failed` | 否 | 门禁不通过（重跑结论相同） |
| `cancelled` | 否 | 用户取消 |
| `internal` | 否 | 未预期异常（带堆栈） |

重试策略：`MAX_ATTEMPTS = 3`，退避 `BACKOFF_BASE_SECONDS=5`、`BACKOFF_MAX_SECONDS=300`，
`backoff_delay(attempt) = min(5 · 2^(attempt-1), 300)`，写进 `tasks.next_run_at`；
到期后才重新可被抢占。

失败时 Worker 先写 `task_attempts.finished_at / error_class / error_message`；
决定重试时**保留工作目录**供检查点复用，不再重试时清理。

## 10. 租约、心跳与失联回收

| 概念 | 实现 |
| --- | --- |
| 抢占 | `TaskQueue.claim()` 在 `BEGIN IMMEDIATE` 事务里做 `UPDATE ... WHERE state IN (...)`，rowcount 决定归属 |
| 候选过滤 | 按 `priority ASC, created_at ASC` 取 25 条候选，逐个比对 `required_capabilities`，能力不足的**跳过**（留给有能力的 Worker，而不是判失败） |
| 租约 | `lease_owner` / `lease_expires_at`（默认 30s，`DRAMA_CLOUD_*` 可调） |
| 心跳 | 后台线程每 5s 续租；续租失败 → `LeaseLost`，Worker 停手 |
| 失联回收 | `reclaim_expired()` 把过期租约的任务按 `worker_lost` 处理：退避后重排队，超过 3 次则 `FAILED` |
| 状态闸门 | `_owns()`：只有租约持有者能改写任务状态，避免旧 Worker 覆盖新 Worker |

`claim()` 每次都会先跑一次 `reclaim_expired()`，所以不需要额外的看门狗进程也能自愈；
`drama-cloud cleanup` 只是可选的定期入口。

## 11. 质量门禁（`media.quality_gate`）

门禁是**成功与失败的分界线**：只有全部检查通过，产物才会登记为最终产物。
未通过 → `quality_gate_failed`（终态，不重试）。

| 检查 | 触发条件 | 判据 |
| --- | --- | --- |
| `file_exists` | 总是 | 产物存在且大于 0 字节 |
| `decodable` | 总是 | ffprobe 能探测出视频轨且时长 > 0 |
| `target_video_codec` | `validate_target_profile` | 实际编码符合 `media.encode.video_codec` |
| `target_resolution` | `validate_target_profile` 且声明了分辨率 | `fit`/`pad` 按「不超过框」判断；`fill` 要求完全相等 |
| `target_container` | `validate_target_profile` | 容器族匹配（`mp4` 与 `mov` 视为同族） |
| `duration` | 有期望时长 | `|实际 − 期望| ≤ duration_tolerance_seconds`（默认 1.0s） |
| `min_duration` | 总是 | `实际 ≥ quality_policy.min_duration_seconds` |
| `av_sync` | `check_audio_video_sync` 且有音轨 | 音视频时长差 `≤ max_av_sync_ms`（默认 200ms）；无音轨时跳过并记明 |
| `audio_track` | 总是 | 「源有音轨且未被 `audio_codec=none` 丢弃」⇔ 产物有音轨 |
| `subtitle_track` | 软字幕模式 | 产物真的带字幕轨 |
| `artifact_hash` | 总是 | 能算出产物 SHA-256 |

报告写进 `ctx.plan["quality_report"]`，并随任务结果返回：

```json
{
  "ok": true,
  "checks": [{"name": "decodable", "ok": true, "detail": "1920x1080 @ 25.0fps"}],
  "sha256": "…", "size_bytes": 1234567,
  "media": {"duration": 12.3, "width": 1920, "height": 1080, "has_audio": true}
}
```

## 12. 产物登记与保留期

`_publish_artifacts` 把以下内容登记进 `artifacts` 表：

| `kind` | 内容 | 说明 |
| --- | --- | --- |
| `output` | `ctx.encoded` | 主产物 |
| `chapter` | 分集文件 | 仅 `chapters.split=true`；stream-copy，不重编码 |
| `report` | `run_report.json`（`name=run_report.json`） | 任务报告：探测摘要、步骤、笔记、门禁报告 |
| 其他 | `ctx.artifacts` 里的条目 | 例如字幕文件、抽检报告 |

每条产物记录 `sha256`、`size_bytes`、`media_json`、`quality_json`、`retain_until`。
`retain_until` 由 `settings.artifact_retention_days` 决定（`0` 表示不自动清理）；
`drama-cloud cleanup` / `cleanup_expired_artifacts()` 删除过期对象与记录。

产物登记表 `artifacts` 与素材表 `assets` 分开：素材是输入，产物是输出，生命周期不同。

## 13. 并发与配额

- Worker 可以起多个，队列的抢占语义保证同一任务只会被一个 Worker 执行；
- 单机并发由 `Settings.effective_concurrency` 推算（默认 `cpu_count // 4`，可用
  `max_concurrency` 覆盖）——原型里这个值用于文档与运维口径，
  真正的并行度由「起几个 `run-worker` 进程」决定；
- `drama-cloud status` / `GET /api/v1/system/status` 暴露队列深度与各状态任务数，用来判断要不要加 Worker。
