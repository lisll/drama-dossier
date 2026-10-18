# 短剧工具逆向分析报告：Grove Max 与 剧集助手 V2

> 分析日期：2026-10-13
> 分析方法：静态分析（解包 + 字符串提取 + 配置/资源清点 + 模块文件名清点）
> 分析工具：7-Zip 25.01、Python、grep/strings；方法论参照 REA（Reverse Engineer Anything）的证据链原则
> 声明：本报告仅做功能存在性梳理与架构定性，**不包含**可操作的规避参数配方，**不涉及**任何破解、补丁或复刻实现。

---

## 0. 分析对象

| | Grove Max | 剧集助手 V2 |
|---|---|---|
| 样本文件 | `Grove-Max_0.4.2_aarch64.dmg` / `Grove-Max_1.2.4_x64-setup.exe` | `剧集助手V2-2.2.7.zip` |
| 官网 | https://grove-max.app/ | （第三方卡密渠道分发） |
| 对外定位 | "漫剧报审材料准备工具" | 短剧批量处理工具 |
| Windows 版 SHA-256 | `747dae2f9e344933ba8129176d6295092960a604263cd64440765c16965d6012`（与官网一致） | `9880f1605d184f5aed93c453ad484d5cc69686c31141e6be2b7e9e43b6c68533`（zip） |
| 主程序 SHA-256 | `d4f21731db38d367976b15a57192587ecf15ad07d317d46188a1b39cf8d9f9a7` | — |

---

## 1. 技术架构对比

### 1.1 Grove Max

- **框架**：Tauri 2（Rust + WebView）
  - macOS 0.4.2：Tauri updater 插件 2.10.1，Mach-O arm64，19MB
  - Windows 1.2.4：Tauri 2.11.5，PE32+，40.8MB，**无数字签名**（PE 证书表 size=0）
- **安装器**：DMG（HFS+，含 macOS 代码签名）/ NSIS-3（含 `nsis_tauri_utils.dll`）
- **前端**：Vite 构建后内嵌于 Rust 二进制（开发期 `localhost:1420`），Resources 仅 `client-config.json` + 图标
- **Bundle ID**：`cn.grovemax.review`；内部名 `grove-max-review`
- **出品方线索**：Raycaster Studio（`api.raycaster.app`、`gm.raycaster.app`、`dl.raycaster.app`）；开发者 Rust 构建路径泄露 `C:\Users\yr\.cargo\...`

### 1.2 剧集助手 V2

- **框架**：PySide6（Qt）桌面应用，PyInstaller（Python 3.13）单文件打包
  - 原版主程序 51MB；解包目录 `PYZ.pyz_extracted/` 随包附带
- **技术保护**：仅本地 RS256 JWT 卡密校验，无原生混淆 → 已被"本地修复版"整套破解（详见 §5）
- **依赖栈**（从 site-packages 清点）：PySide6、Playwright、win32com/comtypes（Office COM）、python-docx/openpyxl、pandas/numpy/scipy、onnxruntime、ctranslate2 + tokenizers（本地 AI 推理）、huggingface_hub、FastAPI + uvicorn（本地网页宿主）、pyJianYingDraft、demucs.cpp.exe / demucs_mt.exe、内置 ffmpeg/ffprobe（各 ~130MB）

### 1.3 商业模式对比

| | Grove Max | 剧集助手 V2 |
|---|---|---|
| 计费 | 邮箱注册 + 卡密兑换 + **积分按次扣减**（云端） | 卡密 + RS256 JWT + **10 个功能位拆分授权**（本地校验） |
| 支付 | z-pay.cn / zpayz.cn 第三方聚合支付 | 卡密渠道（本样本为破解伪服务器版） |
| 云端依赖 | 高（AI 中继 + 去重配方下发 + 云端擦除） | 低（本地重计算，AI 走用户/内置 Key） |
| 端点 | `api.grove-max.app`、`api.raycaster.app/grove-max`、腾讯云 COS 更新源 | 正版授权服务器（不明）；本样本指向 `http://106.52.97.61:83` 伪服务器 |

