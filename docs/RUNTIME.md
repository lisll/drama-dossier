# 运行时、后期与长任务能力层（Tier 1 / 2 / 3）

> 用途：本仓库只做技术学习。本文记录与 Grove Max 1.2.4 做过一次**纯技术**能力对比之后，
> 按「阻塞性缺口 → 能力空白 → 工程完整性」三档补齐的成果：每项能力落在哪个 crate、
> 怎么用、怎么验证、哪里仍然是空的。
>
> 三档的原始定义写在 `docs/PROJECT_REVIEW.md` §8.1（本轮从对比结论固定下来，避免口径漂移）。

## 0. 一页对照

| 档 | 缺口（对比结论） | 落地位置 | 库侧单测 |
| --- | --- | --- | --- |
| Tier 1 | 运行时/模型交付体系；GPU、硬件编码与并行度协商；模型端到端可用性 | `crates/dossier-runtime` | 97 |
| Tier 2 | 音源分离、变速变调、ASR、OCR 识别、镜头切点、图像合成渲染 | `crates/dossier-media` | 60 |
| Tier 3 | 长任务进度流与断点续跑、管线版本化与产物一致性；应用内更新/凭据/设备指纹 | `crates/dossier-core`（前两项）+ `crates/dossier-runtime`（第三项）+ 进度流的**生产/消费接线**（见 §3.5） | 68 + 3（集成） |

三档都要求「库侧能力」之外还有**云端接线**，否则能力等于不存在：Tier 1 的模型能力要能在
计划阶段如实缺席（§1.7），Tier 2 的后期模块要能被配方引用并真的执行（§2.3），
Tier 3 的进度流要有人写、有人读（§3.5），产物戳记要有人产、有人校验（§3.3）。

三档共用同一条铁律：**不捆绑外部工具**、**不静默降级**、**业务规则在库里、命令层极薄**。
所有外部调用都经过 `dossier_runtime::CommandRunner` 注入，因此无 GPU、无模型权重、
无 `rubberband`/`whisper`/`tesseract` 的机器上，也能把「该怎么决策」的分支测穿；
只有「真的跑一次外部程序」那部分需要真环境。

## 1. Tier 1 — `crates/dossier-runtime`

### 1.1 它解决什么

缺这一层时，模型类能力「接线了但不可用」：权重靠手工放、模型装没装没法确认、
硬件编码器其实初始化不了却仍被选中、模型 worker 需要一个 Python 却不知道是哪个。
所以这一层的职责是**把「这台机器现在能做什么」变成可查询、可复现、可回滚的事实**。

### 1.2 模块

| 模块 | 职责 | 关键类型 |
| --- | --- | --- |
| `digest` | SHA-256（字节/文件/流式）与小工具 | `sha256_file` / `sha256_bytes` / `is_sha256_hex` |
| `command` | 可注入的命令执行器（**全部外部调用都走这里**） | `CommandSpec` / `CommandRunner` / `SystemRunner` / `FakeRunner` |
| `probe` | 只报事实：工具版本、`ffmpeg -filters/-encoders`、`nvidia-smi` | `probe_ffmpeg_capabilities` / `probe_gpu` / `FilterList` / `EncoderList` |
| `manifest` | 运行时组件清单（角色、版本、文件、摘要、URL） | `RuntimeManifest` / `Component` / `RuntimeRoles` |
| `store` | 校验 → 安装 → 状态 → 回滚（失败不污染当前版本） | `RuntimeStore` / `StoreConfig` / `Downloader` / `Archiver` |
| `caps` | 把事实翻译成决策，且每条决策带理由 | `probe_environment` / `plan_encoder` / `evaluate_gpu` / `ParallelismPreset` |
| `update` | 更新通道：签名清单 + 验签 + 版本比较（fail closed） | `UpdateChannel` / `plan_update` / `Verifier` |
| `secret` | 本地凭据存储（`0700/0600` + 原子写，**不是** OS 钥匙串） | `SecretStore` / `mask` |
| `device` | 设备指纹（只输出摘要，不外发、不采集强标识） | `compute_device_fingerprint` |

### 1.3 三件「决策」的判定规则

- **GPU 够不够格**：显存下限 + 计算能力下限两条都满足才算 `eligible`，不够时在报告里写明
  卡在哪一条；不满足就退回 CPU 路径，而不是「先试了再说」。
