# DramaDossier × Grove Max 1.2.4 —— 功能与技术实现差异对比

> 用途声明：仅用于技术学习与工程对照。**未修改任何代码**（仓库保持 `9ff2c0d` 干净状态）。
> Grove Max 部分为静态逆向结论，不提供破解/复刻/规避方案；不用于处理他人作品的版权标识。
> 分析日期：2026-10-14

---

## 第 0 部分 · 一句话结论

两者都是 Tauri 2 桌面应用、都做「短剧素材 → 成片/材料」的本地处理，但**产品目标完全相反**：

| | DramaDossier（剧备） | Grove Max 1.2.4 |
|---|---|---|
| 目标 | 把**自有素材**整理成**可复核、可提交**的报审材料包 | 把**别人的成片**批量**改头换面**并**自动搬运**到新账号发布 |
| 核心资产 | 规则引擎 + 确认流 + 审计链 | 去重配方（服务端强控）+ 一键分发 |
| 边界 | 不伪造签章、不改他人标识、不做分发 | 内建免责条款把违法风险转移给用户 |
| 开源 | MIT，可审计 | 闭源商业软件 |

**一句话**：DramaDossier 在「确认流 / 可复核 / 可测试 / 可审计」上是 Grove Max 的**严格超集**；
Grove Max 在「对抗性去重 / 自动分发 / 商业化」上有 DramaDossier **刻意不做**的能力。

---

## 第 1 部分 · DramaDossier 项目全景梳理

### 1.1 定位与边界（来自 `README.md`）
- 本地优先桌面应用：**素材扫描 → 媒体探测 → 合规检查 → 材料包生成**
- 明确边界：不绕 DRM、不清洗盗版素材、不伪造公章/签名/时间戳、不生成虚假成本数据、
  **不向外分发**；擦除能力**仅限自有素材的角标/台标**
- 无遥测、不上传素材、不执行下载的可执行文件

### 1.2 代码规模（实测）

| 区域 | 行数 | 文件 | 职责 |
|---|---:|---:|---|
| `crates/dossier-core` | 3,323 | 11 | 领域核心：扫描/探测/检查/打包/转码建议/差异/平台档案/剪映草稿/设置 |
| `crates/dossier-inpaint` | 7,407 | 12 | 擦除管线：探测/掩码/检测/时序修复/回贴/合音轨/校验/常驻 worker |
| `src-tauri/src` | 438 | 4 | 薄命令层（16 个 IPC 命令） |
| `src`（前端） | 1,448 | 3 | 原生 HTML/CSS/JS，无框架 |
| `cloud/src` | 7,989 | 30 | FastAPI 控制面 + 队列 + 状态机 + 15 个处理模块 + 复核队列 |
| `tools/model_worker` | 2,159 | 5 | Python 模型 worker（DBNet 检测 + E2FGVI/LaMa/ProPainter 适配） |
| `cloud/tests` | 2,219 | 13 | 云端测试 |

### 1.3 分层架构
```text
桌面壳 (Tauri 2, src-tauri/src: lib.rs / dossier.rs / erase.rs)
   │ 16 个 IPC 命令（界面只做状态展示与触发，业务规则全在 core）
   ▼
dossier-core  ────────────────►  dossier-inpaint
(扫描/探测/检查/打包/转码建议/       (probe → mask → detect → inpaint →
 差异/平台档案/剪映草稿/设置)        patch → frames → verify → worker)
                                        │ stdio 行协议 / JSON 协议
                                        ▼
                                  tools/model_worker (Python)
                                  dbnet.py / inpaint_backends.py / protocol.py
                                  ── 模型权重需自行 fetch（本机无 GPU/无 torch）
云侧（学习原型，独立进程）
   FastAPI 控制面 → tasks/{state,queue,service,runner,review} → modules/(15) → ffmpeg 4.2.4
```

### 1.4 桌面端功能模块（16 个命令对应）