---

## 2. Grove Max 功能模块编目

> 证据来源：二进制字符串（中文 UI 文案 1192 条）、Rust 结构体符号、API 路径、环境变量、任务类型、CLI 参数、内嵌前端资源名。
> 定性图例：✅ 自有内容下合法｜⚠️ 双用途（用途决定性质）｜❌ 功能设计目的即侵权/造假

### A. 输入与素材管理

| # | 功能 | 证据 | 定性 |
|---|---|---|---|
| A1 | 本地剧集库扫描（目录→剧集→分集） | 扫描相关 UI 文案 | ✅ |
| A2 | 剪映草稿库发现/索引校验 | `grove-max-jianying-target-v1`、"剪映草稿库索引无效" | ✅ |
| A3 | 分集视频规格探测（时长/码率/分辨率） | "正在读取视频信息"、720P 标注 | ✅ |
| A4 | 原片字幕内容识别（ASR） | `source-caption-asr-large-v3-turbo-*`、`safe-caption-analysis-v2` | ⚠️ |
| A5 | 原片字幕区域检测（OCR） | `source-caption-analysis-v2`、`final-draft-analysis-v*-ocr*` | ⚠️ |
| A6 | 原片角标/叠加层检测 | `source-overlay-erasure-analysis-v1` | ❌ |

### B. 擦除与破坏性处理

| # | 功能 | 证据 | 定性 |
|---|---|---|---|
| B1 | 字幕擦除（本地模型逐帧 inpaint） | `subtitleErasure`、"正在逐帧擦除字幕"、`GROVE_PLUS_SUBTITLE_REMOVAL_MODEL` | ❌ |
| B2 | 字幕擦除（腾讯云云端作业） | `/api/cloud-subtitle-jobs`、"准备提交腾讯云自动擦除"（限前四集） | ❌ |
| B3 | 原片水印/角标擦除（Torch 模型） | `GROVE_PLUS_TORCH_OVERLAY`、`--source-overlay-erasure` | ❌ |
| B4 | 字幕重建（擦除后生成新字幕） | `subtitleReconstruction`、`--subtitle-restoration` | ⚠️ |
| B5 | 原片标识与声明擦除确认流 | "原片标识与声明擦除需要前四集字幕擦除流程" | ❌ |
| B6 | 对白/人声分轨与恢复 | "写入视频、原声和可编辑对白"、demucs 集成 | ⚠️ |

### C. 去重对抗引擎（服务端驱动）

| # | 功能 | 证据 | 定性 |
|---|---|---|---|
| C1 | 云端去重配方下发（分平台） | `/api/dedup-recipes/windows-x64` | ❌ |
| C2 | 配方版本策略 | 请求头 `x-grove-dedup-profile: regular-reference-v2`、`x-grove-processing-policy: tiers-v2` | ❌ |
| C3 | 三档去重档位：无/仅字幕/全量 | `dedup-tier-{none,subtitle,full}` 素材图 | ❌ |
| C4 | 画面扰动（调色/锐化/颗粒/帧混合维度） | `DedupRecipe` 子结构：RecipeGrade / Grain / Sharp / AbBlend / TemporalBlend | ❌ |
| C5 | 时间线扰动（裁剪/抖动/运镜/场景阈值） | RecipeCrop / CenterCrop / Jitter / Camer / Scene | ❌ |
| C6 | 编码扰动（码率/GOP/帧率/封装参数） | RecipeBitrat / FrameRate / Output（bFrames、keyframeInterval、faststart）、"编码扰动"文案 | ❌ |
| C7 | 音频去重（EQ/压缩/变调） | RecipeAudio（equalizer/compressor）、`audioDedup` | ❌ |
| C8 | 元数据清除 | recipe `metadata` 维度 | ⚠️ |
| C9 | 微帧处理 | `microFrames`、"微帧输出" | ❌ |
| C10 | 分集重组 | `recompose`、"第一轮重组"、"第二轮去重" | ❌ |
| C11 | 解说重剪 | `narrationRecut`、TTS（ElevenLabs 备选定价素材） | ⚠️ |