- **硬件编码器用哪一个**：三级判断——① 编译进来了吗；② 硬件设备在吗
  （`/dev/dri/renderD128`、`/dev/video*`）；③ 用一帧真编码**实测**。
  第 ③ 步是必需的：本仓库构建机上 `h264_vaapi` 编译进去了、设备节点也在，
  但 `libva` 初始化失败——只看前两级就会选中一个跑不起来的编码器。
  实测失败时带原因回退到下一档（`plan_encoder` 的候选链）。
- **并行度**：`automatic` / `configured` / `effective` / `maximum` 四档分开呈现，
  不把「用户指定」和「实际生效」混成一个数字。

### 1.4 命令

```text
dossier-runtime probe                      # 这台机器能干什么（工具/滤镜/编码器/GPU/并行度）
dossier-runtime selfcheck [--manifest m.json] [--json]
                                           # 交付前自检：ffmpeg 可用、有编码器、有 Python、
                                           # 清单组件齐全、凭据文件权限是否过宽
dossier-runtime check --manifest m.json    # 清单里的组件装齐了没（含摘要校验）
dossier-runtime install --manifest m.json --id e2fgvi --from ./models/e2fgvi
dossier-runtime install --manifest m.json --id e2fgvi        # 走 URL 下载（curl/wget）
dossier-runtime rollback  --manifest m.json --id e2fgvi
dossier-runtime manifest-gen --dir ./models/e2fgvi --id e2fgvi \
    --role inpaint_model --version 1.0       # 由目录生成清单条目（摘要自动算）
dossier-runtime update --channel channel.json --public-key cosign.pub --current 1.0.0
dossier-runtime device                     # 设备指纹（只有摘要）
dossier-runtime secret put cloud.token     # 从 stdin 读，避免进 shell 历史
```

### 1.5 设计要点

- **验签 fail closed**：签名坏了就是「不知道这个包是什么」，不会降级成「先装上再说」。
  验签器是一条链（`openssl` → `node`）：先问 `openssl version`，1.1.1 这类不支持 ED25519 的
  后端会被判为「用不了」而自动换下一个，且**「用不了」和「拒绝了这个签名」是两种结论**——
  前者换后端，后者立即失败（有专门的单测钉住这个区别）。
- **安装不污染现场**：先下到 staging、校验摘要、解压、再切换 `current` 指针；
  任何一步失败都不动当前版本（`install_then_rollback_switches_pointer_without_redownload`）。
- **组件角色表**（`RuntimeRoles`）与模型目录约定绑在一起，配 `is_model(role)` 判断，
  所以「模型文件放哪」不再靠文档口头约定。
- **不自研密码学**：Ed25519 验签交给用户环境已有的 `openssl` / `node`。

### 1.6 与云端的关系（同一台机器不能有两个答案）

云端 Worker 的能力路由要回答同一个问题（「本机到底会不会这个」），所以
`cloud/src/drama_cloud/modules/ffmpeg_caps.py` 的解析规则与 `probe.rs`
**刻意保持镜像**：都不依赖「滤镜清单没有分隔线、编码器清单有分隔线」这个差异，
都按「探测不到 = 确实没有」处理，并在解析失败时**按缺能力处理**。

### 1.7 模型能力怎么被云端判定（Tier 1 的最后一公里）

有了清单校验工具，还要让它真的参与路由，否则「模型装齐了没有」依然只写在文档里：

- `registry.register_capability(tag, probe)`：整机能力（不属于任何模块的那些）注册**探测函数**，
  探测为假、或探测自己抛异常，都按「没有这个能力」处理（不能因为探测炸了就假装有）。
- `video_inpaint_model` 的探测（`modules/inpaint.py::model_capability_available`）分两级：
  ① 必须配了模型 worker（`DRAMA_CLOUD_INPAINT_WORKER`）；② 配了运行时清单
  （`DRAMA_CLOUD_RUNTIME_MANIFEST`）时，还要 `dossier-runtime check --manifest` 通过
  （真的去核文件与摘要）。没配清单时只是**弱保证**，报告里如实这么写。
- `ModuleSpec.dynamic_capabilities`：某些能力取决于**参数**。`video.inpaint` 选
  `inpainter=worker` 或 `detector=worker` 就多要一条 `video_inpaint_model`，
  `plan.build_plan` 在**建任务时**就拒绝（不再依赖部署方手写 `DRAMA_CLOUD_CAPABILITIES`）。

这三条合起来的效果：把一份 `inpainter=worker` 的配方提交到一台没有模型的机器上，
任务**创建不成功**并说明缺哪条能力——而不是排队、领走、跑到一半报 worker 起不来。

