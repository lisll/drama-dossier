# 硬字幕 / 角标擦除与确认流

> 交付物之四（对应处理管线第 3、4 步）。讲清「标识擦除」这一步在云端是怎么被编排、
> 怎么被自动校验、校验不过之后又怎么自动重试与转人工的。
>
> 相关代码：`modules/inpaint.py`（本文件主角）、`tasks/review.py`（人工复核队列）、
> `tasks/runner.py`（重试与入队）、Rust 侧 `crates/dossier-inpaint`（真正的擦除执行者）。
> 擦除算法本身的原理与参数见仓库根目录的 `docs/SUBTITLE_REMOVAL.md` 与 `docs/WATERMARK_REMOVAL.md`。

**仅用于技术学习与处理自有素材**：可清理自有素材的硬字幕与角标/台标，不做 DRM 绕过，
也不处理他人作品的版权标识。

## 1. 在完整业务 Pipeline 里的位置

```text
原始视频
  ↓
① 任务编排 & 区域/时间配置          配方 pipeline：video.inpaint 的 regions / start / duration
  ↓
② 标识检测 + 时序 Mask 生成          dossier-inpaint 内部（fixed 框选 或 worker 模型检测）
  ↓
③ 视频 Inpaint 擦除                  dossier-inpaint 内部（column 基线 或 worker 时序修复模型）
  ↓
④ 擦除结果自动校验（确认流核心）      工具测量指标 → video.erase_verify 按配方阈值裁决
  ↓
分支 ✅ 自动确认通过 → 输出干净视频 + 处理日志（`*.inpaint-report.json`）
分支 ❌ 自动校验失败 → 分支A：自动重试（加大 mask 膨胀 / 换更强后端），最多 3 次
                    → 分支B：送入人工复核队列（`review_queue`）
  ↓
人工复核 → 标记通过 / 标记失败 / 修正 mask 后重跑（新建任务）
```

要点：**测量与裁决分离**。工具只负责把「擦除后画面到底怎么样」量出来（残留、闪烁、接缝、
一致性四组指标），至于是不是可以交付，由配方里的 `video.erase_verify` 按自己的阈值裁决。
这样调阈值不需要重跑一次擦除，也比「工具内置一个阈值 + 外面再抄一份」更难写歪。

## 2. 两个模块

### 2.1 `video.inpaint` · 擦除（C12）

| 参数 | 类型 / 默认 | 说明 |
| --- | --- | --- |
| `input_index` | int / `0` | 要处理的输入下标；擦除后该路输入被替换成擦除结果 |
| `regions` | `[{x,y,width,height}]` / `[]` | 固定标识区域；`detector=fixed` 时必填，`detector=worker` 时作为「只在区域内采纳检测框」的约束 |
| `start` / `duration` | float / 无 | 时间窗（秒）：只处理这一段，窗外帧原样输出 |
| `start_frame` / `end_frame` | int / 无 | 帧级时间窗（优先于秒） |
| `detector` | `fixed` \| `worker` / `fixed` | 定位后端：固定区域 或 模型检测（DBNet 一类） |
| `inpainter` | `column` \| `worker` / `column` | 修复后端：本地基线 或 时序视频修复模型 |
| `dilate` | int / `4` | 掩码膨胀半径（像素）：太小留 logo 轮廓，太大伤正常画面 |
| `feather` | int / `2` | 回贴羽化半径 |
| `context` | float / `1.6` | ROI 相对检测框的外扩比例 |
| `region_pad` | int / `8` | 检测框落在区域内的判定外扩量 |
| `temporal_smooth` | int / `1` | 掩码时序平滑半径（相邻帧掩码取并集，抑制漏检闪烁） |
| `min_score` | float / `0.5` | 检测置信度下限 |
| `clip_frames` / `probe_frames` | int / `30` / `60` | 修复片段帧数 / 定位阶段抽检帧数 |
| `fps` | float / 无 | 覆盖素材帧率（帧率不可信时使用） |
| `vcodec` / `crf` / `preset` | str/int/str / `libx264` / `16` / `medium` | 中间态编码；`lossless` 中间态由 `pix_fmt` 控制 |
| `pix_fmt` | str / `yuv444p` | 角标这类彩色小目标对色度二次抽样敏感，默认不做 4:2:0 |
| `defer_verdict` | bool / `false` | `true` = 工具内部结论只记录、由 `video.erase_verify` 裁决（配方里带确认流步骤时用它） |
| `escalation` | `off` \| `mask` \| `mask+backend` / `mask` | 自动重试的加力方式，见 §4 |
| `timeout_seconds` | float / `1800` | 单次调用超时 |

