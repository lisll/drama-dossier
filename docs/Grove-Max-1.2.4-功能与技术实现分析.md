# Grove Max 1.2.4 (x64) 功能模块与技术实现分析

> 用途声明：本文档仅用于**技术学习与安全分析**，基于公开可得安装包的静态逆向。
> 不包含破解、补丁、绕过授权或规避检测的可操作方案，也不用于任何第三方作品的版权标识处理。
> 所有结论均标注证据来源；未验证项明确标为「未验证/未知」，不作推断性事实陈述。

- 分析日期：2026-10-14
- 样本：`/root/.codex/tmp/Grove-Max_1.2.4_x64-setup.exe`
- 安装包 SHA-256：`747dae2f9e344933ba8129176d6295092960a604263cd64440765c16965d6012`
- 主程序 SHA-256：`d4f21731db38d367976b15a57192587ecf15ad07d317d46188a1b39cf8d9f9a7`
- 分析方法：NSIS 解包 → PE 结构解析 → Rust 符号/panic 路径提取 → Tauri 内嵌资源表解密 → 前端 bundle 静态分析
- 环境限制：x86-64 目标、本机 aarch64，仅静态分析；未执行样本、未联网调用其服务

---

## 一、证据链与方法（可复现）

| 步骤 | 命令/方法 | 产出 |
|---|---|---|
| 1. 解包 | `7zz x -y -o/tmp/gm/nsis Grove-Max_1.2.4_x64-setup.exe` | NSIS-3 Unicode / LZMA 固实包，9 个文件 |
| 2. 载荷识别 | `file` + PE 头解析 | `grove-max-review.exe`，PE32+ x86-64 GUI，40,829,440 B |
| 3. 依赖指纹 | 提取 `index.crates.io-*` 路径 + Rust panics | 106 个 crate 及版本 |
| 4. 资源还原 | 在 `.rdata` 定位 Tauri 资源表（32B 记录：key_ptr/key_len/data_ptr/data_len，恒满足 `data_ptr = key_ptr + key_len`） | 236 条资源记录、14,148,349 B |
| 5. 资源解码 | 每条记录即一个 **brotli 流**，`brotli -d` | 236/236 全部解码成功（16 MB 明文） |
| 6. 前端分析 | 解码后 `assets/index-ChO9pxGD.js`（1,494,375 B 明文） | 命令面 / API 面 / 枚举 / UI 文案 |

**关键点**：Tauri 2 的内嵌资源在磁盘上以 `(key, brotli(payload))` 形式紧邻存放，key 是明文 ASCII。
这使前端 bundle 可完整还原——上一版分析中「前端 JS 未还原」的限制在本轮已解除。

---

## 二、应用形态与技术栈

### 2.1 打包与标识
- 框架：**Tauri 2.11.5**（`tauri-2.11.5`、`tauri-runtime-wry-2.11.4`、`wry-0.55.1`、`tao-0.35.3`）
- 前端：**React + Vite** 单页应用，`index.html` 挂载 `#root`
- 样式：**Tailwind CSS v4**（`oklch()`、`--tw-*`、`@layer`），Inter Variable + Noto Serif SC 字体
- 图表：**Recharts**；HTTP 客户端：**ky**；状态管理为自研 hooks（无 zustand/redux/tanstack-query）
- 应用标识：Tauri id `app.raycaster.grove-max`；Windows bundle id `cn.grovemax.review`；窗口标题 `Grove Max`
- CSP 声明（`src\lib.rs`）允许 `https://api.grove-max.app` 与 `https://api.raycaster.app`、`http://localhost:3001`

### 2.2 运行时安全策略
- TLS：`rustls-0.23.43` + `rustls-platform-verifier-0.7.0` + `ring-0.17.14`（非 OpenSSL）
- 自动更新：`tauri-plugin-updater-2.10.1` + **`minisign-verify-0.2.5`**（Ed25519 签名校验）
  - 更新源：`https://grove-max-downloads-1301443848.cos.ap-guangzhou.myqcloud.com/grove-max-app/updates/app/internal-beta/`
    （腾讯云 COS 广州桶，`internal-beta` 通道）
