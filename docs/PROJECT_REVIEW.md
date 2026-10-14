# 项目复盘与缺口清单（技术学习用）

> 本文是「擦除确认流」落地之后对整个仓库做的一次系统复盘：把**已实现**、
> **本地可补**、**受硬件/平台阻塞** 三类分清楚，并给出补齐顺序。
>
> 复盘范围：根 `README.md` Roadmap、`docs/SUBTITLE_REMOVAL.md` /
> `docs/WATERMARK_REMOVAL.md` 的「已知局限」、`src-tauri` / `src` 的命令与界面面、
> `cloud/docs/ARCHITECTURE.md` 的功能映射与已知限制。
>
> 结论只用于技术学习；不引入任何绕过平台识别机制或版权保护的能力。

## 1. 复盘结论（一句话）

报审链路与云端链路都已闭环，但**两端各有一批「文档里写了、代码里没有」的能力**：
报审侧缺 4 个 Roadmap 项与擦除入口，擦除侧缺常驻 worker、多标识独立 ROI、模型后端多样化，
云端缺 GPU/模型类处理与桌面客户端（后者受平台阻塞）。

## 2. 缺口总表

| # | 缺口 | 来源 | 影响 | 本地可验证 | 处置 |
| --- | --- | --- | --- | --- | --- |
| E1 | worker 每帧/每片段一次进程启动 | `WATERMARK_REMOVAL.md` 已知局限；`detect.rs` 逐帧 spawn | DBNet 每帧多 ~0.6s、E2FGVI 每片段重载权重 | ✅ 假 worker + bright-rows | 本轮实现：常驻 worker（stdio 行协议） |
| E2 | 多标识共用外接 ROI | `WATERMARK_REMOVAL.md` 已知局限；`pipeline.rs` `union_rects` | 两处标识离得远时 ROI 巨大、耗时与串扰上升 | ✅ | 本轮实现：区域分组，逐组独立 ROI |
| E3 | 只有 E2FGVI 一个模型后端 | `inpaint_backends.py` | 无法按场景换更强/更快的模型 | ✅ 假模型 | 本轮实现：LaMa（单帧）/ ProPainter（视频）适配层 |
| E4 | 「第二遍仍是整帧检测 + 整帧送 worker」 | `WATERMARK_REMOVAL.md` 已知局限 | 文档过期：ROI 化已实现 | — | 本轮修正文档 |
| D1 | 转码建议与批量规范化 | 根 `README.md` Roadmap `[ ]` | 码率/分辨率不达标的素材没有可执行建议 | ✅ | 本轮实现：`dossier-core::transcode` |
| D2 | 报审包差异对比 | 根 `README.md` Roadmap `[ ]` | 改稿后无法快速看出包内容变化 | ✅ | 本轮实现：`dossier-core::diff` |
| D3 | 平台材料要求知识库 | 根 `README.md` Roadmap `[ ]` | 平台差异靠人工记忆 | ✅ | 本轮实现：`dossier-core::platforms`（内置数据） |
| D4 | 剪映草稿导入 / 导出 | 根 `README.md` Roadmap `[ ]` | 无法与剪辑工具互通分集/时间线 | ✅ | 本轮实现：`dossier-core::draft` |
| D5 | 桌面端没有擦除入口 | 复盘（`src-tauri` 只有报审命令） | 擦除只能命令行 | ✅ 命令构建可单测 | 本轮实现：`erase.rs` 命令构建 + Tauri 命令 + 界面区 |
| C1 | 校验指标缺 SSIM / 真值 PSNR | `verify.rs` | 无干净参考时只有 PSNR 代理 | ✅ | 本轮实现：SSIM + 参考帧 PSNR |
| C2 | 云端 `video.inpaint` 未声明能力 | `modules/inpaint.py` | 无 GPU 的 worker 会被派到该模块 | ✅ | 本轮实现：能力路由声明 |
| H1 | 云端 GPU 侧模型类处理（光流插帧等） | 根 `README.md` Roadmap `[ ]` | 需 GPU | ❌ 硬件 | 保持 `[ ]`，文档写明 |
| H2 | 桌面客户端（Tauri 2） | Roadmap `[ ]`；`cloud/README.md` | gio 2.68 < 2.70 | ❌ 平台 | 保持 `[ ]` |
| H3 | macOS 签名与公证 | Roadmap `[ ]` | 需 macOS + 证书 | ❌ 平台 | 保持 `[ ]` |
| H4 | 应用内更新通道 | Roadmap `[ ]` | 需发布签名与网络分发 | ❌ 平台 | 保持 `[ ]` |