**可执行文件与 worker 解释器不在配方里**：它们由部署配置给出（`DRAMA_CLOUD_INPAINT_BIN`、
`DRAMA_CLOUD_INPAINT_WORKER`、`DRAMA_CLOUD_INPAINT_WORKER_ARGS`）。配方只能选「用哪个后端」，
不能指定命令——这是 §10「不执行配方内任意命令」的直接落地。

擦除结果会**替换该路输入**（`ctx.inputs[i]` 与 `ctx.primary` 指向擦除后的文件），
所以后面的 `video.quality` 滤镜与 `media.encode` 看到的就是干净画面。
工具写出的处理日志登记为 `inpaint_report` 产物（`inpaint_report.json`），
内容与 `crates/dossier-inpaint` 的 `--report-json` 完全一致。

### 2.2 `video.erase_verify` · 确认流闸门

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `require_report` | `true` | 没有擦除报告视为配方写错，直接失败（不静默放行） |
| `require_content_check` | `true` | 没有内容级检测结果（如 `detector=fixed`）时是否仍要求人工确认 |
| `accept_warn` | `true` | `warn` 结论放行但转人工抽查；设 `false` 表示警告也不放行 |
| `residual_ratio_warn` / `_fail` | `0.02` / `0.10` | 掩码内残留文本像素占比 |
| `flicker_warn` / `flicker_fail` | `1.2` / `3.0` | 闪烁指数（掩码内帧间差 ÷ 掩码外参考帧间差） |
| `seam_warn` / `seam_fail` | `1.0` / `2.0` | 接缝分数（掩码边界梯度 ÷ 参考带梯度） |
| `psnr_warn_db` / `psnr_fail_db` | `30.0` / `24.0` | 掩码内一致性 PSNR（无干净参考时的真值代理），低于阈值即警告/失败 |

判据按「越界即分级」执行：`> fail` 记 `fail`，`> warn`（PSNR 则是 `<`）记 `warn`，
取最严重的一项作为结论。

## 3. 自动校验的四组指标

指标全部只在亮度（Y）上、只在掩码内/边界的小区域上计算，因此可以流式跑完整片。

| 指标 | 度量 | 直觉 |
| --- | --- | --- |
| 残留 `residual` | 对输出帧重跑同一检测器，掩码内仍被判为文本的像素占比 | 「还有没有没擦掉的字」；`fixed` 检测器无法判断内容，会记 `checked=false` |
| 闪烁 `flicker` | 掩码内帧间差的均值 ÷ 掩码外参考带的帧间差 | 「擦除区域是不是在闪」 |
| 接缝 `seam` | 掩码边界内外亮度梯度 ÷ 参考带梯度 | 「贴回去的块和周围接不接得上」 |
| 一致性 `consistency` | 掩码内亮度 vs 前若干帧中位数的 PSNR | 「擦完的块时序上稳不稳」 |

工具自身的 `verdict`（`pass` / `warn` / `fail`）也写进报告；`defer_verdict=false` 时
`video.inpaint` 直接采用它，`defer_verdict=true` 时以 `video.erase_verify` 的结论为准。

## 4. 分支A：自动重试怎么"换参数"

