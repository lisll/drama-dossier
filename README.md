# DramaDossier · 剧备

![CI](https://github.com/lisll/drama-dossier/actions/workflows/desktop.yml/badge.svg)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-blue)
![Rust](https://img.shields.io/badge/Rust-1.85%2B-orange)
![Tauri](https://img.shields.io/badge/Tauri-2-24C8D8)
![License](https://img.shields.io/badge/license-MIT-green)

> 短剧报审材料自动生成工具 · Automated submission dossier builder for short dramas

DramaDossier 是一个本地优先的桌面应用，用于把散落在磁盘上的剧集素材整理成结构化、可人工复核、可提交的报审材料包。项目聚焦“素材扫描 → 媒体探测 → 合规检查 → 材料包生成”的完整链路，不提供网页版，也不会上传用户素材。

> **定位说明**：本项目仅用于技术学习与交流（Rust 领域工程、Tauri 2 桌面应用、本地媒体处理管线），不用于实际报审提交或对外分发。文中的报审格式、平台要求与发布流程都是学习示例，不构成任何合规或法律意见。

## 目录

- [项目定位](#项目定位)
- [功能特性](#功能特性)
- [系统架构](#系统架构)
- [目录结构](#目录结构)
- [技术栈](#技术栈)
- [核心模块](#核心模块)
- [字幕与角标擦除实验](#字幕与角标擦除实验)
- [云端制作系统（学习原型）](#云端制作系统学习原型)
- [数据流](#数据流)
- [快速开始](#快速开始)
- [素材目录规范](#素材目录规范)
- [开发规范](#开发规范)
- [CI / CD](#ci--cd)
- [发布与部署](#发布与部署)
- [安全与隐私](#安全与隐私)
- [合规责任](#合规责任)
- [Roadmap](#roadmap)
- [常见问题](#常见问题)
- [贡献](#贡献)
- [License](#license)

## 项目定位

短剧报审通常需要整理分集清单、封面、简介、成本说明、权属声明等多类材料。这些工作高度重复、格式分散，也容易因文件遗漏导致反复返工。DramaDossier 把这一过程工程化：

- 按约定扫描本机剧集目录；
- 调用 `ffprobe` 读取分集规格；
- 按可解释规则生成材料检查报告；
- 输出编号清晰的报审包和本地完整性清单；
- 强制保留人工填写与确认的责任边界。

项目适合作为短剧内容制作工具链、Tauri 2 桌面应用、Rust 领域工程和本地媒体处理管线的学习参考。

## 功能特性

- **剧目扫描**：以“素材根目录 / 一部剧一个子目录”为约定，支持一层视频子目录。
- **集号识别**：支持 `第01集`、`E01`、`01.mp4` 等常见命名，并按集号排序。
- **媒体探测**：通过外部 `ffprobe` 获取时长、分辨率、编码、帧率和码率。
- **材料检查**：区分 `pass` / `warn` / `fail` / `pending`，避免把人工判断伪装成自动结论。
- **规则可配置**：检查阈值可在界面覆盖，并与 ffprobe 路径、输出目录一起持久化到本机配置文件。
- **报审包生成**：输出剧集信息、封面海报、分集清单、成本表、权属声明和 `manifest.json`。
- **桌面应用**：基于 Tauri 2 构建 Windows NSIS 安装包与 macOS `.app` / `.dmg`。
- **隐私优先**：素材、报告和生成结果都保留在本地文件系统。
- **合规边界明确**：不伪造签名、印章、时间戳或权属证明。
- **转码建议**：对照目标规格逐集判断是否需要转码、为什么，并生成可审阅的 `ffmpeg` 批处理脚本（只建议，不自动执行）。
- **报审包差异对比**：读两份 `manifest.json`，列出改稿前后的新增 / 移除 / 修改文件。
- **平台材料要求（示例档案）**：内置少量投放形态档案，逐集对照时长、画幅、码率、编码与必需材料。
- **剪映草稿导入 / 导出**：把分集顺序与时长交接给剪辑工具，或把草稿时间线读回来核对。
- **字幕与角标擦除实验（学习用）**：独立的 `dossier-inpaint` 管线，用 DBNet 定位字幕、时序模型（E2FGVI）重建像素；同一管线也可固定方形 ROI 擦除**自有素材**的角标/水印（小 mask 场景精度最高）。见 [硬字幕擦除](docs/SUBTITLE_REMOVAL.md) 与 [角标 / 水印擦除](docs/WATERMARK_REMOVAL.md)。

## 产品界面

应用主流程分为三个区域：

1. **素材路径区**：选择或输入剧集素材根目录，执行扫描。
2. **剧目列表**：展示剧目名称、分集数量和素材体积。
3. **检查与生成区**：查看检查结果，填写真实成本信息，生成报审包。
4. **报审配套工具（折叠）**：转码建议、改稿差异对比、平台档案对照、剪映草稿导入/导出。
5. **擦除实验（折叠）**：检测工具状态 → 预览 `dossier-inpaint` 命令 → 执行 → 回显确认流结论。

界面只负责状态展示和触发 Rust 命令，业务规则全部保存在 `dossier-core` 中。

界面操作（含四模块工作台、工具箱与常见排错）见 [`docs/USER_GUIDE.md`](docs/USER_GUIDE.md)。

## 系统架构

```text
+------------------------------------------------------+
|                    Tauri 2 Desktop                   |
|                                                      |
|  +------------------------------------------------+  |
|  |              WebView UI (src/)                 |  |
|  |  HTML / CSS / JavaScript state and forms       |  |
|  +---------------------+--------------------------+  |
|                        | invoke()                   |
|  +---------------------v--------------------------+  |
|  |           Tauri Commands (src-tauri/)          |  |
|  |  scan_library / analyze_drama / generate       |  |
|  +---------------------+--------------------------+  |
|                        | typed structs              |
|  +---------------------v--------------------------+  |
|  |          Domain Core (crates/dossier-core)     |  |
|  |  scanner -> probe -> checklist -> package      |  |
|  +---------+---------------------+----------------+  |
|            |                     |                  |
+------------|---------------------|------------------+
             |                     |
             v                     v
     Local media files        ffprobe process
```

架构原则：

- **UI 不实现业务规则**：界面只渲染状态、收集表单并调用命令。
- **命令层不做规则**：Tauri command 负责参数适配、错误转换和进程桥接。
- **核心库保持纯净**：`dossier-core` 不依赖 GUI、HTTP 服务或网络能力。
- **外部能力显式声明**：`ffprobe` 不捆绑，由用户安装或指定路径。
- **错误可操作**：路径、权限、外部进程错误都应提示下一步如何处理。
- **本地优先**：除启动外部 `ffprobe` 进程外，不发起网络请求。

## 目录结构

```text
.
├── crates/
│   ├── dossier-core/            # 纯 Rust 领域核心
│   │   └── src/
│   │       ├── scanner.rs       # 目录扫描与集号解析
│   │       ├── probe.rs         # ffprobe 调用与 JSON 解析
│   │       ├── checklist.rs     # 报审材料检查规则
│   │       ├── package.rs       # 报审包生成与 manifest
│   │       ├── diff.rs          # 两份清单的改稿差异对比
│   │       ├── transcode.rs     # 转码建议与批量规范化脚本
│   │       ├── platforms.rs     # 平台材料要求知识库（示例档案）
│   │       ├── draft.rs         # 剪映草稿导出 / 导入
│   │       ├── progress.rs      # 长任务进度流（JSONL，可续写）
│   │       ├── checkpoint.rs    # 检查点与断点续跑
│   │       ├── pipeline_version.rs  # 管线版本与产物一致性校验
│   │       ├── types.rs         # 共享领域模型
│   │       └── bin/dossier-artifacts.rs  # `check`：跨产物戳记一致性校验（退出码 0/1/2）
│   ├── dossier-runtime/         # 运行时能力管理（Tier 1）
│   │   └── src/
│   │       ├── command.rs       # 可注入的命令执行器（含 FakeRunner）
│   │       ├── probe.rs         # 工具版本 / ffmpeg 滤镜与编码器 / GPU 探测
│   │       ├── caps.rs          # 决策：GPU 够格、硬件编码器、并行度
│   │       ├── manifest.rs      # 运行时组件清单
│   │       ├── store.rs         # 校验 / 安装 / 状态 / 回滚
│   │       ├── update.rs        # 更新通道：签名清单 + 验签 + 版本比较
│   │       ├── secret.rs        # 本地凭据存储（0600 + 原子写）
│   │       ├── device.rs        # 设备指纹（只输出摘要）
│   │       ├── paths.rs         # 平台数据目录（XDG / HOME / Windows %APPDATA%）
│   │       └── main.rs          # probe / selfcheck / install / rollback / update
│   ├── dossier-media/           # 后期能力适配（Tier 2）
│   │   └── src/
│   │       ├── tools.rs         # 工具链盘点与能力标签
│   │       ├── render.rs        # 合成渲染（文字 / 图片 / 字幕烧录）
│   │       ├── shot.rs          # 镜头切点检测
│   │       ├── timestretch.rs   # 变速变调（Rubber Band 优先）
│   │       ├── separate.rs      # 音源分离（Demucs）
│   │       ├── asr.rs           # 语音识别（Whisper）
│   │       ├── ocr.rs           # OCR 文本识别（Tesseract）
│   │       └── srt.rs           # 字幕读写
│   └── dossier-inpaint/         # 字幕 / 角标擦除管线（学习用原型）
│       └── src/
│           ├── probe.rs         # ffprobe 解析
│           ├── frames.rs        # bgr24 rawvideo 编解码
│           ├── mask.rs          # 掩码、膨胀、羽化、合成
│           ├── patch.rs         # ROI 裁剪与回贴
│           ├── detect.rs        # DBNet 检测后端与跨帧聚合
│           ├── inpaint.rs       # E2FGVI / 基线修复后端
│           ├── worker.rs        # 常驻 worker 会话（stdio 行协议）
│           ├── verify.rs        # 擦除结果自动校验（确认流）
│           ├── erase.rs         # 桌面入口的命令拼装与执行
│           └── pipeline.rs      # 两遍管线 + 区域分组
├── src/                         # Tauri WebView 前端
│   ├── index.html
│   ├── main.js
│   └── styles.css
├── src-tauri/                   # Tauri 桌面壳
│   ├── src/
│   │   ├── lib.rs               # IPC commands 与应用入口
│   │   ├── erase.rs             # 擦除入口命令（预览 / 执行）
│   │   └── dossier.rs           # 转码建议 / 差异对比 / 平台档案 / 剪映草稿命令
│   ├── tauri.conf.json          # 窗口、bundle 和打包配置
│   ├── capabilities/            # Tauri capability 配置
│   └── icons/                   # Windows / macOS 图标
├── tools/
│   └── model_worker/            # 模型 worker（DBNet / E2FGVI）
│       ├── worker.py            # 入口：读请求 → 跑模型 → 写产物
│       ├── dbnet.py             # DBNet ONNX 推理与后处理
│       ├── inpaint_backends.py  # E2FGVI 适配层与基线
│       └── fetch_models.sh      # 模型获取（权重不入库）
├── cloud/                       # 云端制作系统（学习原型，Python）
│   ├── src/drama_cloud/         # API / CLI / Worker / 配方 / 处理模块
│   ├── recipes/                 # 内置配方（7 份，含 drama-logo-erase 擦除配方）
│   ├── docs/                    # 架构、配方、管线、API、数据库、打包、测试、擦除确认流
│   │   └── INPAINT_REVIEW_FLOW.md  # 确认流：自动校验 + 分支A 重试 + 分支B 人工复核
│   └── tests/                   # 215 个测试
├── docs/
│   ├── BUILD.md                 # 构建与发布说明
│   ├── DEVELOPMENT.md           # 开发规范与模块边界
│   ├── MATERIAL_SPEC.md         # 素材与报审包数据规格
│   ├── PROJECT_REVIEW.md        # 全项目复盘、缺口清单与验证记录
│   ├── RUNTIME.md               # Tier 1/2/3：运行时、后期与长任务能力层
│   ├── SUBTITLE_REMOVAL.md      # 硬字幕擦除原理、协议与实测
│   ├── USER_GUIDE.md            # 用户使用手册（界面操作、工具箱与排错）
│   └── WATERMARK_REMOVAL.md     # 角标 / 水印擦除参数、编码策略与实测
└── .github/workflows/           # CI 和桌面包构建
```

## 技术栈

| 层级 | 技术 | 说明 |
|---|---|---|
| 桌面壳 | Tauri 2 | 窗口、WebView、IPC、打包和 capability 管理 |
| 后端核心 | Rust / edition 2021 | 领域逻辑、文件处理和进程调用 |
| 序列化 | Serde / serde_json | 领域模型与前端 JSON 协议 |
| 哈希 | SHA-2 | 生成分集和材料包的本地校验值 |
| 文件遍历 | walkdir | 读取剧目目录结构 |
| 前端 | HTML / CSS / JavaScript | 无构建框架的轻量 WebView UI |
| 媒体探测 | FFmpeg `ffprobe` | 外部依赖，不随应用重新分发 |
| CI/CD | GitHub Actions | 多平台质量检查和安装包构建 |
| 打包 | NSIS / DMG | Windows 与 macOS 安装包 |

## 核心模块

### `scanner` — 素材扫描

- 扫描用户选择的素材根目录；
- 每个非隐藏一级子目录识别为一部剧；
- 读取剧目目录及一层子目录中的视频、封面和简介；
- 从文件名解析集号；
- 排序分集并返回 `Drama` 模型。

支持的视频扩展名：

```text
mp4, mov, mkv, flv, ts, m4v, webm
```

封面识别关键字：

```text
封面, cover, poster
```

简介识别关键字：

```text
简介, intro, synopsis
```

集号解析优先级：

1. `第12集` / `第12话` / `第12回`
2. 词边界形式的 `E12` / `e03`
3. 文件名主干中的独立数字，例如 `show_12`

### `probe` — ffprobe 探测

- 不捆绑 FFmpeg 二进制；
- 默认从 `PATH` 查找 `ffprobe`；
- 支持用户指定 `ffprobe` 完整路径；
- 使用 `--show_format --show_streams --print_format json` 获取数据；
- 将结果解析为 `EpisodeSpec`；
- 探测逻辑与进程调用分离，便于使用固定 JSON 样本测试。

提取字段包括：

- 时长；
- 宽高；
- 视频编码；
- 音频编码；
- 平均帧率；
- 总码率。

### `checklist` — 材料检查

检查器不修改文件，只生成可解释的检查报告。

| 检查项 | 默认规则 | 级别 |
|---|---:|---|
| 分集数量 | 0 集失败；少于 4 集警告 | `fail` / `warn` |
| 封面 | 缺失失败 | `fail` |
| 简介 | 缺失失败；少于 30 字警告 | `fail` / `warn` |
| 单集时长 | 31–480 秒之外警告 | `warn` |
| 码率 | 超过 8 Mbps 建议规范化 | `warn` |
| 画幅 | 高大于宽为竖屏建议 | `warn` |

`ready = true` 表示没有 `fail` 项；`warn` 仍需要人工确认。

### `package` — 报审包生成

默认输出到剧目目录下的 `报审包/`：

```text
报审包/
├── 01_剧集信息/
│   ├── 剧名.txt
│   └── 简介.txt
├── 02_封面海报/
│   └── 封面.jpg
├── 03_分集清单/
│   └── 分集清单.csv
├── 04_成本配置表/
│   └── 成本配置表.md
├── 05_权属声明/
│   └── 内容权属声明.txt
├── 06_分集视频/          # 仅选择复制时生成
│   └── 第01集.mp4
└── manifest.json
```

`manifest.json` 记录：

- schema 版本；
- 剧目名称；
- UTC 生成时间；
- 是否使用 AI 辅助生成；
- 是否复制分集视频；
- 每个文件的相对路径、大小和 SHA-256。

manifest 是本地完整性清单，不是官方审核回执，也不能替代签名、公章或可信时间戳。

### `transcode` / `diff` / `platforms` / `draft` — 报审配套能力

- **`transcode`（转码建议与批量规范化）**：对照一个目标规格（容器 / 编码 / 码率上限 /
  分辨率上限 / 帧率 / CRF）逐集给出「是否需要转码、为什么」，并生成可直接执行的
  `ffmpeg` 命令与一份 `set -euo pipefail` 的批量脚本。**只产出建议、绝不隐式转码**，
  脚本里对已存在的输出会跳过。
- **`diff`（报审包差异对比）**：读两份 `manifest.json`，输出新增 / 移除 / 修改 / 未变
  与大小变化；只比对清单登记的路径、大小、SHA-256，不读素材内容。
- **`platforms`（平台材料要求知识库）**：内置少量**学习示例**档案（竖屏短剧 / 横屏长剧 /
  预告切片 / 通用兜底），把「容器、编码、时长、画幅、码率、必需材料」结构化；
  纯查表函数，不联网、不抓取平台页面。示例数据不是官方规范。
- **`draft`（剪映草稿导入 / 导出）**：把分集顺序与时长导出成剪映草稿目录
  （`draft_content.json` + `draft_meta_info.json`），也能把草稿里的时间线读回来核对。
  草稿格式是未公开的私有 JSON，随版本变化，本模块只写「最小可用 + 常见字段」的骨架。

### `src-tauri` — Tauri IPC 桥

| Command | 作用 |
|---|---|
| `pick_folder` | 打开系统文件夹选择器 |
| `scan_library` | 扫描素材根目录下的所有剧目 |
| `load_app_settings` | 读取本机配置文件中的检查规则、ffprobe 路径与输出目录 |
| `save_app_settings` | 校验并写入本机配置文件 |
| `analyze_drama` | 探测指定剧目，按传入或默认检查规则返回检查报告 |
| `generate_package` | 重新扫描、探测并生成报审包 |
| `pick_video` | 选择待擦除的视频文件 |
| `erase_status` | 查询 `dossier-inpaint` 是否可用 |
| `plan_erase` | 只拼装擦除命令，供界面预览（不执行） |
| `run_erase` | 执行擦除并按确认流退出码（0 通过 / 3 需抽查 / 2 校验失败）回传结论 |
| `transcode_advice` | 生成转码建议与批处理脚本（不执行转码） |
| `diff_report_packages` | 对比两份报审包清单 |
| `platform_profiles` | 列出内置平台档案（示例数据） |
| `platform_review` | 按平台档案逐集对照 |
| `export_draft` / `import_draft` | 导出 / 读回剪映草稿 |

Rust command 使用 snake_case，JavaScript 调用时使用 camelCase 参数。命令层返回中文可读错误，前端只做展示和用户输入适配。

### `src` — WebView UI

- 无 React、Vue 或 Vite 构建链，降低桌面示例复杂度；
- 使用原生 HTML/CSS/JavaScript；
- 通过 `window.__TAURI__.core.invoke` 调用命令；
- 页面 CSP 与 Tauri security 配置保持本地资源边界。

## 字幕与角标擦除实验

这是与报审材料链路**相互独立**的学习原型：`crates/dossier-inpaint`（Rust 管线）
+ `tools/model_worker`（Python 模型 worker）。默认命令行运行；桌面端只在
「擦除实验」里提供「预览命令 → 执行 → 回显确认流结论」的入口，业务规则仍在库里。

- **思路**：两遍处理。第一遍抽帧用 **DBNet** 定位字幕区域，跨帧聚合出稳定的 ROI；
  第二遍按片段裁剪 ROI，生成掩码后用 **E2FGVI** 做时序视频修复，再羽化回贴。
- **角标 / 水印**：同一管线，跳过检测、直接用紧贴角标的 `--roi` 定掩码、`--context` 给 E2FGVI
  留接近方形的上下文窗口（小 mask 场景精度最高）。掩码规则是 `--dilate ≥ --feather`；
  参数表与实测见 [角标 / 水印擦除](docs/WATERMARK_REMOVAL.md)。
- **边界**：只改掩码覆盖的像素（有逐字节回归测试守住），音轨直接 copy 不重编码；
  只处理**自有素材**，不做 DRM 绕过，不处理他人作品的版权标识。
- **编码**：`--intermediate delivery` 直接出片；`lossless` 先落 FFV1 无损母版，
  之后换 CRF / preset / pix-fmt 重出片不必重跑最贵的 inpaint 步骤。
- **多标识与提速**：`--region-group-area-ratio`（默认 2.0）把离得远的标识拆成多个裁剪作业
  各自送模型；`--persistent-worker` + `worker.py --serve` 让整段视频只启动一次 worker
  （实测 20 帧样例 8.08s → 1.14s）。
- **确认流**：输出帧自动校验残留 / 闪烁 / 接缝 / 一致性 / SSIM，退出码 0 通过、3 需人工抽查、
  2 校验失败；桌面端「擦除实验」区可先预览命令再执行，并回显工具自检结论。
- **现实**：aarch64 / 无 GPU 的机器上 DBNet 可用，E2FGVI 需要 GPU 机器；
  当前仍提供一个「列填充」基线后端，用来在没有模型时把整条链路跑通。

```bash
# 不需要任何模型即可跑通链路（固定区域 + 本地基线）
cargo run -p dossier-inpaint -- \
  --input in.mp4 --output out.mp4 --roi 120,300,400,40 \
  --detector fixed --inpainter column

# 完整方案（DBNet + E2FGVI）
tools/model_worker/fetch_models.sh detect
cargo run -p dossier-inpaint -- \
  --input in.mp4 --output out.mp4 \
  --detector worker --inpainter worker \
  --worker-program python3 --worker-args tools/model_worker/worker.py

# 跨产物一致性：擦除默认写 <输出>.stamp.json，一批分集用同一条命令一起核对
cargo run -p dossier-core --bin dossier-artifacts -- check ./out/*.stamp.json
# 退出码 0=一致 / 1=不一致 / 2=读不出来；云端对应 `drama-cloud artifacts check <task_id...>`
```

原理、交换机协议、实测数据与已知局限见 [docs/SUBTITLE_REMOVAL.md](docs/SUBTITLE_REMOVAL.md)
与 [docs/WATERMARK_REMOVAL.md](docs/WATERMARK_REMOVAL.md)。

## 云端制作系统（学习原型）

与桌面报审链路**相互独立**：`cloud/`（Python + FastAPI）实现设计文档里的
「云端短剧视频制作与多平台适配系统」，覆盖 C1–C12 功能映射与阶段 1/2/3。
**仅用于技术学习与自有素材**。

- **能力**：配方发布/版本/灰度/回滚（C1/C2）、三档模式 no_edit / subtitle / full_edit（C3）、
  画面与音频处理（C4/C7）、帧率与去闪烁（C9）、时间线（C5）、编码与元数据（C6/C8）、
  分集重组（C10）、解说候选时间线（C11）、质量门禁；
- **擦除确认流（C12）**：角标/水印擦除（`video.inpaint`）+ 擦除结果自动校验（`video.erase_verify`），
  自动通过即输出；失败走**分支A 自动重试**（逐次加力 mask 膨胀 / 切换 worker 后端），
  用尽则入**分支B 人工复核队列**（失败路径保留 `inpaint_candidate` 候选产物，人工可放行 / 判定失败 /
  修正 mask 重跑），Web 客户端的「人工复核队列」面板与 `drama-cloud review-*` 命令都能操作；
- **可靠性**：持久化队列 + 租约/心跳/失联回收、阶段检查点复用、协作式取消、
  指数退避重试、产物保留期清理；
- **客户端**：API 自带的 Web 客户端（本机 gio 2.68 < Tauri 2 要求的 2.70，装不了桌面壳；
  方案见 `cloud/docs/PACKAGING_GUIDE.md`）；
- **与文档的差异**：SQLite 代替 PostgreSQL、SQLite 队列代替 Celery + Redis、
  本地文件系统代替 S3 兼容对象存储——差异集中在 `cloud/docs/ARCHITECTURE.md` §7，
  替换点分别落在 `db.py` 与 `storage.py`。

```bash
python3.9 -m venv .venv && ./.venv/bin/pip install -e cloud[dev]
./.venv/bin/drama-cloud initdb --admin demo   # 管理员令牌打印在最后一行
./.venv/bin/drama-cloud seed
./.venv/bin/drama-cloud serve                 # http://127.0.0.1:8000
./.venv/bin/drama-cloud run-worker            # 另一个终端
./cloud/demo.sh                               # 一键端到端演示
```

文档在 `cloud/docs/`：架构、配方规范（含 21 个模块参数表）、处理管线、API 规范、
数据库 Schema、打包指南、测试计划、擦除确认流。

## 数据流

```text
素材根目录
  ↓ scanner::scan_library
剧目列表
  ↓ 用户选择剧目
scanner::scan_drama_dir
  ↓ EpisodeFile[]
probe::probe_episode
  ↓ EpisodeSpec[]
checklist::run_checks
  ↓ ChecklistReport
用户确认并填写真实数据
  ↓
package::generate_report_package
  ↓
报审包 + manifest.json
```

生成报审包时会重新扫描和探测，避免使用过期前端状态。没有成功探测规格的分集不会写入分集清单。

## 快速开始

### 1. 安装依赖

通用依赖：

- Rust 1.85 或更新版本；
- Tauri CLI；
- 包含 `ffprobe` 的 FFmpeg。

macOS：

```bash
brew install rust ffmpeg
cargo install tauri-cli --locked
```

Windows：

1. 安装 [Rust](https://rustup.rs/)；
2. 安装 Visual Studio Build Tools 与 Windows SDK；
3. 安装 WebView2 Runtime；
4. 安装 [FFmpeg](https://ffmpeg.org/download.html)；
5. 安装 Tauri CLI：

```powershell
cargo install tauri-cli --locked
```

### 2. 获取源码

```bash
git clone https://github.com/lisll/drama-dossier.git
cd drama-dossier
```

### 3. 开发运行

```bash
cargo test --workspace --all-features
cargo tauri dev
```

### 4. 构建安装包

```bash
cargo tauri build
```

产物位置：

- Windows：`target/release/bundle/nsis/*.exe`
- macOS：`target/release/bundle/macos/*.app`、`target/release/bundle/dmg/*.dmg`

## 素材目录规范

推荐的素材组织方式：

```text
素材根目录/
└── 示例短剧/
    ├── 第01集.mp4
    ├── 第02集.mp4
    ├── 第03集.mp4
    ├── 第04集.mp4
    ├── 封面.jpg
    └── 简介.txt
```

推荐命名：

| 类型 | 推荐命名 |
|---|---|
| 分集 | `第01集.mp4` |
| 封面 | `封面.jpg` |
| 简介 | `简介.txt` |

简介建议不少于 30 字，且说明故事主题、题材、主要人物和受众定位。详细规则见 [`docs/MATERIAL_SPEC.md`](docs/MATERIAL_SPEC.md)。

## 开发规范

### 模块边界

| 模块 | 允许 | 禁止 |
|---|---|---|
| `dossier-core` | 领域模型、扫描、探测、检查、包生成 | 依赖 GUI、HTTP 服务或全局可变状态 |
| `src-tauri` | 参数适配、IPC、文件夹选择、进程边界 | 把业务规则散落在 command 中 |
| `src` | 表单、状态、渲染、用户提示 | 直接访问文件系统或复制核心逻辑 |

### 质量门槛

提交或合并前必须通过：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features
python3 -m json.tool src-tauri/tauri.conf.json
python3 -m json.tool src-tauri/capabilities/default.json
node --check src/main.js
```

推荐本地执行：

```bash
cargo fmt
cargo clippy --fix --allow-dirty --allow-staged
cargo test
cargo tauri build
```

### 代码风格

- 领域模型使用 Serde 的 camelCase JSON 表示；
- 错误信息应说明原因和下一步处理方式；
- 文件路径和用户输入必须做边界处理；
- 核心规则必须配单元测试；
- 不在库代码中使用 `unwrap()` 处理可失败 I/O；
- 不引入未经讨论的网络能力或遥测。

### Git 工作流

- `master` 保持可构建状态；
- 功能分支命名：`feature/<topic>`；
- 缺陷修复命名：`fix/<topic>`；
- 文档修改命名：`docs/<topic>`；
- 发布 tag 命名：`app-v<semver>`，例如 `app-v0.1.0`。

提交信息使用 Conventional Commits：

```text
feat: add transcode suggestion
fix: preserve episode order when numbers are duplicated
docs: expand packaging checklist
refactor: isolate ffprobe parser
test: add malformed media fixture
ci: add macOS Intel bundle job
```

### Pull Request 要求

PR 需要说明：

1. 变更目的；
2. 用户可见行为变化；
3. 数据或报审包结构是否变化；
4. 已执行的测试；
5. 对 Windows / macOS 的兼容性影响。

涉及以下内容时必须额外说明迁移方案：

- `manifest.json` schema；
- 分集清单 CSV 字段；
- IPC command 参数或返回值；
- 默认检查规则；
- 报审包目录结构。

## CI / CD

GitHub Actions 工作流：

- 在 Linux、Windows、macOS 上执行格式检查、Clippy 和测试；
- 在 tag `app-v*` 或手动触发时构建桌面包；
- Windows x64 构建 NSIS 安装包；
- macOS Apple Silicon 构建 `.app` / `.dmg`；
- macOS Intel 构建 `.app` / `.dmg`。

构建配置见 [`.github/workflows/desktop.yml`](.github/workflows/desktop.yml)。

## 发布与部署

### 版本规则

项目使用语义化版本：

- `MAJOR`：报审包结构、manifest schema 或 IPC 协议发生不兼容变化；
- `MINOR`：新增能力或检查规则，保持兼容；
- `PATCH`：缺陷修复、文档和构建优化。

### 发布检查

发布前确认：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-features
cargo tauri build
```

同时人工验证：

1. Windows 安装、启动、卸载；
2. macOS 安装、启动、拖入 Applications；
3. 空目录、无权限目录和 ffprobe 缺失时的提示；
4. 真实剧目扫描与检查结果；
5. 生成包能否打开且 manifest 哈希正确。

### GitHub Release

1. 更新 `Cargo.toml`、`src-tauri/Cargo.toml` 和 `src-tauri/tauri.conf.json` 中的版本；
2. 更新文档中的平台要求或产物说明；
3. 创建 PR 并合入 `master`；
4. 打 tag：

```bash
git tag app-v0.2.1
git push origin app-v0.2.1
```

5. 等待 CI 完成:`test` 三平台质量门 → `bundle` 三平台构建 → `release` 自动创建 Release;
6. Release 由 CI 自动创建(标记为 prerelease,含三个平台的安装包与 `SHA256SUMS.txt`),
   并自动附加变更列表。发布说明中的安装须知由工作流的 `release` job 提供,如需补充
   或修正,在 GitHub 上编辑该 Release 即可。

> `release` job 只在推 tag(`app-v*`)时运行。手动触发(`workflow_dispatch`)只做构建,
> 不会创建 Release;推 `master` 只跑质量门与测试。

### Windows 分发

未签名安装包首次运行可能触发 SmartScreen。正式分发建议：

- 使用 OV 或 EV Code Signing 证书；
- 记录证书私钥的保管方式；
- 不将签名凭据写入仓库或 CI 日志。

### macOS 分发

未签名应用首次打开需要在 Finder 中右键选择“打开”。正式分发建议：

- 加入 Apple Developer Program；
- 使用 Developer ID Application 证书签名；
- 执行 notarization；
- 在文档中说明 Gatekeeper 行为。

### FFmpeg 分发边界

DramaDossier 通过外部 `ffprobe` 读取媒体信息，不重新分发 FFmpeg。这样可以：

- 减小应用体积；
- 避免 FFmpeg / LGPL / GPL 组合的许可证复杂度；
- 允许用户使用系统或自有 FFmpeg 构建。

若未来提供便携包，需要单独审查对应 FFmpeg 构建的许可证。

## 安全与隐私

- 应用不上传素材；
- 不收集用户剧目、联系方式或成本数据；
- 不包含遥测；
- 不执行下载的可执行文件；
- 只读取用户显式选择的目录及其子目录；
- 生成文件只写入用户确认或约定的输出位置。

报告安全问题时请勿直接创建公开 issue。可以通过仓库维护者的安全联系方式私下报告。

## 合规责任

DramaDossier 只做真实材料的整理、格式化和完整性提示：

- 不绕过版权保护或 DRM；
- 不识别或清洗盗版素材；
- 不去除他人作品的版权标识、水印或台标，也不把擦除结果用于对外分发；仅允许清理**自有素材**的角标/台标，见 [docs/WATERMARK_REMOVAL.md](docs/WATERMARK_REMOVAL.md)；
- 不伪造公章、签名或可信时间戳；
- 不生成虚假投资或成本数据；
- 不代替用户确认著作权或授权状态。

使用者必须确保提交材料的真实性、合法性和授权完整性。AI 辅助生成的剧名、简介、封面等内容必须如实标识并经过人工审核。

## Roadmap

- [x] Rust 领域核心
- [x] Tauri 2 桌面壳
- [x] Windows / macOS 构建目标
- [x] GitHub Actions 质量与打包流水线
- [x] 转码建议与批量规范化（`dossier-core::transcode`，只建议不执行）
- [x] 剪映草稿导入 / 导出（`dossier-core::draft`，最小骨架 + 顺序核对）
- [x] 平台材料要求知识库（`dossier-core::platforms`，内置示例档案）
- [x] 检查规则自定义配置
- [x] 硬字幕擦除原型（DBNet 检测 + 时序修复管线）
- [x] 角标 / 水印擦除原型（E2FGVI 光流引导 + 无损中间态与两段式编码）
- [x] 云端制作系统原型（C1–C11 配方、持久化队列、质量门禁、Web 客户端）
- [x] 云端擦除确认流（C12：擦除 + 自动校验 + 分支A 重试 + 分支B 人工复核）
- [x] 云端人工复核队列（Web 面板 + `drama-cloud review-*`，失败路径保留候选产物供放行）
- [x] 运行时能力管理（`dossier-runtime`：组件清单/校验/安装/回滚、GPU 与硬件编码与并行度协商、更新验签、凭据、设备指纹、`selfcheck`）
- [x] 后期能力适配（`dossier-media`：合成渲染、镜头切点、变速变调、音源分离、ASR、OCR 文本识别）
- [x] 长任务工程完整性（`dossier-core`：进度流 / 检查点续跑 / 管线版本与产物一致性；见 `docs/RUNTIME.md`）
- [x] 云端能力探测补全（`ffmpeg -filters/-encoders` 解析 + 21 个模块的能力标签如实缺席）
- [x] Tier 2 后期能力接线（`video.render` / `audio.separate` / `audio.timestretch` / `subtitle.asr` / `subtitle.ocr` / `timeline.shots`）
- [x] 产物戳记与跨产物一致性（`dossier-inpaint` 写 `.stamp.json`，`dossier-artifacts check` 校验，云端 `artifacts check` 透传退出码）
- [ ] 云端：GPU 侧模型类处理（光流插帧、模型修复）
- [ ] 云端：桌面客户端（Tauri 2，需 glib ≥ 2.70）
- [x] 擦除管线：第二遍只在 ROI 内检测（约 10x 提速）
- [x] 擦除管线：多标识区域分组（`--region-group-area-ratio`，远距离标识各自送模型）
- [x] 桌面端擦除入口（预览命令 → 执行 → 按确认流退出码归档结论）
- [x] 擦除管线：worker 常驻以消除每帧进程启动开销（`--persistent-worker`，实测约 7×）
- [x] 报审包差异对比（`dossier-core::diff`）
- [ ] macOS 签名与公证示例
- [ ] 应用内更新通道（签名清单 + 验签 + 版本比较已实现；发布通道与自动替换未做）

## 常见问题

### 为什么需要安装 FFmpeg？

DramaDossier 使用 `ffprobe` 读取媒体元数据。应用本身不解析视频容器，也不捆绑 FFmpeg 二进制。

### 为什么部分项目显示警告但可以生成？

`warn` 表示需要人工判断，例如集数偏少、码率偏高或横屏画幅。应用不会替用户隐藏这些风险。

### 为什么生成包时会重新探测？

前端状态可能已经过期。重新扫描和探测可以减少使用旧规格或旧路径生成清单的风险。

### 为什么不提供网页版？

本项目需要稳定访问本地文件系统、运行外部 `ffprobe`，并把输出写入用户选择的位置。桌面边界比浏览器更合适，也更符合本地优先的数据处理模型。

## 贡献

欢迎 issue 和 pull request。提交前请：

1. 阅读 [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)；
2. 保持模块边界清晰；
3. 补充必要测试；
4. 运行完整质量门槛；
5. 说明对数据和兼容性的影响。

文档类 PR 也欢迎，包括平台差异、报审实践、构建排错和术语校对。

## License

本项目基于 [MIT License](LICENSE) 开源。