## 3. 实施顺序

1. **擦除管线**：E1 常驻 worker → E2 区域分组 → E3 模型适配层 → E4 文档勘误。
2. **校验指标**：C1 SSIM / 参考帧 PSNR，并接到 cloud 的 `video.erase_verify` 阈值。
3. **云端**：C2 能力路由声明。
4. **报审链路**：D1 → D2 → D3 → D4 → D5（core 模块先行，Tauri/界面收口）。
5. 每完成一批更新本文「状态」列与相关文档，最后再跑一次全量验证。
6. **第三轮**（跑通确认流之后的收口）：修退出码撞码 → 工具开关探测降级 → 失败候选产物 →
   云端 Web 复核面板 → 文档与计数校正（见 §7）。

## 4. 状态

| 批次 | 状态 |
| --- | --- |
| E1 常驻 worker | ✅ 已完成 |
| E2 区域分组 | ✅ 已完成 |
| E3 模型适配层 | ✅ 已完成 |
| E4 文档勘误 | ✅ 已完成 |
| C1 SSIM | ✅ 已完成（Rust + cloud 接线） |
| C2 能力路由 | ✅ 已完成 |
| D1–D5 报审链路 | ✅ 已完成（D5 的 Tauri 层受平台阻塞，仅代码审阅） |
| D6 配套能力的桌面入口 | ✅ 已完成（第二轮复核补） |
| 第三轮收口（退出码语义 / 开关探测 / 候选产物 / Web 复核面板） | ✅ 已完成 |
| H1–H4 | 保持未实现（硬件/平台阻塞） |

## 5. 实施记录

### E1 常驻 worker（已完成）

- **Rust**：新增 `crates/dossier-inpaint/src/worker.rs`（`WorkerSession::spawn/run/pid`，
  stdio 行协议 `{"workDir":…}` → `{"ok":true}`；`Drop` 负责 kill）；`detect.rs` /
  `inpaint.rs` 的 worker 后端改为可持有共享会话（`with_session` / `invoke_backend`），
  删掉各自的内联 `Command`；`main.rs` 新增 `--persistent-worker`。
- **Python**：`tools/model_worker/worker.py` 新增 `--serve` 与进程内 `_ENGINE_CACHE`
  （模型只加载一次）；stdout 被 `os.dup2(2, 1)` 改道到 stderr，保证协议行不被打字污染。
- **验证**：Rust 单测 5 例（假 worker 用 `/bin/sh -c` 规避 ETXTBSY，连跑 40 次无 flake）；
  实测 320×180@10fps / 20 帧样例 `--persistent-worker` **1.14 s** vs 一次性 **8.08 s**（约 7×），
  输出一致。

### E2 区域分组（已完成）

- `pipeline.rs`：`PipelineConfig.region_group_area_ratio`（默认 2.0，`<= 1.0` 等价旧行为）、
  `RoiJob` 与 `plan_roi_jobs()`（按 `x,y` 贪心合并，阈值「合并后外扩面积 ≤ 各自独立面积和 × ratio」）；
  `process_clip` 改为逐作业生成掩码 → 修复 → 合并进并集 ROI 掩码，校验/覆盖率语义不变；
  `PipelineReport` 新增 `inpaint_batches` / `roi_jobs`；`main.rs` 新增
  `--region-group-area-ratio`。
- **验证**：3 例单测（近邻合并/远距拆分、每组只见自己的 ROI 且两处都真的被修复、
  合并掩码与并集 ROI 对齐）；实测 640×360 双标识素材 → 2 组、2 次提交，两处标识都清除。

### E3 模型后端适配层（已完成）

- `tools/model_worker/inpaint_backends.py`：`E2fgviInpainter` 泛化为 `VideoModelInpainter`
  （新增 `codec`：`tanh` = E2FGVI、`unit` = LaMa 类）；新增 `FrameWiseInpainter`（单帧模型逐帧
  调用）、`build_torch_lama`（TorchScript）、`build_external_backend` + `build_torch_propainter`
  （要求模型仓库提供 `dossier_backend.py::build_inpainter`，把上游内部 API 的不确定性隔离在
  适配层）、`BACKEND_BUILDERS` 注册表。