| 域 | 命令 | 能力 |
|---|---|---|
| 素材 | `pick_folder` / `scan_library` | 目录扫描、集号识别（`第01集`/`E01`/`01.mp4`）、按集号排序 |
| 探测检查 | `analyze_drama` | `ffprobe` 取时长/分辨率/编码/帧率/码率；`pass/warn/fail/pending` 四态检查 |
| 设置 | `load_app_settings` / `save_app_settings` | 阈值/ffprobe 路径/输出目录持久化 |
| 打包 | `generate_package` | 剧集信息、封面海报、分集清单、成本表、权属声明、`manifest.json` |
| 转码建议 | `dossier::transcode_advice` | **只建议不执行**，产出可审阅的 ffmpeg 批处理脚本 |
| 差异对比 | `dossier::diff_report_packages` | 两份 `manifest.json` 的新增/移除/修改 |
| 平台档案 | `dossier::platform_profiles` / `platform_review` | 4 个内置档案：`vertical-short`（竖屏短剧）/`horizontal-drama`（横屏长剧）/`trailer-cut`（预告切片）/`generic-h264-mp4`（通用兜底） |
| 剪映互通 | `dossier::export_draft` / `import_draft` | 分集顺序与时长交接给剪辑工具，或把草稿时间线读回核对 |
| 擦除 | `erase::pick_video` / `erase_status` / `plan_erase` / `run_erase` | 检测工具状态 → 预览命令 → 执行 → 回显确认流结论 |

### 1.5 擦除管线（`dossier-inpaint`）

| 阶段 | 实现 |
|---|---|
| 探测 | `probe.rs`（ffprobe 取帧率/时长，只用外部 ffprobe，不捆绑 FFmpeg） |
| 掩码 | `mask.rs`（`MaskRect` 缩放/裁剪/膨胀/羽化） |
| 检测 | `detect.rs`：`FixedRoiDetector`（单框）/ `FixedRoisDetector`（多区域分组，各自独立 ROI）/ `WorkerTextDetector`（DBNet 模型，常驻 `WorkerSession`） |
| 修复 | `inpaint.rs` + `worker.rs`：本地 `column` 基线 或 worker 时序模型 |
| 回贴/编码 | `patch.rs` / `frames.rs`（两段式：`delivery` 1 代有损；`lossless` FFV1 母版；`-vf scale=flags=full_chroma_int+accurate_rnd` 固定） |
| 校验 | `verify.rs`：**5 组指标** —— 残留率 `residual_ratio`、闪烁指数 `flicker_index`、接缝分 `seam_score`、PSNR（含参考帧 PSNR）、SSIM；各带 `warn`/`fail` 双阈值 |
| 报告 | `--report-json` → `*.inpaint-report.json` |

CLI 关键开关：`--roi` `--dilate` `--feather` `--context` `--detector fixed|worker` `--inpainter column|worker`
`--region-group-area-ratio` `--persistent-worker` `--intermediate delivery|lossless` `--pix-fmt`
`--verify-*` 全套阈值、`--timeout`、`--report-json`、`--defer-verdict`、`--escalation`

工程优化（README Roadmap 已勾选）：
- 第二遍只在 ROI 内检测（≈10× 提速）
- 多标识区域分组（远距离标识各自送模型）
- worker 常驻消除每帧进程启动开销（≈7×）

退码语义：`0` 通过 / `2` 自动校验失败 / `3` 需人工复核 / `1` 执行错误（第三轮复核专门修掉了 `2` 与 clap 用法错误撞码）

### 1.6 模型后端（`tools/model_worker`）
- `dbnet.py`：PP-OCRv4 det（`models/ch_PP-OCRv4_det_infer.onnx` 已随仓库，需 onnxruntime）
- `inpaint_backends.py` `BACKEND_BUILDERS = { e2fgvi, lama, propainter }`
  - `E2fgviInpainter`：Torch 时序视频修复（兼容 `e2fgvi` / `e2fgvi_hq` 不同 forward 签名）
  - `FrameWiseInpainter`：单帧后端基类（LaMa 走此路，加载 TorchScript `big-lama.pt`）
  - `build_external_backend` / `build_torch_propainter`：外部仓库适配（`dossier_backend.py`）
  - `column_fill`：无模型时的中值列填充基线
