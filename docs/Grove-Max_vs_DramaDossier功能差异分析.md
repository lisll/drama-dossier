# Grove-Max 与 DramaDossier 功能差异分析

> 分析范围：Grove-Max `0.4.2`（macOS DMG）与 `1.2.4`（Windows x64 安装器/主程序），对照 `drama-dossier` 当前仓库 `9ff2c0d`（`master` 与 `origin/master` 已同步）。
>
> 方法：Grove-Max 采用已完成的 REA 静态工件分析；DramaDossier 采用当前仓库源代码、文档、模块注册表和配置的定向检索。本文只比较技术能力，不实现功能。

## 1. 结论先行

DramaDossier 并不是“没有媒体处理能力”：它已经有本地素材扫描、ffprobe 探测、报审包、字幕封装、擦除管线、云端配方/队列/质量门禁，以及一套基于 FFmpeg 的画面、音频、时间线和编码模块。

相对之下，Grove-Max 的高置信度新增能力主要是：

1. 面向微信视频号等平台的浏览器自动化发布闭环；
2. 对真实剪映/Jianying 安装、运行环境、草稿、截图和验证的编排；
3. 云字幕任务服务（上传、异步处理、识别/恢复/擦除、结果 sidecar）；
4. 模型中继及其 session/task token、报价/credits 业务协议；
5. 运行时组件包的下载、哈希预检、原子切换和回滚；
6. macOS Keychain / Windows Credential Store 等系统凭据适配；
7. 应用内自动更新通道；
8. 面向去重生产的专用 recipe/profile 和硬件编码器选择。

其中第 8 项与 DramaDossier 的媒体处理框架有重叠，属于“专用业务模型或实现深度缺失”，不是所有底层滤镜都缺失。

## 2. 差异矩阵

差异等级含义：

- **缺失**：当前仓库未发现对应业务模块或端到端流程。
- **部分具备**：有同类底层能力，但缺少 Grove-Max 的专用协议、外部应用控制或实现深度。
- **基本等价**：两者都有可比的技术能力，差异主要在产品范围或实现方式。
- **未知**：Grove-Max 只有静态候选证据，无法确认运行时是否实际启用。