**`DedupRecipe` 结构维度**（仅列维度名，不含参数值）：`seed / picture / timeline / frameRate / transition / encoding / metadata / scene / microFrames / audio / regularReference`

### D. AI 重新包装

| # | 功能 | 证据 | 定性 |
|---|---|---|---|
| D1 | 新剧名生成（可保留原名） | `--custom-title` / `--preserve-title` | ⚠️ |
| D2 | 新简介生成 | `--preserve-synopsis`、oldSynopsis/newSynopsis | ⚠️ |
| D3 | 新海报/封面生成 | `--preserve-poster` | ⚠️ |
| D4 | 剧名水印烧录 | `titleWatermarkEnabled`、预览素材 | ✅（自有内容） |
| D5 | AI 生成内容标识（合规贴片） | `aiLabelRequested`、`ai-label-{portrait,landscape}.png`、`ai-labels.json` | ✅ |
| D6 | 模型中继网关（AI + 计费） | `modelRelayUrl`（`client-config.json`） | — |

### E. 报审材料产出

| # | 功能 | 证据 | 定性 |
|---|---|---|---|
| E1 | 报审包目录组织（按剧输出） | `reportPublished` | ✅ |
| E2 | 成本配置表生成 | `cost-report-input.json`、`totalInvestmentWan`、`actorCompensationRatioPercent` | ⚠️ |
| E3 | 电子签名板 | `signature_pad-*.js` | ⚠️ |
| E4 | 电子公章渲染 | `render-seal-*.js`、`sealPngBase64` | ❌ |
| E5 | 时间戳证明 | 相关任务类型 | ⚠️ |
| E6 | 剪辑过程图/工程图 | `tracks-region-canvas-*.js` | ✅（自有工程） |
| E7 | 新手引导视频 | `cost-report.mp4`、`jianying-drafts.mp4` | ✅ |

### F. 分发自动化（微信视频号）

| # | 功能 | 证据 | 定性 |
|---|---|---|---|
| F1 | 浏览器自动化登录（会话持久化） | `channels.weixin.qq.com`、`channels-weixin-session.json` | ⚠️ |
| F2 | 自动发表（表单自动填充） | "视频号表单操作失败"、"视频号发表页面已经变化" | ⚠️ |
| F3 | 批量并行上传 | `videoParallelism` | ⚠️ |
| F4 | 上传状态机/断点 | MOUNT 挂载任务体系 | ⚠️ |
| F5 | 封面自动上传 | 相关失败文案 | ⚠️ |

### G. 商业化与账号体系

| # | 功能 | 证据 |
|---|---|---|
| G1 | 邮箱注册/登录 | `/api/auth/ticket` |
| G2 | 卡密兑换 | 官网 FAQ |
| G3 | 积分计费 | "确认本次所需的积分" |
| G4 | 第三方支付 | z-pay.cn / zpayz.cn |
| G5 | 会话/任务令牌 | `GROVE_MAX_SESSION_TOKEN` / `TASK_TOKEN` |
| G6 | 设备指纹 | JWT device_id |
| G7 | 错误上报 | `/api/client-errors` |
| G8 | 运行时组件按需下载（Torch 加速组件） | `/api/runtime-downloads/` |
| G9 | 自动更新（签名校验+静默安装） | tauri-plugin-updater、腾讯 COS 更新源 |
| G10 | 邀请返利 | `invitation-hero` 素材 |

---

## 3. 剧集助手 V2 功能模块编目

> 证据来源：`config.json` 全量键（60+）、license JWT features 字段、PYZ 业务模块文件名（core/ 100+ 个）、`check_and_fix.py`、运行日志（`transcode_session.log`）、assets 目录。