- `protocol.py`：JSON 请求 + `mask.pgm` / 帧文件目录的工作目录协议

### 1.7 云端原型（`cloud/`）
- 控制面：FastAPI，角色 `viewer < operator < admin`，主体级限流，幂等键
- 队列：SQLite（WAL + `BEGIN IMMEDIATE`）—— 抢占/租约/心跳/指数退避/失联回收
- 状态机：`tasks/state.py` 服务端强校验（API 层不写 `UPDATE tasks.state`）
- 模块白名单（**15 个**，代码字面量，配方改不动）：
  `media.probe`、`video.quality`(C4)、`frame.fps_convert`(C9)、`frame.deflicker`(C9)、`frame.inspect`(C9)、
  `timeline.user_edits`(C5)、`chapters.reorg`(C10)、`commentary.rebuild`(C11)、`audio.quality`(C7)、
  `subtitle.mux`(C3)、`metadata.policy`(C8)、`video.inpaint`(C12)、`video.erase_verify`(C12)、
  `media.encode`(C6)、`media.quality_gate`
- 依赖闭包在**发布时**校验；能力分两层（配方级 + 模块级）+ `capability_probe` 动态探测
- 配方快照冻结进任务；只有 `media.encode` 物化文件（其余模块是纯函数）
- 7 条内置配方：`drama.logo.erase`、`drama.subtitle.burn`、`drama.subtitle.soft`、
  `drama.episodes.split`、`drama.full.edit`、`drama.commentary.draft`、`drama.no.edit.generic`
- Web 客户端 `cloud/src/drama_cloud/web/index.html`：令牌登录/直传/选配方建任务/轮询/下载，
  含「7 · 人工复核队列」面板

### 1.8 擦除确认流（C12，本项目最具差异化的能力）
```text
① 区域/时间配置（配方 regions / start / duration / start_frame / end_frame）
② 检测 + 时序 Mask（fixed 框选 或 worker DBNet 模型；temporal_smooth 相邻帧掩码并集）
③ Inpaint（column 基线 或 worker 时序模型）
④ 自动校验（video.erase_verify 按配方阈值裁决 5 组指标）
   ✅ 通过 → 输出干净视频 + *.inpaint-report.json
   ❌ 失败 → 分支A：自动重试（escalation = mask | mask+backend，最多 3 次，dilate/feather/context 递增）
            → 分支B：review_queue 人工复核
人工复核：claim（认领）→ decide（approve/reject）→ rerun（修正 mask 后新建任务，原记录 superseded）
```
复核状态机：`REVIEW_STATUSES = (pending, approved, rejected, superseded)`；
严重度 `SEVERITIES = (info, warn, high)`；自动失败原因 `REASON_AUTO_FAILED = auto_verify_failed`
**测量与裁决分离**：工具只量指标，是否可交付由配方阈值裁决（改阈值不必重跑擦除）
第三轮复核修掉的关键缺陷：退出码撞码、按 `--help` 探测开关能力（而非硬传参数）、
失败后保留 `inpaint_candidate` 候选产物供人工放行、补 Web 复核入口

### 1.9 质量与测试（仓库文档实测口径）
| 项 | 数量 |
|---|---|
| `cargo test -p dossier-core` | 35 通过 |
| `cargo test -p dossier-inpaint` | 91 + 3 通过（静态统计 `#[test]` = 94） |
| `cd cloud && pytest -q` | 147 通过（静态统计 `def test_` = 130） |
| `python -m unittest discover -s tests`（worker） | 48 通过（1 skip） |
| CI | GitHub Actions `desktop.yml`（fmt / clippy `-D warnings` / test / 打包） |
| 端到端 | `./cloud/demo.sh` 4 条配方全 SUCCEEDED，门禁 9/9 与 10/10 |

### 1.10 未完成项（Roadmap 未勾选）
- `[ ]` 云端 GPU 侧模型类处理（光流插帧、模型修复）—— **硬件阻塞**
- `[ ]` 云端桌面客户端（Tauri 2，需 glib ≥ 2.70）—— **平台阻塞**
- `[ ]` macOS 签名与公证示例 —— 需证书
- `[ ]` 应用内更新通道 —— 需发布签名与网络分发