## 2. Tier 2 — `crates/dossier-media`

### 2.1 模块

| 模块 | 能力 | 依赖的外部程序 | 缺了会怎样 |
| --- | --- | --- | --- |
| `tools` | 盘点工具链，产出能力标签（`media.*`） | ffmpeg / ffprobe / rubberband / demucs / whisper / tesseract | 逐个报「缺什么、装什么」 |
| `render` | 合成渲染：文字叠加、图片（角标）叠加、字幕烧录 | ffmpeg（`drawtext` / `overlay` / `subtitles`） | 缺滤镜 → 构建计划时显式报错 |
| `shot` | 镜头切点检测（`scene` 分数 + 合并阈值）；云端 `timeline.shots` 直接调它，产物登记成 `shot_list` | ffmpeg（`select` 滤镜） | 缺滤镜 → 能力 `media.shot.detect` 缺席 |
| `timestretch` | 变速/变调；优先 Rubber Band，否则 `atempo` 链 | rubberband（可选） | 要求「变速不变调」却没有 rubberband → 显式拒绝 |
| `separate` | 音源分离（Demucs，two/four stems） | demucs（Python 包或 CLI） | 显式报错并给出安装方式 |
| `asr` | 语音识别转字幕（Whisper，`openai`/`whisper.cpp` 两种口味） | whisper CLI / whisper.cpp | 显式报错 |
| `ocr` | 区域抽帧 + OCR **文本识别**（Tesseract TSV） | ffmpeg + tesseract | 显式报错 |
| `srt` | 字幕文件读写（ASR/OCR 的公共出口） | — | — |

`ocr` 这一条补的是对比里点名的缺口「只有检测没有识别」：`dossier-inpaint` 侧的 DBNet
只给「哪里有字」，`ocr` 负责「写的是什么」。抽帧与识别分两步，区域用 `Region` 表达，
识别结果按置信度过滤（`OcrLine::is_confident`）。

### 2.2 命令

```text
dossier-media probe --ffmpeg ffmpeg --ffprobe ffprobe --json
dossier-media render --input in.mp4 --output out.mp4 --text "第 1 集" --font-file /path/字体.ttf
dossier-media render --input in.mp4 --output out.mp4 --logo logo.png,120 --subtitles a.srt \
    --subtitle-style Fontsize=32,Outline=2 --run
dossier-media shots --input in.mp4 --threshold 0.35 --min-gap 0.2 --duration 720 --json
dossier-media speed --input voice.wav --output fixed.wav --speed 1.08 --pitch -1.5 \
    --allow-pitch-shift
dossier-media separate --input mix.wav --output-dir stems --mode two --device cpu
dossier-media asr --input voice.wav --output-dir subs --flavor openai --model small --language zh
dossier-media ocr --input ep01.mp4 --output-dir ocr --x 0 --y 900 --width 1920 --height 180
```

除 `probe` 外，各子命令默认**只打印将要执行的命令**（可审阅），加 `--run` 才真的执行。

### 2.3 执行入口（库层的 `execute`，不是第二套实现）

「打印命令」和「真的跑」曾经只有 CLI 才有：库层能规划、能拼命令，却没有执行入口。
于是云端要么自己拼命令行（等于第二套实现），要么干脆接不上。现在六个计划类型各有一个
`execute`，CLI 的 `--run` 与云端的模块都调它，**命令只拼一次**：

| 计划类型 | 执行入口 | 跑完之后核对什么 |
| --- | --- | --- |
| `TimeStretchPlan` | `execute(runner, ffmpeg, rubberband)` | 输出文件存在且非空 |
| `SeparatePlan` | `execute(runner, demucs)` | 预期 stem 逐个存在（不是「跑完就算成功」） |
| `AsrPlan` | `execute(runner, program)` | 回读 SRT 并数出条目数（`load_transcript`） |
| `RegionFramePlan` | `execute(runner, ffmpeg)` | 按编号收集**真的抽出来**的帧 |
| `RecognizePlan` | `execute(runner, tesseract)` | 读回 TSV 并解析成行 |
| `RenderPlan` | 由 `media.encode` 物化 | —（渲染不走 `--run`，它是计划型模块） |
| —（`shot::detect_shots` 直接调用） | `dossier-media shots --json --run` → `timeline.shots` | 从混合输出里取 JSON；取不到就报 `internal`，不把「读不出结果」当成「没有切点」 |