- 凭据存储：**`keyring-core` + `windows-native-keyring-store`** → 会话 Token / 卡密进 Windows 凭据管理器
- 设备指纹：`smbios-lib-0.9.4` + `winreg/ windows-registry`（读 SMBIOS 与注册表）
- 解压/归档：`zip-4.6.1`、`tar-0.4.46`、`flate2`、`brotli-decompressor`
- Office 文档互操作：`cfb-0.7.3`（OLE 复合文档，配合 LibreOffice 产出报表）

### 2.3 后端 Rust 模块（源文件路径泄漏）
`src\lib.rs`、`src\mount.rs`、`src\client_errors.rs`、`src\pipeline_v5.rs`、`src\media_sidecar.rs`
（自有源码路径未泄漏，release 已 strip；仅有 cargo registry 路径与 `R:\Sg`、`D:\O`、`E:\qr.r` 等残余）

---

## 三、本地运行时（RuntimeRoles：22 类组件）

Rust 侧定义 `RuntimeRoles` 共 **22 个元素**，构成可下载 / 可校验 / 可更新的本地运行时：

| 角色 | 作用（依据） |
|---|---|
| `engineRoot` | 引擎根目录 |
| `node` | Node.js 运行时（多个 sidecar 是 `dist/*.js`） |
| `ffmpeg` / `ffprobe` | 编解码与探测（品牌化为 "Grove FFmpeg"） |
| `analysisSidecar` / `analysisPython` | 媒体分析 sidecar（OCR / 画面分析） |
| `ocrModelDirectory` | OCR 模型目录 |
| `mediaSidecar` | 媒体分析协议（`MEDIA_SIDECAR_*`、`MEDIA_ANALYSIS_*`） |
| `shotBoundaryModel` | **镜头边界检测模型**（分集切点依据） |
| `subtitleRemovalModel` | **字幕擦除模型**：`subtitle-sttn/infer_model.pth` |
| `torchOverlay` | **PyTorch 覆盖层**（`GROVE_PLUS_TORCH_OVERLAY`） |
| `speechRecognitionRoot` | 语音识别（`transnetv2` / `speech-recognition` 目录名出现） |
| `demucsModelDirectory` | **Demucs 音源分离**（人声/伴奏） |
| `audioPython` / `audioSeparatorSidecar` / `audioSeparatorRunner` | 音频分离执行链 |
| `rubberband` | 变速/变调 CLI（`pitchSemitones`、`audioTempoCompensation`） |
| `jianyingPython` | 剪映草稿 Python 环境（`pyJianYingDraft`） |
| `draftBuilderScript` | 剪映草稿构建脚本 |
| `chromium` | **Chromium（Playwright）**，用于浏览器自动化发布 |
| `libreoffice` | 文档转换/渲染 |
| `reportPackageRunner` | 报审材料包生成器 |
| `mountRunner` | 搬运/发布执行器 |

### 3.1 GPU 加速组件（`accelerator_*`）
- 组件名 `torch-cuda`；通过 `nvidia-smi.exe` 探测：
  `--query-gpu=name,memory.total,driver_version,compute_cap --format=csv,noheader,nounits`
- 探测字段：`driverVersion` / `vramMiB` / `computeCap` / `platform` / `gpu` / `eligibility` / `requiredDriver`
- 支持 `--cpu-only` 回退；引导页给出 `https://www.nvidia.cn/drivers/`
- 命令：`accelerator_status`、`accelerator_install_component`、`accelerator_cancel_install`、`accelerator_remove_component`
- 安装状态机：`checking` → `downloading`（`receivedBytes`/`totalBytes`）→ `failed`/就绪；有 `runtime-rollback-marker.json` 回滚标记

### 3.2 运行时生命周期
`report_package_probe_runtime` / `install_runtime` / `runtime_check_progress` / `check_runtime_update` / `runtime_update_status`
→ 运行时**按需下发、增量更新**，错误码含 `RUNTIME_ROOT_MISSING`、`RUNTIME_BUSY`、`RUNTIME_MANIFEST_INVALID`、`RUNTIME_INSTALL_IN_PROGRESS`