明确「原型不做」的三项：复核不阻断产物下载、无操作者身份/双人复核、无画面级差分预览。

---

## 第 2 部分 · 功能差异矩阵

图例：✅ 已实现｜⚠️ 部分/仅接线｜❌ 无｜🚫 刻意不做（合规边界）

| # | 功能 | DramaDossier | Grove Max 1.2.4 | 说明 |
|---|---|:--:|:--:|---|
| **素材与工程** |
| 1 | 素材目录扫描 / 集号识别 | ✅ | ✅ | DramaDossier 规则显式、可配置；Grove Max 侧为 `mount_scan_workspace` |
| 2 | 媒体探测（ffprobe） | ✅ 外部 ffprobe | ✅ 自带 "Grove FFmpeg" | 前者不捆绑（许可考量），后者按需下载运行时 |
| 3 | 规格检查（pass/warn/fail/pending） | ✅ 可配置阈值 | ❌ | Grove Max 无「检查报告」概念 |
| 4 | 转码建议（不执行） | ✅ | ⚠️ 直接执行（`transcode_start`） | 语义相反：建议 vs 自动执行 |
| 5 | 报审包差异对比（改稿前后） | ✅ `dossier-core::diff` | ❌ | 差异独有 |
| 6 | 平台材料要求知识库 | ✅ 4 档案 | ❌ | 差异独有 |
| 7 | 剪映草稿互通 | ✅ 导入 + 导出 | ⚠️ 仅生成（`tracks_draft_*`） | Grove Max 生成「分轨剪映草稿」但不读回 |
| **擦除** |
| 8 | 硬字幕擦除 | ✅ DBNet + E2FGVI/LaMa/ProPainter | ✅ 本地 STTN（`subtitle-sttn/infer_model.pth`）+ 云端 | 见 §3.4 实现差异 |
| 9 | 角标 / 水印擦除 | ✅ 同管线 + fixed ROI | ⚠️ `sourceOverlayErasure` 开关（v5 内部） | Grove Max 是去重档位里的一环，不是独立工具 |
| 10 | 字幕区域人工确认 | ⚠️ 命令行/表单填区域 | ✅ 前 4 集区域确认 UI | Grove Max UI 更成熟 |
| 11 | 字幕带自动识别 | ⚠️ DBNet 检测框 | ✅ `caption_bands` + OCR | Grove Max 有 OCR 模型目录与 `caption_scan` |
| 12 | **擦除结果自动校验（确认流核心）** | ✅ 5 组指标 + 双阈值 | ❌ **无** | **DramaDossier 独有** |
| 13 | 自动重试（加力 / 换后端） | ✅ `escalation`，最多 3 次 | ❌ | **独有** |
| 14 | 人工复核队列 | ✅ `review_queue` + 状态机 + 审计 | ❌ | **独有** |
| 15 | 修正 mask 后重跑 | ✅ `review-rerun` → 新任务 + `superseded` | ❌ | **独有** |
| 16 | 擦除处理日志 | ✅ `*.inpaint-report.json` | ⚠️ `inpaint_report` 类产物 | 两者都有报告概念 |
| 17 | 无损中间态 / 两段式编码 | ✅ `delivery` / `lossless`(FFV1) | ⚠️ 未观察到对应概念 | DramaDossier 有量化实验 |
| **成片处理** |
| 18 | 分辨率/滤镜/去闪/变速 | ⚠️ 云端模块（C4/C9） | ✅ v5 管线 | 覆盖度接近 |
| 19 | 分集重组 | ✅ `chapters.reorg` | ✅ 镜头边界模型 + 分集时长 | Grove Max 用 **shot-boundary 模型**自动定切点 |
| 20 | 解说重剪 | ⚠️ 候选时间线，**无 ASR** | ✅ `narrationRecut` + Demucs 音源分离 | Grove Max 有 ASR + Demucs |
| 21 | **去重对抗（画面/编码指纹）** | 🚫 | ✅ `DedupRecipe` 服务端下发 | 合规边界 |
| 22 | 音频处理 | ✅ loudnorm/EQ/压缩/响度测量 | ✅ 音频去重 + Rubber Band 变速变调 | 目标不同：合规 vs 对抗 |
| 23 | 硬件编码 | ❌ 仅 libx264 | ✅ `h264_nvenc` / `h264_videotoolbox` + CBR | 差异明显 |
| **材料产出** |
| 24 | 报审材料包生成 | ✅ | ✅ `report_package_*`（23 个命令） | 两者都做 |
| 25 | 封面 / 海报 | ✅ | ✅ | — |
| 26 | 成本报表 | ✅ 结构化表 + 用户填写 | ✅ **渲染成图片**（`dist/cost-report-cli.js`） | Grove Max 额外做可视化渲染 |
| 27 | 电子签名 | 🚫 明确不伪造 | ✅ `signature_pad` | **合规边界相反** |
| 28 | 电子公章 | 🚫 明确不伪造 | ✅ drawstamputils | **合规边界相反** |
| 29 | AI 内容标识水印 | ⚠️ 仅勾选「使用了 AI 辅助生成」并记录 | ✅ `ai_label_preview` 渲染到画面 | 目的不同：声明 vs 上标 |
| 30 | 剧名水印渲染 | ❌ | ✅ `sidecar/title_watermark.py` | — |
| 31 | 剧名/简介/封面 AI 生成 | ❌ | ✅ `rewrite_copy` / `reverse_prompt` / `scene_text` | — |
| **分发与商业** |
| 32 | **自动上传到视频号** | 🚫 | ✅ Playwright + Chromium + 持久会话 | 合规边界 |
| 33 | 发布结果确认 | ❌ | ✅ `mount_resolve_publish_result` | — |
| 34 | 账号 / 登录 / 令牌 | ❌ | ✅ 邮箱注册 + 验证码 + 会话 | — |
| 35 | 卡密 / 设备绑定 | ❌ | ✅ keyring + SMBIOS 指纹 | — |
| 36 | 积分 / 报价 / 结算 | ❌ | ✅ `quote → task → settle` | — |
| 37 | 在线支付 | ❌ | ✅ Z-Pay（`zpayz.cn` / `z-pay.cn`）+ 线下充值 | — |
| 38 | 邀请拉新 | ❌ | ✅ `/invitations/*` | — |
| 39 | 自动更新（签名下发） | ❌（Roadmap 未完成） | ✅ minisign + 腾讯云 COS | — |
| **运维与工程** |
| 40 | 本地运行时按需下发 | 🚫 靠用户自备 ffprobe / 手工装 torch | ✅ 22 个 RuntimeRoles + 校验 + 回滚 | 体验差异明显 |
| 41 | GPU 加速组件管理 | ❌ | ✅ `torch-cuda` + `nvidia-smi` 探测 + `--cpu-only` 回退 | — |
| 42 | 单测 / CI / 门禁 | ✅ 35+94+147+48 + GH Actions | ❌ 不可见（闭源） | **可测试性差异巨大** |
| 43 | 开源可审计 | ✅ MIT | ❌ | — |
| 44 | 人工复核面板（Web） | ✅ | ❌ | — |
| 45 | 遥测 / 素材上传 | 🚫 明确无 | ✅ 账号 + 云端任务 | 数据流向相反 |