**授权功能位（license JWT features）**：`transcode` `channels_upload` `ai_script` `dedupe` `jianying` `cost_sheet` `timestamp_cert` `process_diagram` `engineering_diagram` `asr`

### A. 输入与素材管理

| # | 功能 | 模块/证据 | 定性 |
|---|---|---|---|
| A1 | 剧集库扫描 | `core/scanner`；日志"扫描 → 1 部" | ✅ |
| A2 | 分集解析与命名识别 | `core/episode_parser` | ✅ |
| A3 | 媒体探测 | `core/media_probe`、MediaInfo.dll | ✅ |
| A4 | 剪映安装/草稿目录发现 | `check_and_fix.py: find_jianying_exe` | ✅ |
| A5 | 剪映草稿索引与校验 | `core/jianying/draft_index` | ✅ |
| A6 | 简介内容清洗 | `core/intro_sanitize` | ✅ |

### B. 转码与视频处理

| # | 功能 | 模块/证据 | 定性 |
|---|---|---|---|
| B1 | 批量转码引擎 | `core/transcoder`、内置 ffmpeg | ✅ |
| B2 | GPU 加速编码 | `core/gpu_encode` | ✅ |
| B3 | 转码策略（CRF/preset/码率/并发/超时） | `core/transcode_policy` | ✅ |
| B4 | 转码容量调度 | `core/transcode_capacity`、`max_parallel_dramas` | ✅ |
| B5 | 抽帧（缩略图/时间轴帧/关键帧） | `core/ffmpeg_frame` | ✅ |

### C. 去重对抗引擎（三代实现并存）

| # | 功能 | 模块/证据 | 定性 |
|---|---|---|---|
| C1 | 一代：滤镜栈 | `core/dedupe/legacy_filters`、`light_filter`、`filter_stack_params` | ❌ |
| C2 | 二代：分集重组 | `reorg_episodes`、`re_*` 配置族（重剪片段/变速/掐头去尾/集数合并） | ❌ |
| C3 | 三代：`rf_*` 全维度配方 | 调色/几何/帧率/时间混合/音频变调/元数据/编码器抖动 | ❌ |
| C4 | 内容级去重 | `content_dedup` | ❌ |
| C5 | A/B 注入管线 | `ab_inject` / `ab_soft` / `ab_pipeline` | ❌ |
| C6 | 关键帧/场景查重规避 | `fs_*` 配置族（pHash/场景切点/EQ/裁边/GOP/噪声/帧克隆/垫片等维度） | ❌ |
| C7 | 音频去重 | EQ/变调/压缩 | ❌ |
| C8 | demucs 音源分离（本地 ONNX/GGML，4 stems） | `assets/models/htdemucs*`、`demucs*.exe` | ⚠️ |
| C9 | Whisper 字幕识别 | `cnt_whisper_subtitles` | ⚠️ |
| C10 | SenseVoice ASR（本地 ctranslate2 推理） | `sensevoice_asr/worker` | ⚠️ |

### D. 剪映自动化

| # | 功能 | 模块/证据 | 定性 |
|---|---|---|---|
| D1 | 草稿构建（母工程/分集工程） | `mother_draft_bundle`、`draft_builder`、`episode_draft_writer` | ⚠️ |
| D2 | 详细草稿写入器 | `detailed_draft_writer/runner` | ⚠️ |
| D3 | 分镜草稿 v1/v2 | `mini_storyboard_*`、`storyboard_v2_*` | ⚠️ |
| D4 | 快照草稿 | `snapshot_*` | ⚠️ |
| D5 | 剪映私有字节码注入（`_cnt_bytecode` vs `_cnt_ref` 双轨） | audio/display layout、effects、素材库、校验 | ⚠️ |
| D6 | 剪映 UI 自动化（UIA 控件树遍历/暖机） | `check_and_fix.py: warm_jianying_draft` | ⚠️ |
| D7 | 时间轴/画面校准 | `calibration_store`、gui 校准对话框 | ⚠️ |
| D8 | 工程截图库 | `cnt_screenshot_lib` | ⚠️ |
| D9 | 剪映滤镜包注入（Lua/shader） | `assets/jianying_filters/summer_blossoms` | ⚠️ |
| D10 | pyJianYingDraft 开源库集成 | site-packages | ✅ |