---

## 四、功能模块清单

### 4.1 命令面总览
前端 bundle 中出现 **68 个应用命令 + 92 个 `plugin:*` 命令**；结合 Rust 注册表（命令列表字符串）
可得完整 IPC 面。按业务域归类：

#### A. 视频处理管线（核心）
| 命令 | 功能 |
|---|---|
| `video_processing_capabilities` | 返回 `hardwareAvailable`/`encoder`/`gpuName`/`gpuMemoryMb`/`hardwareCbrAvailable`/`automaticParallelism`/`configuredParallelism`/`effectiveParallelism`/`maxParallelism` |
| `transcode_inspect` / `transcode_start` / `transcode_stop` | 转码检视、启动、停止（`normalizeMismatch`） |
| `subtitle_removal_preflight` | 字幕擦除预检，参数 `preferSttn`（布尔）与 `processingMode`（`local`/`cloud`） |
| `tracks_draft_start/pause/status/results/preview/inspect` | 分轨 → 剪映草稿生成全流程 |
| `toolbox_inspect_mp4_directory` / `toolbox_rename_mp4_files` | MP4 目录检查 / 按集数批量重命名（`剧名 - 第几集`） |
| `toolbox_inspect_bitrate_directory` / `toolbox_repair_bitrates` / `toolbox_stop_bitrate_repair` | 码率检查与「码率修复」 |
| `toolbox_inspect_encoding_variation_directory` / `toolbox_run_encoding_variation` / `toolbox_stop_encoding_variation` | 编码参数变体扫描与批量重编码 |

#### B. 报审材料包（对外包装主线）
`report_package_*` 共 23 个命令：`preflight`/`inspect`/`scan_library`/`read_cover`/`active_run`/`abandon_run`/`stop`/
`delete_project`/`delete_pending_quote`/`write_pending_quote`/`read_pending_quote`/`relay_url`/
`detect_jianying_app`/`discover_jianying_roots`/`validate_jianying_root`/`install_jianying`/
`capture_jianying_screenshots`/`prepare_pending_ai_label`/`install_runtime`/`probe_runtime`/…
产物清单（字符串证据）：`production_script`、`production_episode_manifest`、`production_asset_manifest`、
`production_asset_profile`、`production_asset_image`、`final-draft-manifest.json`、
`clean-sources-manifest.json`、`caption-overlays-manifest.json`

#### C. 生成工作流与计费
`generation_workflow_preflight` / `for_task` / `active` / `recoverable` / `retry_video` /
`stop_video` / `abandon_video` / `validate_cost_report`
事件：`run_started/completed/failed`、`stage_started/completed`、`video_step_started/progress/completed`、
`model_request_started`、`media_analysis_progress`、`artifacts_published`、`cost_report_started/completed/failed`
→ 服务端签发任务（`quoteId`）、客户端执行、按阶段上报。

#### D. 成本报表与签章
`cost_report_read_image` / `cost_report_save_signature` / `cost_report_*` 事件
- 渲染器：`dist/cost-report-cli.js`（Node）+ `cost-report-input.json` → `cost-report.png`
- 素材：`cost-report-assets`、`cost-report-signature`、`cost-report-seal.*`（支持上传或内置印章）
- 公章使用 **drawstamputils**（`DrawStampUtils`，固定 commit `dd3144d5…`，Apache-2.0）
- 手写签名使用 **signature_pad 5.1.4**
- 输入项：公司名称、投资额、演员片酬占比（万元单位）；校验 `发票/签名/印章` 齐全后才允许产出
- 免责文案（前端硬编码）：
  - 「请确保公司名称、印章编码及签名真实且已获授权，严禁伪造、冒用或用于违法用途。」
  - 「因违法或未经授权使用产生的后果及责任，由使用者自行承担，与本软件及开发者无关。」
  - 「上述信息与图片仅限用于通过本软件生成漫剧平台报审材料。」