---

## 第 3 部分 · 技术实现差异

### 3.1 元框架与前端

| 维度 | DramaDossier | Grove Max 1.2.4 |
|---|---|---|
| 桌面框架 | Tauri 2（`desktop.yml` 构建 NSIS/.dmg） | Tauri **2.11.5**（+ wry 0.55.1 / tao 0.35.3） |
| 前端 | **原生 HTML/CSS/JS**（1,448 行，无框架） | **React + Vite + Tailwind v4**（bundle 1.49 MB 明文） |
| UI 组件 | 手写 | React Bits / Recharts / ky / signature_pad |
| 主包体积 | 小（无前端框架） | 40.8 MB exe，内嵌 236 个资源（含 3.2 MB 与 1.4 MB 引导视频、120+ 分片字体） |
| 资源打包 | Tauri 默认（读取 `src/`） | Tauri 资源表 + **逐资源 brotli 压缩**内嵌进 `.rdata` |
| IPC 命令数 | **16**（面窄、职责清晰） | **~100 应用命令 + 92 插件命令** |
| 状态管理 | 原生 DOM 状态 | 自研 hooks（无 zustand/redux/tanstack-query） |
| 国际化 | 中文 | 中文（`lang="zh-CN"`，硬编码） |

### 3.2 后端与本地运行时