| 技术模块 | Grove-Max 静态证据 | DramaDossier 当前实现 | 判断 |
|---|---|---|---|
| 平台发布自动化 | Windows 主程序含 `https://channels.weixin.qq.com/`、浏览器 session/登录状态、封面/视频上传、表单提交、提交确认、防重复发布暂停等字符串和状态名（REA 主程序证据 `ev_ad37dd...`、`ev_40bf...`） | 只有平台规格/材料示例档案和本地逐集检查；代码检索未发现浏览器驱动、WebDriver、Playwright、视频号 URL 或上传发布命令 | **缺失** |
| 真实剪映/Jianying 集成 | 含 `discover_jianying`、`InstallJianyingResult`、`JIANYING_*`、工程根目录/草稿/截图/素材绑定等候选模块 | `dossier-core::draft` 只生成/读取最小 `draft_content.json` 时间线骨架；没有剪映安装发现、版本检测、启动、截图或真实工程验证（`README.md:64`、`README.md:701`） | **部分具备**，真实环境控制可视为缺失 |
| 云字幕任务 | 含 `/api/cloud-subtitle-jobs`、`cloud_subtitles.rs`、speech recognition、subtitle restoration、source overlay erasure、sidecar/轮询状态等候选 | 有 `subtitle.mux`（外挂/内嵌字幕烧录或封装），有本地 DBNet/E2FGVI 等擦除与确认流；没有 ASR、云字幕上传/下载、异步字幕分析和恢复服务。仓库明确写有“无 ASR、转写需外部提供”（`cloud/docs/ARCHITECTURE.md:171-172,202`） | **缺失**（本地字幕处理是部分重叠） |
| 模型中继与账号/成本协议 | 配置内含模型中继 URL；字符串含 `session-token`、`task-token`、`quoteId`、credits/报价以及去重/字幕 API 路径 | 有本地 FastAPI 控制面、Bearer 用户令牌、上传会话令牌和任务 ID；这是本地制作服务认证，不是 Grove-Max 的模型中继、报价或 credits 协议 | **缺失** |
| 专用去重配方服务 | 含 `/api/dedup-recipes/`、`x-grove-dedup-profile`、`dedup_recipe`/`dedup_shape`，配方覆盖裁切、颜色、锐化、颗粒、抖动、时间线、音频均衡/压缩、编码等 | 有通用配方版本、灰度、撤回/回滚和 FFmpeg 模块；没有 dedup 专用 API、profile 或语义化去重配方模型 | **部分具备**：基础滤镜有，专用去重业务层缺失 |
| 运行时组件生命周期 | 含 `runtime_bundle`、`runtime-manifest.json`、组件 hash/size/symlink 检查、暂存交换、原子替换和回滚标记；还出现 FFmpeg/OCR/Python/Chromium/Demucs 等角色 | FFmpeg/ffprobe 和模型 worker 由部署环境提供（`README.md:187`、`cloud/docs/PACKAGING_GUIDE.md:134`）；`Settings` 仅保存路径/能力声明，未发现运行时下载器、组件 manifest、哈希预检、原子切换/回滚 | **缺失** |
| 系统凭据存储 | 含 `secret_store`、`Credential`、`installation-id`、`license-ticket`，以及 macOS native keyring / Windows native keyring 依赖线索 | 本地云服务使用 `cloud/var/secret.key`（文件权限 0600）和 SQLite 用户令牌；未发现 Keychain、Windows Credential Store 或 Tauri keyring 集成 | **缺失** |
| 应用内更新 | 含 `tauri-plugin-updater/2.10.1` 和 COS `latest.json` 更新模板 | `src-tauri/tauri.conf.json` 未配置 updater；README Roadmap 明确“应用内更新通道”尚未完成（`README.md:716-717`） | **缺失** |
| 硬件编码器选择 | 字符串含 `h264_nvenc`、`h264_videotoolbox`、`libx264`，并有硬件编码分支候选 | `media.encode` 的逻辑编码器固定映射到 `libx264`/`libx265`/`libvpx-vp9`/`libaom-av1`；没有 NVIDIA NVENC、VideoToolbox 或按 Worker 能力动态选择的参数（`cloud/src/drama_cloud/modules/encode.py:120-130`） | **部分具备** |
| 统一桌面制作工作流 | Grove-Max 桌面端同时承载网络服务、媒体管线、外部工具、浏览器和业务界面 | DramaDossier 桌面端主要是本地报审/擦除入口；云端是独立 FastAPI + Web 客户端，面向云端的 Tauri 壳仍在 Roadmap（`README.md:706-710`、`cloud/docs/PACKAGING_GUIDE.md:141-157`） | **部分具备** |

## 3. “看起来像缺失、实际已有”的能力

以下项目不应被误判为 Grove-Max 独有：

| 能力 | DramaDossier 证据 | 结论 |
|---|---|---|
| 本地媒体探测 | 外部 `ffprobe` 读取时长、分辨率、编码、帧率和码率（`README.md:52-56`） | 基本等价；Grove-Max 另有运行时工具管理和硬件编码策略 |
| FFmpeg 媒体生产 | `media.encode` 将时间线、滤镜、字幕、元数据编译为一次 FFmpeg 调用（`cloud/docs/PROCESSING_PIPELINE.md:17-27`） | 基础管线已具备，不是整体缺失 |
| 画面处理 | `video.quality` 支持色彩、锐化、降噪、缩放；另有帧率转换、去闪烁、逐帧抽检（`cloud/src/drama_cloud/modules/filters.py:1-12`） | 与 Grove-Max 的一般视频处理有重叠；缺的是更丰富的专用去重参数和产品编排 |
| 音频处理 | EQ、响度、压缩、动态归一化、声道处理（`cloud/src/drama_cloud/modules/audio.py`） | 基本等价；Grove-Max 的差异在其去重 recipe 的业务组合和端到端流程 |
| 时间线/分集重组 | `timeline.user_edits`、`chapters.reorg`、`commentary.rebuild` | 基本等价的通用编排能力 |
| 字幕封装 | `subtitle.mux` 支持外挂/嵌入字幕，burn/soft 两种模式（`cloud/src/drama_cloud/modules/encode.py:19-70`） | 基础字幕 mux 已具备；不等于 Grove-Max 的云字幕识别/恢复服务 |
| 字幕/角标擦除 | `dossier-inpaint` + `video.inpaint` + `video.erase_verify`，有自动重试和人工复核队列（`cloud/src/drama_cloud/modules/inpaint.py:1-23`） | 本地/云端擦除基础能力已具备；Grove-Max 的云字幕分析、OCR、恢复和 sidecar 仍缺 |
| 配方、任务和可靠性 | 配方白名单、能力检查、版本快照、队列、租约、心跳、重试、取消、质量门禁 | 基础远程制作控制面已经存在；缺 Grove-Max 的模型中继/报价业务协议和统一桌面承载 |
| Tauri 桌面壳 | Tauri 2 + WebView + Rust IPC，支持 NSIS/DMG（`README.md:58`、`README.md:81-104`） | 基本等价；Grove-Max 在桌面壳中进一步整合了浏览器、外部工具和云服务 |
| 哈希/完整性 | 本地报审包 manifest、输入/产物 SHA-256、任务指纹和检查点 | 基本等价的内容完整性能力；缺的是 Grove-Max 运行时组件 manifest 的生命周期管理 |