- `worker.py`：`--inpaint-engine` 扩展为 `auto|e2fgvi|lama|propainter|column`。
- **验证**：`tools/model_worker` 单测 48 例通过（1 skipped），新增 `TensorCodecTests` /
  `FrameWiseInpainterTests` / `BackendRegistryTests` / `ServeModeTests`。
  权重本身在本机（aarch64、无 GPU）跑不起来，属已知局限。

### E4 文档勘误（已完成）

- `docs/WATERMARK_REMOVAL.md`：删除「第二遍仍是整帧检测 + 整帧送 worker」这条过期局限
  （ROI 化早已实现），把 worker 启动开销与多区域裁剪两条改写为「已实现 / 可拆分」，
  并补上 `--persistent-worker`、`--region-group-area-ratio`、lama / propainter 用法。
- `docs/SUBTITLE_REMOVAL.md`：代码位置表补 `worker.rs` / `verify.rs`；补充常驻 worker 与区域
  分组示例、参数表、`--serve` 行协议、SSIM 指标行；后续工作 1/2 标记完成。
- `docs/DEVELOPMENT.md`：补三条工程约定（区域分组 / 常驻 worker / 只测量不裁决）。

### C1 校验指标 SSIM（已完成）

- **Rust**（`verify.rs`）：新增第 5 项指标 `SsimMetric{samples, frames_below_fail, mean_ssim,
  min_ssim}` 与 `VerifyConfig.ssim_warn`（0.85）/`ssim_fail`（0.55）；`check_consistency`
  同时计算全局 SSIM（`global_ssim`，C1=6.5025 / C2=58.5225），`finish()` 增加 SSIM 裁决；
  `main.rs` 新增 `--verify-ssim-warn` / `--verify-ssim-fail`。
- **cloud**：`InpaintParams.ssim_warn/ssim_fail` 透传给 CLI；
  `EraseVerifyParams.ssim_warn/ssim_fail` + `erase_ssim` 裁决项（用最差帧 `minSsim`，
  方向与残留/闪烁/接缝相反）；`report_summary` 增加 `ssim_mean` / `ssim_min`。
- **验证**：Rust 4 例（一致序列 SSIM≈1、结构被抹平 SSIM<0.5、`ssim_catches_smearing_that_psnr_accepts`、
  配置校验）；cloud 新增 3 例（SSIM 方向裁决、阈值透传、报告摘要）。

### D1 转码建议与批量规范化（已完成）

- 新增 `crates/dossier-core/src/transcode.rs`：`TranscodeTarget`（容器 / 编码 / 码率上限 /
  分辨率上限 / 帧率 / CRF / preset / faststart）、`advise_episode` 逐集给出「是否转码 + 理由 +
  ffmpeg 参数 + 完整命令」、`plan_normalization` 汇总成计划并渲染一份 `set -euo pipefail` 脚本。
- **只建议、不执行**：脚本对已存在的输出会跳过；`libx264/libx265…` 与 ffprobe 报的
  `h264/hevc` 做了「同族不算需要转码」的映射，避免无意义重编码。
- **验证**：6 例单测（合规素材不转码、超码率/超分辨率生成限码命令、同族编码不误判、
  计划计数与脚本只含需转码的集、非法目标校验、shell 转义）。发现并修掉一个真实缺陷：
  `--worker-args` 之前不接受以连字符开头的值（`--serve` 会被当成本工具参数）。

### D2 报审包差异对比（已完成）

- 新增 `crates/dossier-core/src/diff.rs`：`PackageManifest`（可反序列化）、`diff_manifests`、
  `diff_packages`（直接读两个包目录）、`FileDiff`（新增/移除/修改/未变 + 大小增量）、
  内容指纹；未变文件不进明细，只计入汇总。
- **验证**：5 例单测（四类差异分类、相同清单无变化、带符号的大小增量、真实 manifest 形状解析、
  非法 JSON 报错）。

### D3 平台材料要求知识库（已完成）

- 新增 `crates/dossier-core/src/platforms.rs`：4 个**学习示例**档案（竖屏短剧 / 横屏长剧 /
  预告切片 / 通用兜底）+ `check_profile` 对照本地规格与材料情况（时长、方向、分辨率、码率、
  视频编码、必需材料）。数据明确标注为行业惯例示例，不是任何平台官方规范。
- **验证**：5 例单测（档案 id 唯一且自洽、横屏素材在竖屏档案下 fail、干净竖屏全通过、
  缺封面/简介 report fail、未知档案返回 None）。