| 维度 | DramaDossier | Grove Max |
|---|---|---|
| 业务逻辑位置 | `dossier-core` / `dossier-inpaint` 库，命令层仅 438 行 | `src/lib.rs`、`src/pipeline_v5.rs`、`src/mount.rs`、`src/media_sidecar.rs` |
| 外部工具来源 | 用户自备 `ffprobe`；模型权重 `fetch_models.sh` 自行下载 | **22 个运行时组件按需下载**：node / ffmpeg / ffprobe / python×4 / ocr 模型 / demucs / rubberband / libreoffice / **chromium** / torch overlay / 各 runner |
| 运行时完整性 | 无清单机制 | `runtime-manifest.json` + `RUNTIME_ROOT_INVALID` 校验 + `runtime-rollback-marker.json` 回滚 |
| 语言栈 | Rust + Python（worker，stdio 行协议） | Rust + **Node.js**（`dist/*.js` CLI）+ Python（sidecar）+ C 工具 |
| 加速 | ❌（本机无 GPU/torch/onnxruntime） | `torch-cuda` 组件 + `nvidia-smi --query-gpu=...` 探测 + `--cpu-only` 回退 |
| 硬件编码 | ❌ libx264 软编 | `h264_nvenc`（Win）/ `h264_videotoolbox`（macOS）+ CBR |

**关键工程差异**：DramaDossier 把外部依赖当「用户环境前提」；Grove Max 把外部依赖当
「产品的一部分」（自建分发 + 校验 + 回滚 + GPU 探测）。这是**学习原型**与**商业产品**的分野。

### 3.3 云端架构

| 维度 | DramaDossier `cloud/` | Grove Max 云端 |
|---|---|---|
| 语言/框架 | Python / FastAPI | 未知（仅见客户端契约） |
| 队列 | SQLite 持久化队列（租约 + 心跳 + 指数退避 + 失联回收） | 不适用（客户端本地执行 + 服务端签发 quote/recipe） |
| 状态机 | `tasks/state.py` 单一实现点，服务端强校验 | 客户端 `generation_workflow_*` + `/generation-tasks/{id}/settle` |
| 配方 | `recipes/registry.py` 代码白名单（15 模块）+ 依赖闭包校验 + 能力探测 | `DedupRecipe` 等 **20+ 结构体**，服务端通过 `x-grove-dedup-profile: regular-reference-v2` 下发 |
| 配方冻结 | 快照冻结进任务（发新版不影响在跑任务） | `videoPipelineVersion: v5` 与 pending quote 一致性校验 |
| 存储 | 本地 FS，对象键不透明，短时 HMAC 令牌 | 未观察 |
| 认证 | Bearer 令牌 + 角色（viewer/operator/admin）+ 幂等键 + 限流 | 邮箱注册 + 验证码 + 会话 + 卡密 + 设备指纹 |
| 计费 | ❌ | ✅ quote / 积分 / ledger / 日用量 / 充值档位 |
| 人工复核 | ✅ `review_queue` + 认领/裁决/重跑 + 审计事件 | ❌ |
| 质量门禁 | ✅ `media.quality_gate`（可解码/音画同步/目标规格/时长） | ⚠️ 发布结果人工确认（`mount_resolve_publish_result`），无质量指标门禁 |

### 3.4 擦除管线实现对照（最值得互看的一处）