外部调用一律经 `dossier_runtime::CommandRunner` 注入，所以「跑成功但没产物」「产物为空」
「工具不在（`ToolMissing`）」这三类分支在无 GPU、没装 demucs/whisper/tesseract 的机器上
也能用 `FakeRunner` 测穿。云端 `dossier-media` 的失败分类还补了一条：shell 退出码 **127**
（「命令找不到」）归为 `capability_missing`——重试多少遍都不会变好。

### 2.4 实测过的坑（写在代码注释与测试里）

- `drawtext` 的转义：`:` 要 2 个反斜杠、`'` 要 3 个、`\` 要 **4** 个（不是 3），
  `\n` 要写成**裸换行**（字面 `\n` 会被当成两个字画出来）；`expansion=none` 下 `%` 与 `=`
  不用转义。这组结论是用 `textfile=` 造参考帧、逐像素比对暴力枚举出来的
  （`escape_table_matches_textfile_reference_pixel_for_pixel`）。
- `overlay` 不能用空滤镜链：无预处理时直接写 `[1:v]`，不能造 `[1:v][img0]`。
- 中文字幕/水印必须显式给 `fontfile=`，默认字体没有汉字。

## 3. Tier 3 — 长任务工程完整性

### 3.1 `dossier-core::progress` — 进度流

- `TaskProgress`：阶段序号 + 阶段名 + 单元数（`stage_index/stage_total/units_done/units_total`），
  `fraction()` 与 `overall_fraction()` 分开算（阶段内进度 ≠ 总进度）。
- `ProgressEvent`：`seq`（单调递增）+ `at` + `kind` + `progress`；`ProgressRecorder` 以
  **JSONL 追加写 + 每行 flush**，所以进程被 kill 也能保留已落盘的部分。
- 拒绝乱序 `seq`；进入终态后只接受 `Started`（防止旧任务的事件串进新任务）。
- `resume()` 续写时**丢弃最后一行截断的记录**（崩溃时最后一行可能写了一半），
  这是「续跑」能用的前提。

### 3.2 `dossier-core::checkpoint` — 断点续跑

- `Checkpoint`：`StepState ∈ {Pending, Running, Done, Failed, Skipped}` + 每步 `ArtifactRef`。
- `input_fingerprint(&[(&str, &str)])`：把输入路径与摘要排序后哈希，作为「输入变了吗」的判据。
- `resume(Some(fp))`：指纹不一致直接拒绝续跑；`Running` 回退成 `Pending`（上次没跑完）；
  `Done` 但产物缺失/摘要不符也回退成 `Pending`，并写入 `reset_reason` 说明为什么重置。
- 保存是原子的（`.tmp<pid>` + `rename`），所以检查点本身不会被写成半个文件。

### 3.3 `dossier-core::pipeline_version` — 管线版本与产物一致性

- `ArtifactStamp`：管线名 + 版本 + 任务 id + 时间 + 工具 + 参数摘要 + 输入/输出摘要，
  与产物**并排**保存（`out.mp4` → `out.mp4.stamp.json`，原子写）。
- `check_consistency` 能区分四类偏差：`PipelineVersionMixed`（同批产物来自不同管线版本）、
  `ToolVersionMixed`（工具版本不一致）、`ParamsMismatch`、`InputMismatch`。
- 内置管线名与版本：`ERASE_PIPELINE` / `SUBTITLE_PIPELINE` / `REPORT_PIPELINE`（均为 `1.0.0`），
  用 `pipeline_version(name)` 查询。
- 为什么需要它：对比里点名的症状是「工具不认识某个开关」（第三轮复核踩过），
  当时的补丁是按 `--help` 探测；有了版本戳记，才能回答「这批产物是不是同一套流程出的」。

**生产端与消费端都已接上**（只有类型定义不算能力）：

- 生产：`dossier-inpaint` 跑完默认写 `<输出>.stamp.json`（`--no-stamp` 关掉，
  `--stamp-json <路径>` 换位置）。戳记里带管线名与版本（`ERASE_PIPELINE` = 1.0.0）、
  工具名与版本、`input_fingerprint`（检测器/修复器/ROI/窗口/掩码参数/编码参数/校验阈值）、
  输入 sha256、输出与报告 sha256，写之前先 `validate()`。
  两种写失败的处理**刻意不同**：显式指定路径写不了 → 整次失败（用户要的就是这份戳记）；
  默认路径写不了 → 只告警（擦除本身已完成，不该把成果判成失败）。
  另外，**戳记与校验结论是两件事**：自动校验判 Fail（工具退出码 2）时戳记照写——
  戳记回答的是「这份产物是怎么来的」（管线/工具/参数/输入），与「这一版擦得干净不干净」无关；
  后者由 `inpaint_report.json` 与确认流负责。
- 消费：`dossier-artifacts check <路径...>` 递归收集 `*.stamp.json` 并跑
  `check_consistency`，退出码 **0 = 一致 / 1 = 不一致 / 2 = 读不出来**。
  云端 `drama-cloud artifacts check <task_id...>` 从数据库取出戳记产物、落到临时目录、
  调这个工具，并把**退出码原样透传**——一致性规则只有 `dossier_core::pipeline_version`
  一份实现，云端不重写。

典型用法是审一批分集：一次跑批里混进了旧工具产出的那一集，只看单个任务是看不出来的。

### 3.4 `dossier-runtime::{update, secret, device}`

对应 Tier 3 的「应用内更新 / 凭据存储 / 设备指纹」：

- 更新：签名清单 + 验签 + 版本比较 + 产物摘要（组件级安装/回滚也走 `store`）；
  **发布通道本身（服务器、密钥托管）不在本仓库内**，属明确不做。
- 凭据：`0700` 目录 + `0600` 文件 + 原子写 + 权限告警；不加密、不参与网络传输，
  值永不进日志（`mask` 只给前几位）。
- 指纹：`machine-id` / 主机名 / CPU / OS / arch 的摘要拼接，只输出 32 位摘要，
  并显式标注 `stable` 与否（只有主机名时是弱指纹）。

### 3.5 进度流的端到端接线

上面三个模块只提供**能力**；一个没人写的进度流、一个没人读的进度流都不算长任务能力。
本轮把「生产 → 消费」两端接上，链路上的每一段都能单独验证：

| 环节 | 位置 | 关键设计 |
| --- | --- | --- |
| 生产 | `dossier-inpaint --progress-json <路径>` | 两个阶段（`locate` 逐帧检测 / `inpaint` 逐片段修复）；每修完一个片段追加一条；定位阶段每 1/10 采一条（只在首尾上报与「卡死」无法区分） |
| 消费 | `cloud/modules/ffmpeg.py::run_streaming` | 后台线程按**字节偏移**增量读 JSONL，半行留给下一次；进程还在跑就把事件交给回调 |
| 落地 | `cloud/tasks/runner.py::StepProgress` | 回调线程内自建 SQLite 连接（见下），把事件写进 `tasks.progress_json` |
| 展示 | `drama-cloud task <id>` / `_wait_for_task` | 有逐帧进度时显示 `stage done/total（百分比）`，否则退回阶段名 |

几个必须写下来的取舍：

- **分母是估计，且只会向上修正**：擦除是流式读帧，开始前拿不到真实帧数（`nb_frames`
  大量素材不写）。所以 CLI 用 `时长 × 帧率` 并按处理窗口截断得到估计值，管线每次上报用
  `max(估计, 实际已完成)` 当分母——估计偏小只会让分母跟着涨，绝不会写出
  `unit_done > unit_total` 这种会被记录器直接拒收的事件（`pipeline::inpaint_total`）。
- **失败也要留终态**：管线错误时尽力补一条 `Failed`（原错误优先，不被「进度写不进去」顶替）；
  没检测到标识、ROI 为空、作业为空这三条「原样输出」的早退路径同样写 `Progress + Finished`，
  否则进度文件永远停在「运行中」。
- **进度文件是可选开关**：不传 `--progress-json` 时行为完全不变；云端按 `--help` 探测这个
  开关，旧工具不认识就不加，只在 notes 里说明「本次没有逐帧进度」。
- **跨线程用 SQLite 必须自建连接**：`sqlite3` 的连接默认只能被创建它的线程使用。
  回调与心跳都跑在后台线程上，复用 Worker 主连接会抛 `ProgrammingError`；心跳把异常吞掉
  （心跳失败不该让 Worker 崩），于是**租约悄悄过期**、长任务被判「Worker 失联」。
  现在心跳线程与进度回调各自懒建连接（`Heartbeat._own_conn` / `StepProgress._local`），
  并有回归测试直接断言「租约时间被推进」——因为「线程没崩」根本不能当证据。

## 4. 验证方式

```bash
cargo test --offline -p dossier-core -p dossier-inpaint -p dossier-media -p dossier-runtime
cargo clippy --offline -p dossier-core -p dossier-inpaint -p dossier-media -p dossier-runtime \
    --all-targets -- -D warnings