### E. 证据材料产出

| # | 功能 | 模块/证据 | 定性 |
|---|---|---|---|
| E1 | 成本配置表生成（DOCX 模板） | `cost_sheet_docx/generator/render`、WPS/LibreOffice COM 导出 | ⚠️ |
| E2 | 印章/签名叠加渲染 | `.cache/cost_sheet_assets/{印章,签名}.png`、日志"已在 PDF 上叠加签名" | ❌ |
| E3 | 时间戳证明 | `core/timestamp_certificate` | ⚠️ |
| E4 | 工程图自动生成（网页剪辑器截图） | 日志：场景切点→stems→ASR→剪辑直出网页→等待 DOM→截图 | ⚠️ |
| E5 | 过程图生成 | 授权位 `process_diagram` | ⚠️ |
| E6 | fablecut 自研网页剪辑器 | `assets/fablecut_editor` + `core/fablecut_proof` | ⚠️ |

### F. AI 文本生成

| # | 功能 | 证据 | 定性 |
|---|---|---|---|
| F1 | AI 新剧名 | `gen_new_name` | ⚠️ |
| F2 | AI 新简介 | `gen_new_intro` | ⚠️ |
| F3 | AI 新封面 | `gen_new_cover` | ⚠️ |
| F4 | 火山方舟 API 接入 | `ark_api_key` | — |
| F5 | LAS 模型服务接入 | `las_api_key` | — |

### G. 分发自动化

| # | 功能 | 模块/证据 | 定性 |
|---|---|---|---|
| G1 | 视频号 Playwright 自动化 | `channels_playwright` | ⚠️ |
| G2 | 账号存储/多账号 | `channels_account_store` | ⚠️ |
| G3 | 上传素材打包器 | `channels_upload_bundle/assets` | ⚠️ |
| G4 | 上传执行器+状态机 | `channels_upload_runner/status` | ⚠️ |
| G5 | 已发布剧集管理 | `channels_completed_dramas` | ⚠️ |

### H. 授权与商业化

| # | 功能 | 模块 |
|---|---|---|
| H1 | 卡密授权客户端 | `core/license_client` |
| H2 | RS256 JWT 校验+公钥同步 | `license_trust`、`license_public_key_sync` |
| H3 | 授权本地存储（加密） | `license_store`、`secure_storage` |
| H4 | 设备指纹 | `core/device_id` |
| H5 | 按功能位拆分售卖 | JWT features 10 项 |
| H6 | 自动更新 | `core/update_*` 五模块 |

### I. 工程基建

审计子系统（`core/audit`：borders/compliance/metrics/probe/scorer/report，输出质量自检打分）、单实例锁、子进程管理、运行时守卫、依赖检测（`check_and_fix.py`）、UIA 自动化、Office COM、paramiko(SSH)、FastAPI 本地服务。

---

## 4. 真实执行流水线（剧集助手运行日志还原）

```
扫描: F:\漫剧素材\测试 → 识别 1 部
│
├─ 生成工程图
│   ├─ 字幕引擎 SenseVoice
│   ├─ 预抽素材库缩略图（分集）
│   ├─ 场景切点检测（每集，11~12 段）
│   ├─ demucs 分离 stems（4 条音轨 + ASR wav）
│   ├─ 抽取时间轴缩略图
│   ├─ SenseVoice 识别 → 14 条字幕
│   ├─ 启动剪辑直出本地网页服务（FastAPI）
│   ├─ Playwright 等待 DOM（轨片段/波形/胶片条/缩略图/关键帧对齐）
│   ├─ 选中字幕片段 → 截图 → 输出 XX-第N集工程图.jpg
│   └─ 逐集重复
│
└─ 生成成本图
    ├─ 加载印章.png / 签名.png（.cache/cost_sheet_assets/）
    ├─ DOCX 模板 → WPS/LibreOffice 导出 PDF
    ├─ PDF 上叠加签名印章 → 转 JPG
    └─ 输出 成本配置.jpg 到剧集文件夹
```