| 维度 | DramaDossier | Grove Max |
|---|---|---|
| 检测模型 | **DBNet**（PP-OCRv4 det，ONNX） | OCR 模型目录 + `caption_scan`（模型名未确认） |
| 修复模型 | **E2FGVI**（主）/ **LaMa**（单帧）/ **ProPainter**（外部适配）/ `column` 基线 | **`subtitle-sttn/infer_model.pth`**（目录名 `subtitle-sttn`，STTN 名称来自 `preferSttn` 标志；**网络结构未做权重级验证**） |
| 时序策略 | 光流引导（E2FGVI 1/4 分辨率传播）+ `neighbor_stride/num_ref` + `--clip-frames` ≥ 2×num_ref | 未在本次样本中观察到具体时序策略 |
| 掩码策略 | `--dilate` / `--feather`，有**量化实验表**（`dilate ≥ feather` 规则） | v5 内部开关，无公开参数 |
| 时间窗 | `--start/--duration`、`start_frame/end_frame` | 前 4 集区域确认 |
| 云端分支 | ✅ `/cloud-subtitle-jobs/availability` + Node 分析 CLI（OCR，`--ocr-threads`） | 客户端 `processingMode: 'cloud'` + `/cloud-subtitle-jobs/*` |
| **质量校验** | ✅ 残留率 / 闪烁 / 接缝 / PSNR(+参考帧) / SSIM，含 `warn`/`fail` 双阈值 | ❌ 无（只有「字幕处理方式与待处理报价一致性」这类契约校验） |
| 自动重试 | ✅ `escalation = mask | mask+backend`，3 次递增加力 | ❌ |
| 人工复核 | ✅ 队列 + 状态机 + 重跑 | ❌ |
| 报告产物 | `*.inpaint-report.json` | `inpaint_report` 类产物 |

**结论**：两者擦除的「算法接线」层次接近，DramaDossier 多的是**确认流**（第 4 步及其两个分支），
Grove Max 多的是**运行时工程化**（模型/组件自动下发 + GPU 探测）与 **UI 成熟度**。

### 3.5 授权、凭据与信任模型

| 维度 | DramaDossier | Grove Max |
|---|---|---|
| 授权 | 无（MIT 开源） | 卡密 + 设备指纹 + 服务端配方强控 |
| 凭据存储 | 无 | `keyring-core` + Windows 凭据管理器 |
| 设备指纹 | 无 | SMBIOS + 注册表 |
| 网络信任 | 无外联（除用户自建云端） | rustls + platform-verifier；`GROVE_MAX_SESSION_TOKEN` / `TASK_TOKEN` |
| 更新信任 | 无应用内更新 | `minisign-verify` Ed25519 签名校验 |
| 服务端控制力 | 配方由本地仓库提供，可自审 | 配方/报价/授权全部服务端签发，客户端仅执行器 |

---

## 第 4 部分 · 「原片标识与声明擦除确认流」对照（回答历史提问）

完整业务 Pipeline 六步在两边的落点：

| 步骤 | DramaDossier | Grove Max 1.2.4 |
|---|---|---|
| ① 任务编排 & 区域/时间配置 | ✅ 云端配方 `regions/start/duration`；桌面表单 | ✅ 前 4 集区域确认 UI + pending quote |
| ② 标识检测 + 时序 Mask | ✅ DBNet worker / fixed ROI / `temporal_smooth` | ✅ OCR + caption band + STTN 前置 |
| ③ 视频 Inpaint | ✅ E2FGVI / LaMa / ProPainter / column | ✅ STTN（本地）/ 云端 |
| ④ **擦除结果自动校验** | ✅ 5 指标 + `video.erase_verify` 裁决 | ❌ **缺失** |
| ✅ 通过 → 输出 + 处理日志 | ✅ `*.inpaint-report.json` | ⚠️ 仅有处理进度事件，无质量日志结论 |
| ❌ 分支A：自动重试 | ✅ `escalation` 3 次 | ❌ |
| ❌ 分支B：人工复核队列 | ✅ `review_queue` + Web 面板 | ❌ |
| 人工复核 → 通过/失败/修正重跑 | ✅ `claim/decide/rerun` + `superseded` + 审计 | ❌ |