`quality_retryable` 是**可重试**错误分类，但它的重试不是「原地重跑」——重跑必须换参数，
否则只会得到同一个结论。加力规则由 `plan_escalation()` 决定：

| 尝试 | `mask`（默认） | `mask+backend` |
| --- | --- | --- |
| 第 1 次 | 按配方原参数 | 按配方原参数 |
| 第 2 次 | `dilate +2`、`feather +1`、`context ×1.1`、`temporal_smooth +1` | 同左 |
| 第 3 次 | 同左（继续加力） | 同左，并**切换修复后端** `column → worker`（前提是部署方配了 worker） |

上限：`dilate ≤ 32`、`feather ≤ 12`、`context ≤ 4.0`、`temporal_smooth ≤ 5`。
第 3 次尝试若想换后端却没配 worker，会写一条说明并保持原后端（不会静默变成别的行为）。

调度侧的配套改动有两个：

- 质量类重试使用**固定短等待**（`QUALITY_RETRY_DELAY_SECONDS = 1.0`）而不是指数退避——
  它重跑是为了换参数，不是在等外部故障恢复；
- 进入质量类重试时**清空该任务的阶段检查点**：上游画面会变，任何已完成的编码产物都不再可信。
  普通可重试错误（IO/租约）照旧保留检查点以复用编码结果。

重试次数与全局一致：`MAX_ATTEMPTS = 3`。

## 5. 分支B：人工复核队列

自动重试次数用尽后，任务落到 `FAILED`，并同时登记一条待复核记录（不再"判死"就完事）。
`warn` 结论（包括 `detector=fixed` 时无法确认残留的情况）会在**产物正常产出**的同时
登记一条待抽查记录。

失败路径也会保留一份**候选产物**：最后一遍擦除结果的视频会以 `inpaint_candidate`
类型登记成产物并挂在这条复核记录上（质量信息里写明 `auto_verified: false`）。
理由很直接——没有实物就没法做人工判定，「标记通过」也就无从谈起。
候选产物**不是**成品：它没有通过自动校验，放行必须是人的明确动作，不会是系统的默认结论。

### 5.1 数据模型

`review_queue` 表（见 `docs/DATABASE_SCHEMA.md`）：

| 列 | 说明 |
| --- | --- |
| `id` | `rev_…` |
| `task_id` | 关联任务 |
| `artifact_id` | 待抽查/放行的产物；`warn` 是正常成品，`auto_verify_failed` 是最后一遍擦除结果留下的候选产物（`inpaint_candidate`，`auto_verified: false`） |
| `reason` | `auto_verify_failed` / `auto_verify_warn` |
| `severity` | `info` / `warn` / `high` |
| `status` | `pending` / `approved` / `rejected` / `superseded` |
| `detail_json` | 擦除参数（regions / escalation / summary）、逐项判据、修正提示 |
| `claimed_by` / `created_at` / `decided_by` / `decided_at` / `decision_note` | 认领与裁决痕迹 |

同一个任务 + 同一个原因的 `pending` 记录只保留一条（重复触发不会堆一屏）。

### 5.2 能做什么

| 动作 | 语义 | 约束 |
| --- | --- | --- |
| `claim` | 认领，避免两人同时处理 | 已被他人认领时返回 409 |
| `approve` | 标记通过（人工确认画面可用） | **必须有产物**；失败任务放行的是候选产物，等于人工接受「自动校验没过但画面可接受」 |
| `reject` | 标记失败 | 任何时候都可以 |
| `rerun` | 按修正后的参数重跑 | 生成**新任务**，原记录转 `superseded`；原任务的参数、attempt 记录、审计都不被改写 |

`rerun` 的参数覆盖形态与创建任务一致（`{"<步骤 id>": {"已有参数": 值}}`），
所以「修正 mask」就是覆盖 `video.inpaint` 的 `regions` / `dilate` / `start` 等已有参数：