#### E. AI 提示标识（合规声明水印）
`ai_label_preview` + `report_package_prepare_pending_ai_label`
- 渲染器：`sidecar/ai_label.py` + `@grove-max/engine-render` 字体 `NotoSansCJKsc-Bold.otf`
- 默认文案：`内容由AI生成·纯属虚构·请勿代入现实`（≤ 40 字符）
- 位置枚举：`topLeft` / `bottomLeft` / `bottomRight`（左上/左下/右下）
- 排布枚举：`vertical`(单列竖排) / `singleLine` / `balanced` / `compact`
- 字号：10–24 整数；分 `portrait` / `landscape` 两套呈现
- 约束：「添加 AI 声明需要深度处理」——即 AI 标识仅在高档位管线中提供

#### F. 搬运 / 自动发布（`mount_*`，17 个命令）
`mount_preflight` / `mount_scan_workspace` / `mount_inspect_directory` / `mount_list_plans` /
`mount_save_plan` / `mount_refresh_plan` / `mount_delete_plan` / `mount_open_plan_csv` /
`mount_prepare_copy_generation_input` / `mount_write_generated_copy` / `mount_start_upload` /
`mount_pause_upload` / `mount_resolve_publish_result` / `mount_open_channels_weixin` /
`mount_logout_channels_weixin` / `mount_channels_weixin_status`
- 目标站点：`https://channels.weixin.qq.com/`（微信视频号）；profile 名 `channels-weixin`
- 凭据：`channels-weixin-session.json`（持久化登录态）+ `playwrightVersion` 字段
- 记录结构：`MountSessionRecord`、`MountPlanStore`、`StoredMountPlan`、`StoredUploadedMaterial`、
  `MountUploadProgress`、`MountRunnerSelfCheck`、`MountRunnerOpenResponse`
- 发布字段：`planId` / `currentIndex` / `completedCount` / `totalCount` / `materialFileName` /
  `loggedOut` / `loggedIn` / `confirmedAtMs` / `browserOpen` / `browserName` / `browserVersion` / `checks`
- 上传节流：上传间隔 3–60 秒
- 错误码族：`MOUNT_BROWSER_LAUNCH_FAILED`、`MOUNT_SERIES_SEARCH_TIMEOUT`、`MOUNT_SERIES_NOT_FOUND`、
  `MOUNT_SERIES_AMBIGUOUS`、`MOUNT_PROFILE_DIRECTORY_UNAVAILABLE`、`MOUNT_CHROMIUM_*`、
  `MOUNT_H264_UNSUPPORTED`、`MOUNT_RUNNER_RESPONSE_INVALID`
- 相关：`mount_copy_v1`、`mount-copy/quote`、`mount-copy/generations`（AI 文案）

#### G. AI 文案生成
`rewrite_copy` / `reverse_prompt` / `scene_text` / `text_reconstruction` / `ai_proof`
→ 反推提示词、改写文案、场景文本、文本重建；结果写回 `挂载文案.csv`（含 `expectedCsvSha256` 校验）

#### H. 工具箱
MP4 重命名、码率修复、编码变体、目录检查——均依赖本地运行时（未安装时提示先激活有效卡密）

### 4.2 处理档位枚举（前端定义，权威）
```js
AY = { none: '保持原片', full: '常规处理', subtitle: '深度处理' }
DY = {
  none:     { recompose:false, microFrames:false, audioDedup:false },
  full:     { recompose:true,  microFrames:true,  audioDedup:true, processingLevel:'full' },
  subtitle: { recompose:true,  microFrames:true,  audioDedup:true,
              videoPipelineVersion:'v5', processingLevel:'subtitle' }
}
```
- 管线版本映射：`full → v1`、`subtitle → v4`，最新为 **`v5`**（`v5-local-ffmpeg-scenes/1` profile）
- v5 内部开关：`subtitleErasure`、`sourceOverlayErasure`、`subtitleRestoration`、
  `subtitleReconstruction`、`narrationRecut`、`microFrames`、`audioDedup`、`recompose`
- 字幕处理方式：`subtitleProcessingMode ∈ {local, cloud}`；`preferSttn` 控制本地 STTN 模型优先
- 服务端错误码会触发文案改写：代码中存在 `去重 → 处理`、`请选择常规处理 → 请选择保持原片`
  的字面替换函数（错误信息层做了术语替换）