### D4 剪映草稿导入 / 导出（已完成）

- 新增 `crates/dossier-core/src/draft.rs`：`export_capcut_draft` 生成
  `draft_content.json` + `draft_meta_info.json` + 导入说明（分集按顺序首尾相接，时间单位微秒，
  id 由 SHA-256 派生保证可复现），`import_capcut_draft` 读回时间线（忽略音频轨、
  容忍未知字段与最小草稿）。
- **验证**：4 例单测（导出后再导入顺序/时长/起点一致、已存在目录拒绝覆盖、
  最小且含未知字段的草稿可读且忽略音轨、非法选项校验）。

### D5 桌面端擦除入口（已完成；Tauri 层受平台阻塞，仅代码审阅）

- `crates/dossier-inpaint/src/erase.rs`（**可单测**）：`EraseRequest`（表单 + 校验）、
  `ErasePlan`（program/args/可复制命令行/报告路径）、`build_plan`、`resolve_tool`
  （显式路径 → PATH → 仓库 `target/`）、`run_plan`（带超时、按确认流退出码归档为
  pass/needsReview/verifyFailed/execError）、`EraseOutcome`。
- `crates/dossier-inpaint/src/main.rs`：新增交叉校验测试，用 `clap` 反向解析桌面端拼出的参数，
  守住「桌面端写的开关一定是 CLI 认得的」这条契约。
- `src-tauri/src/erase.rs` + `lib.rs`：`pick_video` / `erase_status` / `plan_erase` / `run_erase`
  四个命令；`src/index.html` + `src/main.js` 增加「擦除实验」区（预览命令 → 执行 → 回显结论）。
- **验证**：`dossier-inpaint` 侧 9 例单测 + 2 例 CLI 交叉校验测试通过；`rustfmt` 语法校验与
  `node --check` 通过。Tauri 层依赖 `glib ≥ 2.70`，本机装不了桌面壳（同 H2），
  因此只能用「逻辑下沉到库里单测 + 命令层极薄」的方式控制风险。

### D6 配套能力的桌面入口（第二轮复核发现并补齐）

第二轮复核发现：`transcode` / `diff` / `platforms` / `draft` 四个模块虽然有单测，
但**没有任何用户界面能到达它们**（本仓库的报审链路只有 Tauri 应用，没有 CLI）。
只补库不补入口，等于「代码里有、用户用不到」，所以补上：

- `crates/dossier-core/src/platforms.rs` 新增 `review_drama` / `ProfileReview`（逐集汇总
  `fail`/`warn` 计数与说明），使「整部剧对照」也可单测。
- `src-tauri/src/dossier.rs`（新）：`transcode_advice` / `diff_report_packages` /
  `platform_profiles` / `platform_review` / `export_draft` / `import_draft` 六个命令，
  全部是对 `dossier-core` 纯函数的薄适配（探测走与 `analyze_drama` 同一个 `probe_episode`）。
- `src/index.html` + `src/main.js` + `src/styles.css`：新增「报审配套工具」折叠面板
  （四块：转码建议、改稿对比、平台档案、剪映草稿），与既有面板同风格。
- **验证**：`dossier-core` 新增 1 例单测（35 通过）；`node --check`、HTML 标签配对检查、
  `rustfmt --edition 2021 --check` 通过。命令层同样受 `glib ≥ 2.70` 阻塞，无法在本机编译。

### C2 能力路由（已完成）

- `ModuleSpec` 新增 `capability_probe`；`worker_capabilities()` 在探测为假时**不再声明**该能力，
  于是不兼容的任务在**计划阶段**就被拒（`capability_missing`），而不是领了任务跑一半才失败。
- `modules/inpaint.py` 的 `video.inpaint` / `video.erase_verify` 挂上
  `capability_available`（纯探测：`dossier-inpaint` 可执行文件是否存在）；
  模型后端另用 `video_inpaint_model` 标签，只有配了模型 worker 的部署才声明它。
- `config.load_settings` 支持 `DRAMA_CLOUD_CAPABILITIES` 环境变量声明额外能力。
- **验证**：cloud 新增 3 例（能力随部署探测、缺工具时任务计划期被拒、GPU 专属能力由部署声明）。

## 6. 第二轮复核（全部实施之后）

第一轮补齐 E/C/D 之后重跑了一次全面复核，方法：把「文档承诺」「用户可达路径」「测试覆盖」
三者交叉比对。