cd cloud && PYTHONPATH=src python -m pytest -q      # 云端能力探测与模块单测
python3 -m unittest discover -s tools/model_worker/tests
```

进度流接线的专项回归（含「心跳线程必须真的续租」这条）：

```bash
cargo test --offline -p dossier-inpaint progress
cd cloud && PYTHONPATH=src python -m pytest -q tests/test_progress_tail.py tests/test_runner.py
```

能力路由与产物戳记的专项回归：

```bash
# 动态能力要求（选了模型后端就得有 video_inpaint_model）、整机能力探测、artifacts check 透传退出码
cd cloud && PYTHONPATH=src python -m pytest -q tests/test_capability_routing.py
# 五个 Tier 2 模块的云端接线（用假 dossier-media 驱动，不依赖模型权重）
cd cloud && PYTHONPATH=src python -m pytest -q tests/test_late_stage.py
# 戳记的生产端与消费端（真实跑一次 320x240@12fps 合成素材）
cargo test --offline -p dossier-inpaint --test stamp_cli
cargo test --offline -p dossier-core --test artifacts_cli
```

手动走一遍「真的产出文件」的路径（本机实测：2.00s 的 440Hz 单音 → 1.34s）：

```bash
dossier-media speed --input tone.wav --output fast.wav --speed 1.5 --allow-pitch-shift --run
dossier-inpaint --input in.mp4 --output out.mp4 --roi 0,0,64,48 --inpainter column
dossier-artifacts check .          # 戳记 1 份…结论：一致
```

本机 `dossier-media probe --json` 的实际结论（其余四个标签因缺程序而缺席）：

```text
capabilities: media.probe, media.render.compose, media.render.subtitle_burn,
              media.audio.tempo_only, media.shot.detect