### 4.3 服务端下发的「配方」（Recipe）结构
Rust 侧结构体族（`struct RecipeX with N elements` 证据）：
`DedupRecipe`、`RecipeAudio`、`RecipePicture`、`RecipeCrop`、`RecipeCropFraming`、`RecipeSize`、
`RecipeGrade`(contrast/brightness/saturation/hueDegrees)、`RecipeGrain`(strength)、`RecipeScene`(threshold)、
`RecipeCamer`、`RecipeEqBand`、`RecipeJitter`、`RecipeSharpen`(amount)、`RecipeAbBlend`(opacity, offsetSeconds)、
`RecipeTemporalBlend`(previousWeight)、`RecipeMicroFrames`、`RecipeBitrat`(targetBps, bufferBps)、
`RecipeOutput`、`RecipeEncoding`、`RecipeTimeline`、`RecipeFrameRate`、`RecipeMetadata`、`RecipeCompressor`
其他字段：`ruleSet`、`quoteId`、`seed`、`transition`、`bFrames`、`keyframeInterval`、`faststart`、
`camerShake`、`headTrimSeconds`/`tailTrimSeconds`、`minEpisodeSeconds`/`maxEpisodeSeconds`、
`pitchSemitones`、`asetrateRatio`、`audioTempoCompensation`、`sceneCutThreshold`、`referenceFrames`、
`boxes`、`standardSizes`、`rotationWave`
- 由服务端通过请求头 `x-grove-dedup-profile: regular-reference-v2`、`x-grove-processing-policy`、
  `tiers-v2`、`x-grove-max-client` 参与下发/协商
- 客户端强制校验：`RECIPE_REQUIRED` / `RECIPE_INVALID` / `RECIPE_EXPIRED` / `RECIPE_UNAVAILABLE` /
  `RECIPE_TASK_NOT_CHARGED` → 配方必须由服务端签发

### 4.4 字幕擦除（与本次对比最相关）
- 本地：`subtitle_removal_preflight{preferSttn, processingMode:'local'}` → `GROVE_PLUS_SUBTITLE_REMOVAL_PYTHON`
  + `GROVE_PLUS_SUBTITLE_REMOVAL_MODEL` → `subtitle-sttn/infer_model.pth`
  ~~模型为 **STTN**（Spatial-Temporal Transformer Network，`infer_model.pth`）~~
  **修正**：`infer_model.pth` 是 PyTorch 权重文件名，STTN 名称来自 `preferSttn` 标志与 `subtitle-sttn` 目录；
  具体网络结构未做权重级验证，标为**待确认**
- sidecar：`sidecar/subtitle_removal/remove.py`、`sidecar/subtitle_removal/finalize.py`
- 错误码：`V2_SUBTITLE_REMOVAL_FAILED`、`V2_SOURCE_DECODE_FAILED`、`SUBTITLE_STTN_MODEL_MISSING`
- 云端：`/cloud-subtitle-jobs/availability`；分析 CLI `dist/cloud-subtitle-analyze-cli.js`
  （`--source --sequence --output --analysis-sidecar-path --analysis-python-path --ocr-model-directory
  --ffmpeg-path --ffprobe-path --ocr-threads --reuse-analysis`）→ `cloud-subtitle-manifest.json`
- 字幕区域：前端有「字幕区域与擦除预览」「必须确认原片第 1–4 集的字幕区域」
- 字幕轨：`caption_bands`、`caption_recognition`、`caption_scan`、`CAPTION_BAND_*` 错误码；
  `TRACKS_DRAFT_VIEW_NO_CAPTIONS`、`TRACKS_PREVIEW_NE_REMOVAL_FAILED`