### 6.1 复核发现

| 发现 | 判定 | 处置 |
| --- | --- | --- |
| D1–D4 四个模块只有库、没有界面入口（本仓库报审链路只有桌面应用，没有 CLI） | 真实缺口 | 已补：D6（Tauri 命令 + 界面面板） |
| `--worker-args` 不接受以连字符开头的值（`--serve` / `--e2fgvi-repo`） | 真实缺陷 | 已修：`allow_hyphen_values = true`；新增 CLI 交叉校验测试 |
| 桌面端擦除入口缺「预览命令」，用户只能盲执行 | 体验缺口 | 已补：`plan_erase` 命令 + 界面预览 |
| `WATERMARK_REMOVAL.md` 仍写「第二遍整帧检测、worker 每帧起进程」 | 文档过期 | 已修（E4） |
| 三处文档的云端测试数量停留在 117 / 139 | 文档过期 | 已修：先统一为 144，第三轮后为 147 |
| 云端 `video.inpaint` 在无 GPU 的 worker 上也会被领取 | 真实缺口 | 已修（C2 能力探测） |

### 6.2 保持未实现（硬件 / 平台阻塞，非遗漏）

| 项 | 阻塞原因 |
| --- | --- |
| H1 云端 GPU 侧模型类处理（光流插帧、模型修复） | 本机 aarch64 无 GPU、无 CUDA；torch 无官方 wheel |
| H2 桌面客户端（Tauri 2）在本机编译 | glib 2.68 < 2.70、缺 javascriptcoregtk-4.1 |
| H3 macOS 签名与公证 | 需要 macOS 机器与开发者证书 |
| H4 应用内更新通道 | 需要发布签名与分发网络 |

H2 的替代路径已存在：擦除/转码/对比等能力都有命令行入口，`cloud/` 提供 Web 客户端。

### 6.3 第二轮验证结果（当时的快照）

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| 格式 | `cargo fmt --all -- --check` | 通过 |
| 静态检查 | `cargo clippy -p dossier-core -p dossier-inpaint --all-targets -- -D warnings` | 通过 |
| 核心单测 | `cargo test -p dossier-core` | 35 通过 |
| 擦除单测 | `cargo test -p dossier-inpaint` | 91 + 2（CLI 交叉校验）通过；第三轮后为 91 + 3 |
| 云端测试 | `cd cloud && pytest -q` | 144 通过；第三轮后为 147 |
| 模型 worker 测试 | `cd tools/model_worker && python -m unittest discover -s tests` | 48 通过（1 skip） |
| 云端端到端 | `./cloud/demo.sh` | 4 条配方全部 SUCCEEDED，门禁通过 |
| 前端语法 | `node --check src/main.js` | 通过 |
| 界面结构 | `html.parser` 标签配对检查 | 通过（无未闭合 / 错配） |
| 桌面命令层格式 | `rustfmt --edition 2021 --check src-tauri/src/*.rs` | 通过 |
| 配置文件 | `python3 -m json.tool`（tauri.conf.json / capabilities / 配方） | 通过 |
| 桌面壳编译 | `cargo check -p drama-dossier` | **无法执行**（缺 glib ≥ 2.70 / javascriptcoregtk-4.1） |

已知不可本地验证的部分（E3 的模型权重、D5/D6 的 Tauri 命令层）都已在
`docs/SUBTITLE_REMOVAL.md` / `docs/WATERMARK_REMOVAL.md` 的「已知局限」中写明，
并由「逻辑下沉到库 + 命令层极薄 + 库侧单测」的方式降低风险。

### 6.4 第二轮补充检查

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| 界面结构 | `html.parser` 标签配对检查 | 通过（无未闭合 / 错配） |
| 桌面命令层格式 | `rustfmt --edition 2021 --check src-tauri/src/*.rs` | 通过 |
| IPC 契约闭合 | 前端 `call()` 集合 vs `generate_handler!` 集合 | 16 / 16 一致 |
| 配置文件 | `python3 -m json.tool`（tauri.conf.json / capabilities / 配方） | 通过 |

## 7. 第三轮复核（跑通确认流之后）

第二轮补完之后，用**真实配方**（`drama.logo.erase`，合成素材 + 真实 `dossier-inpaint`
二进制）把确认流从头跑了一遍，而不是只看单测。端到端跑出来 4 个问题，均已修复：

### 7.1 复核发现