---

## 5. 重要安全发现：剧集助手样本为破解盗版版

本 zip 并非官方原版，而是含完整盗版授权基础设施的"本地授权版"：

| 发现 | 内容 |
|---|---|
| 补丁主程序 | "本地修复逻辑已打进 exe"（原版备份于 `_原版/`） |
| 伪授权服务器 | `http://106.52.97.61:83`（腾讯云 IP，**明文 HTTP**） |
| 伪造密钥 | `本地修复/密钥/` 含与伪后台匹配的 RSA 密钥对 |
| 万能卡密 | `LOCAL-MASTER`（card_type=master，有效期至 2076 年，全功能位） |
| 破解者信息泄露 | 工作目录 `f:\code\XiaoYao\剧集助手`、终端用户 `C:\Users\yanji` |
| 连带盗版 | 检测报告引用的剪映亦为"5.9 免激活"破解版 |

**风险提示**：
1. 运行该 patched 二进制 = 运行来历不明的钩子代码，具有完整系统权限；
2. 明文 HTTP 伪服务器对客户端有完全控制与可见性；
3. 私钥随包流通后，任何持有者均可伪造授权；
4. 分发/运营该破解版构成对"剧集助手V2"厂商的侵权。

---

## 6. Grove Max 版本演化观察

| | macOS 0.4.2（2026-08-27） | Windows 1.2.4（2026-10-10） |
|---|---|---|
| 去重配方（DedupRecipe） | 无 | 有 |
| 字幕擦除/编码扰动 | 无 | 有 |
| 视频号自动上传 | **有** | 有 |
| AI 中继端点 | `api.raycaster.app/grove-max` | `api.grove-max.app`（自有域名） |

结论：去重/擦除流水线为 8 月底至 10 月间新增的核心商业化功能；官网对此**只字未提**，对外仅宣传"报审材料准备"。

---

## 7. 合规性总评

1. 两款软件约 60% 的模块为通用视频/文档/AI 工程（转码、扫描、草稿解析、报表生成、账号体系等），自有内容场景下完全合法。
2. 决定产品性质的 ❌ 部分：**去重对抗 + 原片痕迹擦除 + AI 换皮 + 伪造盖章材料 + 自动分发**。该组合的指向为把他人成片改头换面作为新内容分发，涉及著作权侵权与向平台提交虚假材料，在中国法律环境下存在刑事风险（《刑法》第 217 条侵犯著作权罪；伪造印章另有罪名）。
3. Grove Max 将对抗配方置于服务端（破解客户端无法获得持续更新的配方）；剧集助手将全部逻辑置于本地（已被整体破解）。两种保护策略的对照具有工程参考价值。
4. 合法替代方向：报审材料自动化（真实数据+自有内容）、原创/授权内容多平台分发工具，两类需求均真实存在且可公开商业化。

---

## 附录：分析方法与边界

- 证据类型：① 二进制字符串（中/英）；② Rust 结构体与任务类型符号；③ API 路径与环境变量；④ 配置键全量；⑤ 模块文件名清点（未反编译 .pyc、未还原前端 JS）；⑥ 运行日志。
- 已验证此前第三方分析报告（`Grove-Max-分析报告.md`）的全部关键指控（约 50 项字符串证据逐一命中，含 SHA-256、PE 签名状态）。
- 本报告不含规避参数的具体取值、不含破解操作细节、不含任何复刻实现指导。