### 4.5 分轨 / 剪映草稿
- 命令：`tracks_draft_start/pause/status/preview/inspect/results`
- 产物：`final-draft-manifest.json`、`clean-sources-manifest.json`、`caption-overlays-manifest.json`
- 依赖：`pyJianYingDraft`（Python）、剪映安装目录探测与校验（`grove-max-jianying-target-v1`）
- 错误族：`JIANYING_DRAFT_*`（TIMEOUT、PATH_INVALID、ROOT_NOT_WRITABLE、ROOT_NOT_FOUND、
  PROJECT_NOT_FOUND、SOURCE_CHANGED、MEDIA_INDEX_CONFLICT…）、`TRACKS_*`（OCR_FAILED、
  ANALYSIS_PROTOCOL_INVALID、AUTO_REGION_UNAVAILABLE、REQUEST_REQUIRED…）
- 前端会话标题：`tracks-draft-heading`、`tracks-erasure-heading`、`tracks-task-heading`

### 4.6 标题水印渲染
`sidecar/title_watermark.py` + `@grove-max/engine-render` 的 `NotoSansCJKsc-Bold.otf`
→ `TITLE_WATERMARK_RENDER_FAILED`；前端资产 `title-watermark-preview-*.png`
（UI 提示：「右上角预留给剧名水印」）

---

## 五、云端 API 面（前端静态提取）

Base：`https://api.grove-max.app`（配置项 `modelRelayUrl`，schemaVersion 2）；另有 `https://api.raycaster.app`

| 域 | 端点 |
|---|---|
| 认证 | `POST /auth/register`、`/auth/login`、`/auth/logout`、`/auth/session`、`/auth/password`、`/auth/password-reset`、`/auth/verification-codes`、`/auth/redeem`、`/auth/card-key-record`、`POST /auth/device-identity` |
| 会话 | `GET /sessions`、`/sessions/{id}` |
| 计费 | `/billing/balance`、`/billing/ledger`、`/billing/daily-usage`、`/topup/tiers`、`/topup/orders`、`/topup/orders/{id}` |
| 定价 | `/processing-pricing`、`/tiers`、`POST /quotes` |
| 生成 | `/generation-tasks`、`/{id}`、`/{id}/token`、`/{id}/settle` |
| 去重可用性 | `/dedup-availability` |
| 云端擦字幕 | `/cloud-subtitle-jobs/availability` |
| 搬运 | `/mount-copy/quote`、`/mount-copy/generations`、`/{id}`、`/{id}/result`、`/{id}/ack`、`/{id}/fail` |
| 邀请 | `/invitations/code`、`/invitations/me` |
| 公告 | `/announcements` |

关键机制：
- **报价单（quote）**：`/quotes` 需 `tierId` + `processingLevel`；若返回 `device_identity_required`，
  客户端先上报 `machineIdentity` 再重试 `POST /quotes` → 设备绑定
- **任务令牌**：`/generation-tasks/{id}/token` + `/settle` → 按任务结算积分
- **任务级环境变量**：`GROVE_MAX_SESSION_TOKEN`、`GROVE_MAX_TASK_TOKEN`、`GROVE_MAX_MODEL_RELAY_URL`
- 支付渠道：`https://zpayz.cn/`、`https://z-pay.cn/`（Z-Pay 聚合支付）
- 前端文案：「充值通道正在维护，稍后再来，或联系产品方线下充值。」（支持线下充值）

---

## 六、授权与商业化

- **卡密（card key）**：`secret_store_set/get/clear_card_key`，存 Windows 凭据管理器
- 授权状态：`LICENSE_REQUIRED` / `LICENSE_INVALID` / `LICENSE_DEVICE_MISMATCH` / `LICENSE_EXPIRED` / `LICENSE_UNAVAILABLE`
- 错误路由：`LICENSE_*`→`login`/`expiry`/`message`；`RECIPE_*`→`login`/`expiry`/`message`
- 设备绑定 + 安装 ID（`grove-max.installation-id`，Win 侧经 `secret_store_installation_id`）
- 积分制：`quote → task → settle`；文案「已扣的积分不会退还，重新开始要再扣一次」「不会重复扣积分」
- 邀请/拉新：`/invitations/*`（文案「邀请活动暂未开放」）
- 前端品牌呈现：`Grove Max`、营销页 `pricing-elevenlabs-{free,starter,creator}-*.png`（**ElevenLabs** 语音合成档位图，
  说明产品线含 TTS 配音能力，具体调用路径未在本次样本中验证）、`onboarding/*.mp4` 引导视频