| 发现 | 判定 | 处置 |
| --- | --- | --- |
| `dossier-inpaint` 的 clap 用法错误退出码是 2，与确认流的「自动校验失败」（2）**撞码** | 真实缺陷 | 已修：参数解析失败统一映射成 1（执行错误）；新增单测钉住 0/2/3 语义唯一 |
| 云端无条件传 `--verify-ssim-warn`，遇到不认识该开关的旧二进制会**直接解析失败** | 真实缺陷 | 已修：读 `--help` 探测工具支持的开关（按「路径+大小+mtime」缓存），探测不到就退回旧指标并在日志里说明；新增 3 例测试 |
| 重试耗尽后失败任务**不留任何产物**，于是「标记通过」永远报「没有可交付产物」 | 真实缺陷 | 已修：保留最后一遍擦除结果为 `inpaint_candidate` 候选产物（`auto_verified: false`）并挂到复核记录上，人工可放行 |
| 云端 Web 客户端**没有复核队列入口**（只有 CLI / HTTP API 能操作） | 可达性缺口 | 已修：新增「7 · 人工复核队列」面板（列表 / 详情 / 标记通过 / 标记失败 / 修正后重跑） |

前两个问题的共同根因是**两个发布物的版本可能不同步**，而当时的契约只覆盖了「工具存在」，
没有覆盖「工具认识这个开关」。现在把「按能力拼参数」作为默认做法。

### 7.2 端到端实测（合成素材，真实二进制）

| 环节 | 观测 |
| --- | --- |
| 擦除 + 自动校验 | `drama.logo.erase` 提交后 Worker 执行，确认流判 `fail`（残留 0.00%、闪烁 20.62、接缝 12.52、一致性 8.5 dB、SSIM −0.481） |
| 分支A 自动重试 | 第 2、3 次自动加力（`dilate`/`feather`/`context` 递增），指标随加力下降（闪烁 20.62 → 7.43） |
| 分支B 转人工 | 3 次用尽 → `FAILED` + `auto_verify_failed` 复核记录 + 586 KB `inpaint_candidate` 候选产物 |
| 人工动作 | `review-rerun`（改 `dilate=8`）→ 新任务 + 原记录 `superseded`；`review-approve` → 放行候选产物、写入审计 |

合成素材（`testsrc2` 随机纹理）天然无法被修复算法重建，所以指标必然不达标——这恰好是
确认流该有的行为：**它拦住了不该自动放行的结果**。真实素材上是否通过取决于画面的可修复性。

### 7.3 最终验证（全部改动之后重跑）

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| 格式 | `cargo fmt --all -- --check` | 通过 |
| 静态检查 | `cargo clippy -p dossier-core -p dossier-inpaint --all-targets -- -D warnings` | 通过 |
| 核心单测 | `cargo test -p dossier-core` | 35 通过 |
| 擦除单测 | `cargo test -p dossier-inpaint` | 91 + 3 通过 |
| 云端测试 | `cd cloud && pytest -q` | 147 通过 |
| 模型 worker 测试 | `python -m unittest discover -s tests` | 48 通过（1 skip） |
| 云端端到端 | `./cloud/demo.sh` | 4 条配方全部 SUCCEEDED、门禁 9/9 与 10/10 通过 |
| 确认流端到端 | `drama.logo.erase` + 真实 `dossier-inpaint` | 见 §7.2（自动重试 → 人工队列 → 放行/重跑） |
| 前端语法 | `node --check src/main.js` | 通过 |
| 界面结构 | `html.parser` 标签配对检查（两个 UI） | 通过 |
| 云端 Web 脚本 | 抽取 `<script>` 后 `node --check` | 通过 |
| 桌面命令层格式 | `rustfmt --edition 2021 --check src-tauri/src/*.rs` | 通过 |
| 桌面壳编译 | `cargo check -p drama-dossier` | **无法执行**（缺 glib ≥ 2.70 / javascriptcoregtk-4.1） |

### 7.4 仍然保持未实现

`H1`–`H4` 不变（见 §6.2）。另外有三项属于「原型明确不做」，不是遗漏：

- 复核状态**不阻断产物下载**（`approved` 之前也能取到文件）——生产环境应在下载端点校验；
- 没有操作者身份体系，复核的 `actor` 就是令牌身份，没有双人复核约束；
- 没有视频画面级的差分预览（复核者看的是指标 + 成品文件，不是并排对比工具）。