**Grove Max 真正缺的就是第 4 步及其两个分支**；它的「确认」只发生在**发布结果**层面
（人工确认视频号是否已发表），而不是**处理质量**层面。

---

## 第 5 部分 · 双方各自的能力缺口（供后续推进参考）

### 5.1 DramaDossier 相对 Grove Max 缺的（按「是否值得补」排序）

| 优先级 | 缺口 | 是否合规 | 建议 |
|---|---|---|---|
| P1 | **本地运行时管理**（组件清单/校验/回滚/版本） | ✅ 合规 | 值得补：把 ffprobe/torch/ONNX 权重做成「可探测 + 可校验 + 可回滚」的运行时清单，比现在的 `fetch_models.sh` 体验好得多 |
| P1 | **GPU/加速组件探测与回退** | ✅ 合规 | 值得补：`nvidia-smi` 探测 + CPU 回退 + 并行度自动/手动/上限（Grove Max 的 `video_processing_capabilities` 结构可直接借鉴） |
| P2 | **硬件编码（NVENC / VideoToolbox）** | ✅ 合规 | 值得补：纯性能收益 |
| P2 | **ASR 转写** | ✅ 合规 | C11 解说重剪目前需外部提供时间戳 |
| P2 | **模型端到端验证**（权重 + onnxruntime/torch） | ✅ 合规 | 当前 E2FGVI/LaMa 仅接线，本机无 GPU 无法验证 |
| P3 | 字幕带自动识别 + OCR | ✅ 合规 | 现为 DBNet 检测框，无 OCR 文本 |
| P3 | 应用内更新通道 | ✅ 合规 | Roadmap 未完成，需签名与分发 |
| P3 | 桌面端擦除 UI 成熟度（前 N 集区域确认） | ✅ 合规 | 现有表单够用 |
| P3 | 画面级差分预览（复核者并排对比） | ✅ 合规 | 已列为「原型不做」 |
| 🚫 | 去重对抗 / 自动分发 / 卡密积分 / 伪造签章 | ❌ | **不做**，与项目边界冲突 |

### 5.2 Grove Max 相对 DramaDossier 缺的
1. **擦除质量自动校验**（残留/闪烁/接缝/PSNR/SSIM）——完全没有
2. **自动重试的加力策略**（`escalation` 概念）
3. **人工复核队列与状态机**（`pending/approved/rejected/superseded`）
4. **可解释的检查报告**（pass/warn/fail/pending + 用户可配阈值）
5. **报审包差异对比**
6. **剪映草稿读回**（只有导出）
7. **测试 / CI / 开源可审计**
8. **两段式无损中间态与色度实验结论**

---

## 第 6 部分 · 总体判断

**定位差异**：DramaDossier 是「**可复核的合规材料工程工具**」，Grove Max 是「**对抗性内容再分发套件**」。
两者的「视频处理」技术栈确实高度重合（都是 Tauri 2 + ffmpeg + Torch 类模型 + 桌面壳），
但**工程重心完全不同**：

- DramaDossier 的护城河在 **④ 确认流 + 审计 + 可测试**；
- Grove Max 的护城河在 **服务端强控配方 + 运行时自动下发 + 分发闭环 + 商业化**。

**技术互看价值（合规范围内）**：
1. Grove Max 的 **`video_processing_capabilities` 结构**（硬件/编码器/并行度三档）值得作为
   DramaDossier 运行时探测的接口设计参考；
2. Grove Max 的 **22 组件运行时清单 + 校验 + 回滚**是「模型/工具链交付」的成熟做法，
   可直接用于解决 DramaDossier「模型权重 + onnxruntime/torch 靠用户手工准备」的体验缺口；
3. 反向地，DramaDossier 的 **测量与裁决分离 + escalation + review_queue** 是 Grove Max 完全没有的
   质量闭环，属于**更严谨的工程实践**。

**合规结论**：两者不可混用其能力。DramaDossier 明确不做去重对抗、不做自动分发、
不伪造签章；Grove Max 的免责条款（「因违法或未经授权使用产生的后果由使用者自行承担」）
是风险转移，不能作为其行为正当性依据