- 免责声明页：`免责声明`（前端硬编码免责文案）

---

## 七、第三方组件许可（内置声明）

| 组件 | 版本 | 用途 |
|---|---|---|
| DrawStampUtils | commit `dd3144d5…`(2025-12-16) | 电子公章绘制（Apache-2.0，声明「upstream drawing code is used unchanged; Grove Max supplies its template configuration」） |
| signature_pad | 5.1.4 | 手写签名 |
| simplex-noise | 4.0.3 | 噪声（`RecipeGrain` 颗粒） |
| @fontsource/noto-serif-sc | 5.3.0 | 字体 |
| React Bits | — | UI 组件 |
| smbios-lib | — | 设备指纹 |

---

## 八、行为与合规观察（事实陈述）

1. **术语漂移**：对外主叙述为「漫剧报审材料准备工具」，但内部 12 步 `processingLevel`、`DedupRecipe`、
   `x-grove-dedup-profile: regular-reference-v2` 等标识直接使用去重语义；前端存在把「去重」替换为「处理」的
   错误信息改写函数。属于**对外表述与内部实现不一致**。
2. **服务端强控**：配方（Recipe）与积分报价均由服务端签发并校验，客户端仅是执行器；篡改/离线不可用。
3. **自动化分发**：通过 Playwright + 持久化会话，对 `channels.weixin.qq.com` 执行批量上传与发表结果确认，
   并对「剧集名」做检索匹配（含 `SERIES_AMBIGUOUS` 歧义分支）。
4. **规避性配置面**：`RecipeGrade/RecipeGrain/RecipeJitter/RecipeAbBlend/RecipeTemporalBlend/RecipeMicroFrames/
   RecipeFrameRate/RecipeBitrat` 等构成面向「画面与编码指纹」的参数化处理面。**本文档不展开这些参数的具体取值策略。**
5. **合规声明自相矛盾**：一方面内建「AI 提示标识」与免责声明，另一方面自动化搬运与去重分发第三方内容，
   免责条款（「因违法或未经授权使用产生的后果由使用者自行承担」）实质是把风险转移给终端用户。

---

## 九、本轮对上一轮结论的修正与补充

| 项 | 上一轮 | 本轮（已验证） |
|---|---|---|
| 前端 JS | 未还原 | **完整还原**（brotli 资源表解密） |
| 浏览器自动化 | 「无 Playwright 依赖」 | **修正**：Chromium 为独立运行时角色，`channels-weixin-session.json` 与 `playwrightVersion` 字段证实 Playwright 用于视频号发布 |
| STTN | 未提及 | 本地字幕擦除走 `subtitle-sttn/infer_model.pth` + `preferSttn` 标志 |
| 云端擦字幕 | 未提及 | 存在 `/cloud-subtitle-jobs/availability` 与 Node 分析 CLI |
| 处理档位 | 12 步枚举 | 对外仅 3 档：`保持原片 / 常规处理 / 深度处理`；管线版本 `v1/v4/v5` |
| 加速组件 | 未提及 | `torch-cuda` 组件 + `nvidia-smi` 探测 + `--cpu-only` 回退 + 回滚标记 |
| 音频 | 未提及 | Demucs 音源分离 + Rubber Band 变速变调 + `narrationRecut` |
| 支付 | 未提及 | Z-Pay（`zpayz.cn` / `z-pay.cn`）+ 线下充值 |

## 十、未验证 / 未知项

- STTN 模型的实际网络结构（未做权重级分析）
- 云端 `/cloud-subtitle-jobs/*` 与 `/quotes` 的真实响应契约（未联网调用）
- `@grove-max/engine-render` 包内容（未随主程序分发，属运行时组件）
- ElevenLabs TTS 的实际调用链路（仅见定价物料）
- 服务端下发的 Recipe 默认值（属服务端策略）
- macOS 版本（0.4.2 dmg）与 Windows 1.2.4 的功能差异