## 4. 按技术层次归纳差异

### 4.1 业务入口层

Grove-Max 把“制作—去重—字幕—剪映—视频号发布”串成桌面工作流；DramaDossier 的桌面入口仍围绕“扫描—检查—生成报审包—擦除实验”。因此最大的差异不是单个 FFmpeg 滤镜，而是后者没有 Grove-Max 的平台发布和外部编辑器控制入口。

### 4.2 服务协议层

DramaDossier 的 `cloud/` 已有任务服务，但它的协议是本地学习原型：SQLite、文件系统对象存储、Bearer 认证和本地 Worker。Grove-Max 的静态字符串显示另一套面向产品服务的协议边界：模型 relay、session/task token、报价/credits、去重配方 API 和云字幕任务 API。两者都存在 token 和异步任务，但语义和服务边界不同，不能据此宣称 DramaDossier 已兼容 Grove-Max 后端。

### 4.3 工具/运行时层

DramaDossier 依赖系统提供的 `ffmpeg`、`ffprobe` 和可选模型 worker；文档也说明 FFmpeg 不随应用重新分发。Grove-Max 则把外部运行时视为可管理组件，包含清单、下载、完整性检查、切换和回滚。这个差异会直接影响“首次安装即用、版本固定、损坏恢复和跨机器部署”。

### 4.4 桌面安全与运维层

DramaDossier 当前使用本地 secret 文件/SQLite 令牌，应用内更新仍未实现；Grove-Max 有系统凭据适配和 updater 候选。两者都有 Tauri IPC 和资源边界，但 Grove-Max 的桌面运维闭环更完整。

## 5. 优先级判断（仅用于差异理解）

若以“功能覆盖面”而非具体实现复刻来衡量，差异优先级可分为：

1. **一级差异**：平台发布自动化、云字幕/ASR、真实剪映控制、运行时组件管理。
2. **二级差异**：模型 relay/报价/credits、系统凭据、应用内更新、统一桌面云制作入口。
3. **三级差异**：专用去重 recipe/profile、硬件编码器选择和更丰富的音视频参数。

这不是实现计划；它只是说明哪些差异属于独立技术模块，哪些只是现有媒体框架上的扩展。

## 6. 证据与边界

- Grove-Max 的结论来自静态工件：DMG 根证据 `ev_32b4b74aed45b29f20e607bcf5b7fcec998a0995ccd374cb3e195c37761bb7da`；Windows 安装器 `ev_235f0289766b425192c4adda8b82b3e60a20f5306b421c1bde2a5ff79be7f6a9`；Windows 主程序托管检查 `ev_ad37ddcc357d02fa40c2a395a6512485c35ecd48b62a3bd93748b0d60942e000`；主程序资源 `ev_40bfaa1382ada0b9ff7a1073433db37500cb5ce4b28f29d9cd5919d557c611ef`。详细工件说明见 [Grove-Max技术模块逆向分析.md](/root/.codex/tmp/Grove-Max技术模块逆向分析.md)。
- 静态字符串、错误码、结构名和资源路径可以证明“代码/资源中存在某个设计候选”，不能单独证明运行时一定调用、网络一定上传或服务端一定提供对应行为。
- DramaDossier 的“未发现”是针对当前提交 `9ff2c0d` 的定向源码/文档检索，不是形式化证明；如果未来引入未命名的外部脚本或服务，需重新核验。
- 本文未修改 DramaDossier 源码，也未实现任何 Grove-Max 功能。