```bash
drama-cloud review-rerun rev_ab12cd34 \
  --param '{"erase":{"regions":[{"x":0,"y":0,"width":96,"height":56}],"dilate":6}}' \
  --note "扩大区域覆盖整条 logo" --wait
```

### 5.3 端点与命令

| 接口 | 说明 |
| --- | --- |
| `GET /api/v1/reviews?status=pending&task_id=` | 列出复核记录（viewer） |
| `GET /api/v1/reviews/{review_id}` | 单条详情（viewer） |
| `POST /api/v1/reviews/{review_id}/claim` | 认领（operator） |
| `POST /api/v1/reviews/{review_id}/decision` | `{"decision":"approve"\|"reject","note":"…"}`（operator） |
| `POST /api/v1/reviews/{review_id}/rerun` | `{"user_params":{…},"note":"…"}`（operator） |
| `GET /api/v1/tasks/{task_id}` | 任务详情里的 `review` 字段是复核投影（`pending` 数量、条目摘要） |

CLI：`drama-cloud reviews --status pending`、`drama-cloud review <id>`、
`drama-cloud review-approve|review-reject|review-rerun <id>`。

**原型限制**：复核状态**不会**阻断产物下载（`approved` 之前也能取到文件）。生产环境应当在
下载端点上校验复核状态，或给未放行的产物单独存放区；本原型只保证「状态可见 + 记录可审计」。

## 6. 端到端验收记录（本机实测）

环境：openEuler 22.03 / aarch64 / 无 GPU；素材用 ffmpeg 现场合成，不含第三方内容。

| 场景 | 命令要点 | 结果 |
| --- | --- | --- |
| 自动确认通过 | 320×180 纯色背景 + 左上角 64×48 白底 logo，`drama.logo.erase` | `SUCCEEDED`（1 次尝试）：接缝 0.00、闪烁 0.00、一致性 88.0 dB、无复核记录 |
| 自动重试并加力 | 同一配方换成 `testsrc2` 高细节画面（接缝天然很大） | 第 1/2/3 次尝试依次 `dilate 4→6→8`，均判 `fail` |
| 转人工复核 | 上一场景耗尽 3 次 | `FAILED` + `review_queue` 一条 `auto_verify_failed`（含指标与修正建议）+ 一份 `inpaint_candidate` 候选产物；`approve` 放行候选、`review-reject` + `review-rerun` 生效 |
| mask 修正重跑 | `--param '{"erase":{"regions":[{"x":0,"y":0,"width":96,"height":56}],"dilate":6}}'` | 新任务 `QUEUED`，计划里 `regions` 与 `dilate` 已按覆盖生效，原记录转 `superseded` |

失败场景是**正确行为**而非缺陷：`column` 基线修复在高细节背景上必然留下可见接缝，
四组指标如实把它判成 `fail`。要在这类素材上拿到 `pass`，需要 `inpainter=worker`
（时序视频修复模型）——那需要 GPU + 模型权重，属于部署配置。

## 7. 已知限制

1. **模型后端不在仓库内**：`inpainter=worker` / `detector=worker` 只是接线（`worker_args`
   由部署方给），本机没有 onnxruntime 与权重，也没有 GPU，无法端到端验证模型路径。
2. **`fixed` 检测器没有内容级校验**：残留指标记 `checked=false`，此时自动确认只能依赖
   闪烁/接缝/一致性三组图像指标；要自动确认「字真的没了」，必须用内容驱动检测器。
3. **区域必须盖住整个标识**：掩码之外的残留不属于可校验范围（实测里 logo 文字比区域宽，
   就留下了可见的残余笔画）。确认流会提示用 `regions` 修正后重跑。
4. **中间态会完整重编码**：擦除中间态与最终交付各编码一次；按区域重编码尚未实现。
5. **复核不阻断下载**（见 §5.3）。
6. **单掩码 worker 协议**：Rust 侧逐帧掩码已实现，但 worker 交换协议仍是单个 `mask.pgm`。