missing:      media.audio.timestretch, media.audio.separate, media.asr, media.ocr
```

所以本机**真的能跑**的 Tier 2 模块是 `video.render` 与 `timeline.shots`
（`audio.timestretch` 只在接受变调时可用）；其余三个要有对应程序才会出现在能力清单里。

当前计数见 `docs/PROJECT_REVIEW.md` §8.4。

## 5. 已知局限与明确不做

| 项 | 状态 | 说明 |
| --- | --- | --- |
| GPU 推理（torch-cuda） | **阻塞**：本机 aarch64、无 GPU、无 torch wheel | `caps` 的 GPU 判定分支用假探测结果测穿；真机需要装 CUDA 版 PyTorch |
| 模型权重端到端 | **阻塞**：权重体量大且不在仓库内 | `fetch_models.sh` + `manifest-gen` + `selfcheck` 已就位，装上后可校验；未在本机跑通推理 |
| 浏览器自动化（自动登录/上传） | **不做**（合规边界） | 对比中出现过（Playwright + 视频号会话），本仓库不实现 |
| 桌面壳接线（把新 crate 接进 Tauri） | **阻塞**：glib 2.68 < 2.70、缺 webkit2gtk-4.1 | `src-tauri` 只能 `rustfmt` + 人工审阅；需要 `webkit2gtk4.1-devel` 才能编译验证 |
| 同一进程内对同一路径并发原子写 | 残留局限 | 临时文件名按「目标名 + pid」生成，同进程同目标并发写会互相覆盖（设计上是单写者） |
| 桌面端消费进度流 | **阻塞**（同上一行） | 进度流的生产端（`dossier-inpaint`）与云端消费端都已接好；桌面端要等 `src-tauri` 能编译才能把 JSONL 接进界面事件 |
| 逐帧进度的「事后回看」 | 残留局限 | 任务进度会被**后续阶段**覆盖（`progress_json` 是单值），所以 done/total 只在运行期间可见；跑完之后要看帧数请读 `inpaint_report.json`（`framesIn`/`framesOut`）或处理日志里的末条进度 |
| 五个 Tier 2 模块的真实推理 | **阻塞**：本机没有 demucs / whisper / tesseract / rubberband | 能力探测、参数拼装、产物回收、错误分类都用假工具与 `FakeRunner` 测穿；真机上装了对应程序并 `cargo build` 后即可直接用（模块能力标签会自动出现） |
| 模型权重端到端的**推理链路** | **阻塞**：同上（无 GPU、权重不入库） | `dossier-runtime check --manifest` 与 `video_inpaint_model` 探测已能把「装没装齐」变成硬结论；但没有权重就无法产出真实擦除结果 |
