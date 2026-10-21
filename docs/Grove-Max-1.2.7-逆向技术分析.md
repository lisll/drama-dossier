# Grove Max v1.2.7 逆向技术分析（Windows 样本）

> 分析对象：`/Users/sll/tmp/1020/Grove-Max/grove-max-review.exe`（41 MB，PE32+ GUI，x86-64）
> 参考材料：`MAX1.docx`（17 张界面截图）、`日志文件.txt`（一次完整任务的 UI 运行日志）
> 方法：REA 框架流程。本机 Ghidra 引擎不可用（Swift modulemap 环境问题），故采用「归档清单解包 + PE 字符串层证据检索 + 运行时日志对照」完成，未使用反编译。
> 合规声明：本文仅做静态技术学习分析，未修改目标程序的任何文件，未对其进行运行时注入或网络交互。
> 证据标注约定：〔已证实〕= 二进制字节偏移 / 截图 / 运行日志直接可见；〔推断〕= 由证据合理延伸，需进一步验证。

---

## 1. 样本身份

| 项 | 值 | 证据 |
|---|---|---|
| 产品名 | Grove Max（窗口标题），主程序 `grove-max-review.exe` | 截图1、目录清单 |
| 版本 | 1.2.7（软件版本页显示"已是最新"；产品公告推荐 v1.2.6） | 截图1、截图13 |
| 技术栈 | **Tauri 2.11.5**（tauri-utils 2.9.3 / wry），Rust 后端 + Web 前端 | 偏移 33717088 附近的 `tauri-2.11.5\src\scope\fs.rs` 路径串 |
| Bundle ID | `cn.grovemax.review` | tauri context 串 `cn.grovemax.review`+`main`+`index.html`+`Grove Max` |
| 凭据库服务名 | `app.raycaster.grove-max` | keyring 服务名串 |
| 安装布局 | NSIS 安装：`uninstall.exe` + `grove-max-review.exe` + `client/client-config.json` | 目录清单 |
| client-config.json | `{"schemaVersion":2,"modelRelayUrl":"https://api.grove-max.app"}` | 文件内容〔已证实〕 |
| 公司主体 | Raycaster（后端 `api.raycaster.app`、下载 `dl.raycaster.app`），Grove Max 为产品品牌 | URL 串〔已证实〕 |

## 2. 总体进程拓扑

```
grove-max-review.exe (Rust/Tauri 主进程)
 ├─ WebView2 窗口 ×2（主工作台 + 账单/设置独立窗口，CSP 允许两个窗口）
 ├─ Node.js sidecar（runtime bundle 内置 node）
 │   ├─ dist/cost-report-cli.js        成本报表渲染（--input --output --python --libreoffice）
 │   ├─ dist/cloud-subtitle-analyze-cli.js  云端字幕分析 CLI（--ocr-threads 2）
 │   └─ node_modules/@grove-max/engine-render/  剪映草稿/报审材料渲染引擎（内含 NotoSansCJKsc-Bold.otf）
 ├─ Python sidecars（runtime bundle 内置 python + torchOverlay）
 │   ├─ sidecar/pipeline_v5/native_bridge.py   V5 深度处理内核桥
 │   ├─ sidecar/subtitle_removal/remove.py + finalize.py  字幕擦除（STTN 模型）
 │   ├─ sidecar/title_watermark.py   剧名水印（--validate，产出 watermark-result.log）
 │   └─ sidecar/ai_label.py          AI 声明角标（产出 result.json / error.log）
 ├─ ffmpeg / ffprobe / rubberband     音视频处理与变调
 ├─ chromium（无头）                  ① 剪映草稿渲染 ② 微信视频号自动上传（Playwright）
 └─ LibreOffice                       成本报表表格 → PNG
```

〔已证实〕以上组件名全部来自二进制常量区（runtime bundle 清单结构，偏移 33394149 附近）：
`engineRoot / ocrModelDirectory / demucsModelDirectory / torchOverlay / speechRecognitionRoot / node / ffmpeg / ffprobe / analysisSidecar / analysisPython / jianyingPython / audioPython / chromium / rubberband / libreoffice / reportPackageRunner / draftBuilderScript / audioSeparatorSidecar / audioSeparatorRunner / mountRunner / subtitleRemovalModel / shotBoundaryModel`。

〔推断〕torchOverlay 采用「运行环境解包目录 + `torch/__init__.py` 探测」方式给内置 Python 叠加 PyTorch（证据：字符串 `if overlay and os.path.isfile(os.path.join(overlay, 'torch', '__init__.py')):`）。

## 3. 前端与窗口（来自 17 张截图）

**主工作台窗口**（侧栏：首页/转码/分轨/账单/设置；底部账号区 + 邀请好友）：
- **首页**：六步教程（①授权卡密 →②充值码分 →③准备素材 →④成本报表 →⑤剪映草稿 →⑥开始任务）、消耗积分/余额卡片、7/14/30 天统计。
- **转码页**：顶部「准备运行环境」卡（首次使用需下载 + 重启启用）→ 选择转码工作目录（每剧一个子目录）→ 生成配置（剧名 AI 生成、简介 AI 开关、海报 AI 开关、成本图、AI 声明角标、剧名水印右上角）→ 处理档位两张卡（**保持原片 60 积分/次**、**深度处理 60 积分/次**）→ 开始生成 → **运行日志面板（复制日志按钮）**。
- **分轨页**：字幕擦除（处理原片第 1-4 集，「VSR 未就绪」徽标；自动识别字幕区域 + 手动框选）+ 剪映草稿（每集一份草稿：干净视频轨/原声音轨/可编辑字幕轨）。
- **设置独立窗口**：账号/账单 + 成片（转码设置/剪映/性能/软件）+ 本机（目录）。转码设置含分集时长（单集最短 40s / 参考最长 150s，31–480 整数）、AI 提示文本（"内容由AI生成，纯属虚构，请勿代入现实"，≤40 字）、9:16/16:9 水印排版；成本报表含公司名/印章（自动生成或上传）/印章编码/总投资额（万元，0–100）/演员片酬占比（≤40%）/手写签名板/免责声明（明确"仅保存在本地""严禁伪造"）。
- **性能页**：本地引擎（安装包内置）、当前编码器（**CPU 兜底**徽标，环境装好后探测 GPU 编码器）、码率修复（低码率修复到 4.5 Mbps / 高 8 Mbps）、并行任务（CPU 编码自动单路，可手动多开）。
- **软件页**：引擎版本（安装包内置，**更新失败/重试**按钮）、服务条款与隐私政策入口。
- **授权体系**：账号菜单（积分余额/输入卡密/退出登录）、退出弹窗「退出登录并解绑本机……在新电脑登录会占用一个设备名额」（**设备名额制**）、邀请好友弹窗（邀请码 PR6WFVY3，双方各得 600 积分，下载链接 grove-max.app/#downloads）。

## 4. 后端命令面（Tauri invoke 命令注册表）

〔已证实〕来自命令名拼接区（偏移 33717088–33717460），按命名族分组：

| 族 | 命令 | 功能推断 |
|---|---|---|
| transcode_ | `transcode_start` / `transcode_inspect` | 发起转码任务（参数含 inputDirectory/strategy/normalizeMismatches）、查状态 |
| generation_workflow_ | `generation_workflow_preflight` / `generation_workflow_retry_video` / `generation_workflow_company_record` / `generation_workflow_state`〔推断全名〕 | 任务预检（level/processingLevel/episodeDuration/**videoPipelineVersion**）、单集重试、公司记录、状态机 |
| mount_ | `mount_preflight` / `mount_scan_workspace` / `mount_inspect_directory` / `mount_list_plans` / `mount_save_plan` / `mount_delete_plan` / `mount_refresh_plan` / `mount_open_plan_csv` / `mount_write_generated_copy` / `mount_prepare_copy_generation_input` / `mount_start_upload` / `mount_resolve_publish_result` / `mount_open_channels_weixin` / `mount_channels_weixin_status` / `mount_logout_channels_weixin` | **发布挂载子系统**：分集计划管理（CSV）+ **微信视频号自动上传**（错误码 MOUNT_UPLOAD_ALREADY_ACTIVE「已经有一轮自动上传正在执行」、MOUNT_BROWSER_SESSION_FAILED「挂载浏览器操作异常结束」） |
| report_package_ | `report_package_scan_library` / `report_package_preflight` / `report_package_inspect` / `report_package_relay_url` / `report_package_probe_runtime` / `report_package_check_runtime_update` / `report_package_install_runtime` / `report_package_install_jianying` / `report_package_discover_jianying_roots` / `report_package_validate_jianying_root` / `report_package_capture_jianying_screenshots`（需 3、4 集四个已安装工程）/ `report_package_read/write_pending_quote` / `report_package_delete_project` / `report_package_delete_pending_quote` / `report_package_abandon_run` / `report_package_prepare_pending_ai_label` | 报审包子系统：扫库、预检、运行时管理、剪映集成、成本报价（quote）、AI 声明准备 |
| subtitle_removal_ | `subtitle_removal_preflight`（preferSttn/processingMode/videoProcessing） | 字幕擦除预检，**STTN 模型可选** |
| tracks_draft_ | `tracks_draft_start` / `tracks_draft_preview` / `tracks_draft_inspect` | 剪映草稿（三轨）生成 |
| toolbox_ | `toolbox_rename_mp4_files` / `toolbox_inspect_bitrate_directory` / `toolbox_repair_bitrates` / `toolbox_inspect_encoding_variation_directory` / `toolbox_run_encoding_variation` / `toolbox_inspect_mp4_directory` | 工具箱：批量改名、码率检测/修复（4.5/8 Mbps 两档）、编码变体 |
| secret_store_ | `secret_store_get_session` / `secret_store_set_sessiontoken` / `secret_store_clear_session` / `secret_store_get/set/clear_card_key` / `secret_store_installation_id` | 会话与卡密凭据（Windows Credential Manager，`windows-native-keyring-store` crate）、设备安装 ID |
| accelerator_ | `accelerator_status` / `accelerator_install_component` / `accelerator_cancel_install` / `accelerator_remove_component` | **运行环境一键安装器**（组件下载/取消/移除/状态） |
| ai_label_ | `ai_label_preview` | AI 声明角标预览 |

错误码家族〔已证实〕：`GENERATION_WORKFLOW_*`（去重档位与预检不一致 / **V5 仅支持深度处理档位** / 当前状态不能重试）、`V5_NATIVE_INVALID`（V5 缺少服务端签发的分集策略 / 仅支持完整的参考处理配方）、`MOUNT_*`、`JIANYING_*`（DISCOVERY_FAILED / INSTALL_FAILED / NAME_CONFLICT / RUNTIME / TARGET_INVALID…）、`RUNTIME_AUTH_REQUIRED` / `RUNTIME_UPDATE_SOURCE_MISSING` / `RUNTIME_MANIFEST_UNT[USTED]`、`COST_REPORT_*`、`AI_LABEL_RENDER_FAILED` / `TITLE_WATERMARK_RENDER_FAILED` / `V2_VIDEO_TEXT_FAILED`、`COMPANY_RECORD_STATE_UNAVAILABLE`、`client_error_report_too_large` 等。

## 5. 运行环境（runtime bundle）与一键安装

- 运行环境**不在安装包内**，首次使用经「下载并安装」获取（截图2），安装后需**重启应用**生效。目录名 `engine / python / demucs / ocr-models / report-package-runner`〔已证实〕。
- **下载受授权门禁**：错误文案「请先登录，再下载运行环境」「请确认已登录并激活有效卡密，再下载运行环境」「操作较频繁，请稍后重试」（RUNTIME_AUTH_REQUIRED）〔已证实〕——运行环境是付费能力的一部分。
- 下载源：`https://…/api/runtime-downloads/`（dl.raycaster.app 之下），清单 `runtime-manifest.json` 哈希校验（临时文件 `verification-*.tmp`；内置 `.cpython-…` 哈希样例）〔已证实〕。
- 更新通道：`https://grove-max-downloads-1301443848.cos.ap-guangzhou.myqcloud.com/grove-max-app/updates/app/internal-beta/`（腾讯云 COS，**internal-beta 通道**）；另有 `internal-betaruntime` 通道串〔已证实〕。
- 界面对应：性能页「本地引擎（安装包内置）→ 当前编码器 CPU 兜底 → 码率修复 CPU」+「安装运行环境/重新检测」；软件页「引擎版本 更新失败/重试」〔已证实，截图12/13〕。

〔推断〕对比本项目：Grove Max 的运行环境包含的不只是 ffmpeg，而是 node + python + torch + chromium + LibreOffice + 四组模型（demucs / ocr-models / subtitleRemovalModel / shotBoundaryModel）+ ASR，是「全家桶运行时」，这也是它必须做清单哈希校验与授权门禁的原因。

## 6. 转码任务管线（深度处理 = V5 内核）——运行时已完全证实

状态机来自 `workflow.json`（schemaVersion 10，status/steps/completedSteps/episodeTimings/stages）；一次任务 = `render`（sidecar 渲染）+ `finalize` 两阶段，步骤为 `recompose → microFrames → audioDedup`，`billingTaskId` 直接内嵌计费单。

**去重协议（本轮最核心发现，已证实）**：
1. 本地镜头扫描：ffmpeg `select=scene` 场景检测（`scale=160:-2` 降采样、按 `-ss/-t` 窗口分段、输出 `lavfi.scene_score`，见 `analysis/N-command.json` 与 `N-metadata.txt`）→ 汇总为 `boundary-request.json`（originalBoundaries 原始边界 + sceneCuts 场景切点 + 总时长）。
2. `boundary-request` 发给服务端（`/api/dedup-recipes`），换回**签发的去重参数包**（存入 `workflow.json` 的 `recipe` 与 `nativeV5Recipe`），带 `ruleSet`（如 `2026-10-01c+db6+tiers-v2`）、`quoteId`（计费单）、`seed`（随机种子，可复现）。
3. 参数包内容〔已证实，696266fa 实测值〕：

| 类别 | 参数（实测） | 用途 |
|---|---|---|
| picture.crop | width/heightRatio 0.94 | 轻微裁切变画幅 |
| picture.grade | contrast 1.02 / brightness −0.006 / saturation 0.99 | 微调色彩 |
| picture.sharpen | amount 0.22 | 锐化 |
| picture.abBlend | opacity 0.03 / offset 5s | A/B 双画幅混合 |
| picture.temporalBlend | previousWeight 0.25 | 时域混帧（「微帧」） |
| timeline | head/tailTrim 2s、**speed 1.093**、pitch 0 | 变速不变调 |
| frameRate | candidates [30] | 帧率 |
| encoding | h264 main / bFrames 0 / GOP 250 / 4.5Mbps+9M buffer | 输出规格 |
| **metadata** | **伪 EXIF：Apple iPhone 15 Pro / iOS 17.6.1**，另含 8 种 iPhone 机型池轮换 | **把成片伪装成手机实拍** |
| nativeV5Recipe.audio | compressor（threshold −19.5dB/ratio 2.85/attack 7.3ms…）+ 3 段 EQ（频率/增益/ Q 全随机化） | 音频去重 |

4. 两轮编码均用 **h264_nvenc**（GPU；mothers-manifest：720×1280/30fps/6Mbps）；每步产物以 JSON 信封输出（`schemaVersion/provenance/nativeSnapshot{path,sha256}/runId/taskId/quoteId`），`final-manifest.json` 记录 stage1Manifest 与工具链（含 ffmpeg.exe 的 sha256）——**全链哈希溯源**，最终成片有 fingerprint。

第二任务（6984ab7e，51 集，《保洁赴宴认子》/源剧《一纸鉴定掀起风云》）结构完全一致，episodeDuration 50–65s〔已证实〕。AI 声明（`ai-labels.json`）= 文案 + 竖/横屏 PNG（base64 内嵌）+ 角落/排版/字号配置。

**V5 内核源码补充证实（pipeline_v5 全套已采集，文件名:行号）**：
- `native_bridge.py` 是 sidecar 入口：`_plan()` 校验上游下发的 `requestCanonicalJson/inputHash/originalBoundaries/sceneCuts/boundaries/recipe` 一致性（L95-136），`validate_snapshot()` 逐项校验 JSON 信封与 recipe 的 `picture/timeline/frameRate/encoding/metadata/output/scene/audio` 结构（L139-224）——**服务端参数包在客户端是强校验后照单执行，本地无配方决策权**。
- recipe 落地为 ffmpeg 滤镜链在 `reference_profile.py` `compile_reference()`（L233-264）：`tmix`（时域混帧=temporalBlend）、`crop`、`rotate`、`noise`（颗粒）、`unsharp`（锐化）、`equalizer`、`acompressor`、`atempo`（变速）。
- `first_encode.py` L248-311：trim + PTS 校正 + 缩放 + 拼接 + CBR + 关 B-frame + segment 切分；`final_encode.py` L405-447：视频滤镜 + overlay + 音频滤镜 + metadata + movflags。
- `enhanced_profile.py` `gpu_capability()`（L87-127）用 `ffmpeg -init_hw_device opencl -c:v h264_nvenc` 探测 GPU 能力决定走增强档；`encoding_pool.py` 按机器预算并行多集编码。

## 6a. 运行环境实测构成（文件清单.csv，共 42187 个文件 / ~10.5 GB）

| 组件 | 体积 | 内容 |
|---|---|---|
| libreoffice | 1.47 GB | 成本报表渲染 |
| python | 0.69 GB | CPython + site-packages |
| **torch-cuda** | **4.92 GB** | PyTorch CUDA（STTN 擦除等 GPU 推理） |
| speech-recognition | 1.74 GB | ASR 模型（字幕生成） |
| node / engine | 0.10 / 0.13 GB | 草稿渲染引擎（sidecar 脚本在 `engine\sidecar\`：native_bridge.py 27.7KB、subtitle_removal/remove.py 101KB、finalize.py、title_watermark.py、ai_label.py，均带 test_*） |
| chromium | 0.42 GB | mount 上传 + 渲染 |
| mount-runner | 10 MB | 视频号上传编排 |
| ffmpeg / rubberband | 70 / 6 MB | 音视频 |
| ocr-models / subtitle-sttn / demucs / transnetv2 | 20 / 250 / 80 / 30 MB | OCR / STTN 擦除模型 / 人声分离 / **镜头边界模型** |
| runtime-manifest.json | 9.1 MB | 全组件哈希清单 |

〔已证实，组件 ACQUISITION.json 载明 url+sha256〕**全部 AI 模型都是公开开源权重**：

| 组件 | 模型真身 | 来源 |
|---|---|---|
| subtitle-sttn 250MB | STTN infer_model.pth 66MB + Big-LaMa big-lama.pt 205MB | YaoFANGUK/video-subtitle-remover @ e109b9dd / Sanster models |
| transnetv2 30MB | **TransNetV2 ONNX**（镜头边界检测） | soCzech/TransNetV2 @ 85cef72 |
| ocr-models 20MB | PP-OCRv4 det/cls/rec ONNX | RapidOCR 1.4.4 wheel 抽取 |
| speech-recognition 1.74GB | **faster-whisper-large-v3-turbo**（model.bin 1.6GB） | HF dropbox-dash 镜像 |
| demucs 80MB | htdemucs（955717e8-8726e21a.th 84MB） | Meta facebookresearch |

即上表 `transnetv2 = shotBoundaryModel 真身` 已由 ACQUISITION.json 证实。另证实：ffmpeg 为自编译 7.1.5（x264 静态 + dav1d 1.5.4 + NVENC + OpenCL，`--disable-network`，构建配置在 ffmpeg/BUILD-CONFIG.txt，泄露内部构建路径 `E:/workspace/grove-max/packaging/runtime/inputs/windows-x64/`）；Python 依赖由 uv 0.9.0 锁定（requirements.lock，内部仓库 `studio/grove-plus`，Windows 侧 torch 用 CPU wheel，CUDA 运行时单独放 torch-cuda 组件）。


## 7. 分轨子系统：字幕擦除 + 剪映草稿

### 7a. 字幕擦除（sidecar 源码级证实）

模型与执行流全部来自采集到的 `engine/sidecar/subtitle_removal/` 源码：

**执行流（remove.py，1964 行）〔已证实〕**：
1. 输入 request JSON：`sourcePath/outputPath/ffmpegPath/ffprobePath/modelPath/segments`（kind≠`soft_dialogue` 者为擦除目标，对白软目标不擦）/`sourceOverlayTargets/preserveIntervals/preserveRegions/sceneCutsSeconds/inferencePrecision`（fp16）/`videoEncoder`(auto)/`draftOnly720p/cpuThreads`（L260, 294, 352, 1546-1571, 1673, 1717）。
2. 源指纹校验 `V2_SOURCE_CHANGED`（L1565）、输出冲突 `V2_CLEAN_OUTPUT_CONFLICT`（L1558）——**上游与 sidecar 靠 sha256 互信**。
3. 模型选择：masked 档→LaMa，默认→STTN-auto fp16（L1723 `"algorithm": "lama" if masked_model else "sttn-auto"`）；STTN 网络代码头注 `Vendored from YaoFANGUK/video-subtitle-remover @e109b9dd`（sttn.py L1-2），仅改动 spectral_norm 引用方式。
4. 三线程帧管线：FrameReader 预取 48 帧（L1107）→ FrameCompositor 生成 mask（`sttn_composite_mask` L800、`temporal_stabilize_band` L698 时域稳定）→ ffmpeg rawvideo pipe 交硬件编码器（L1742：`-f rawvideo -pix_fmt bgr24 … -c:a copy -movflags +faststart`）。
5. 质检重试（quality.py）：模板残差 ≥0.20 与暗度 ≥0.40 双独立信号，改善幅度阈值 0.10/0.15（L22-24），不达标按推理窗口重试（`should_retry/accept_retry` L184-193）。
6. 硬件失败整轮回退：nvenc/videotoolbox 失败抛 `HardwareEncoderFailed` → 清显存 → **CPU + libx264 整轮重跑**（L1883-1927）；显存紧张按窗口降精度重试（DeviceModelRunner L1380-1452）。
7. 产物：JSON 结果信封（`sourceFingerprint/cleanFingerprint/inferenceWindows/stabilizedFrames/peakDeviceMemoryBytes` 等，L1870）；stderr 逐阶段进度 JSON（`subtitle_removal_plan/complete/encoder_failed`，L1723/1868/1881）——**这就是探查报告里 stderr「全空」的原因（结构化 JSON 而非文本日志）**。

**finalize.py 真实身份＝字幕重建而非「定稿」〔已证实〕**：把上游识别出的字幕文本重新渲染成贴图叠回视频（`render_captions/caption_patch/bundled_font/overlay_display_frames`，L207-358）；并将「成片前四集的剪映草稿副本」硬链接进 `final-draft-inputs/`（L29-33 注释直接引用桌面端常量 `final_inputs::FINAL_DRAFT_SOURCES_DIRECTORY`，注明「深度去重还不叠字幕贴图，两档都有」）——**擦字幕与重建字幕是可独立启用的两档**。

### 7b. 剪映草稿

- 剪映草稿〔已证实〕：`@grove-max/engine-render` + `draftBuilderScript`，每集一份草稿（干净视频轨/原声音轨/可编辑字幕轨）；剪映安装目录自动探测（截图11 显示 `D:\Program Files\JianyingPro`，版本 11.6.0.14552）；有「草稿分轨版本快照」恢复机制与「剪映工程安装结果与所选草稿目录不一致」校验；`report_package_capture_jianying_screenshots` 要求传入 3、4 集共四个已安装工程截图（报审材料的过程图素材）。Python 侧构建器 `sidecar/jianying_draft/build_draft.py`（117KB，v1/v4 两代并存）与 Node 侧 `engine/dist/jianying-v1~v4.js` 家族对应。

## 8. 报审包与成本报表

- 材料清单（日志对照）：新剧名/简介、剧目海报（AI 生图）、AI 制作证明、过程图、成本报表/成本图、成片；「7 项核心材料与成片已全部完成」。
- AI 能力全部走云端中继〔已证实〕：CSP `connect-src … https://api.grove-max.app https://api.raycaster.app`；`GROVE_MAX_MODEL_RELAY_URL` 环境变量注入 sidecar；文本模型（剧名简介/分镜反推/分集制作清单/资产设定）与生图模型（海报/资产图）均以「开始请求 XX 模型」事件呈现〔日志〕。
- 成本报表渲染〔已证实〕：`cost-report-input.json` → `dist/cost-report-cli.js`（`--input --output --python --libreoffice`，NO_COLOR）→ `cost-report.png`；缺渲染器报 `COST_REPORT_RENDERER_MISSING`「本地运行环境缺少成本报表渲染器，请更新运行环境」。
- 成本数据输入（截图8/9）：公司名、印章（自动生成/上传）、印章编码、总投资额（万元）、演员片酬占比、手写签名板；免责声明四条（本地保存/仅用于报审/严禁伪造/责任自负）。

## 9. 发布挂载（mount 族）＝ 微信视频号自动上传（mount-runner 源码证实）

〔已证实〕mount-runner 是独立 Node 包（内嵌 playwright-core，配 runtime 的 chromium 151.0.7922.77；SOURCE-SNAPSHOT-RECEIPT.json 记录 git revision c9c1b96），CLI 仅两个子命令（cli.js L11-39）：
- `self-check --chromium <path>`：自检 Chromium 可执行、文件上传能力、H.264/AAC 解码、cookie 持久化（check.js L64-108 + playwright-probe.js L43-89）；
- `browser-session --chromium <path> --profile <path>`：常驻会话进程，stdin 收 JSON 命令 `status / upload-one / open`（browser-session.js L62-137）；`upload-one` 契约 = `seriesName/videoPath/description/shortTitle/posterPath`（L62-96）。

视频号操作序列（channels-weixin.js）〔已证实〕：
1. 登录态判定＝打开 `https://channels.weixin.qq.com/` 等待「内容管理」可交互（L77-96）；
2. 发表页＝内容管理 → 视频 → 发表视频，等 URL `/platform/post/create`（L97-122）；
3. 填单：contenteditable 填描述、placeholder 定位短标题、`.post-position-wrap` 选「不显示位置」（L123-157）；
4. **剧集挂载**：打开剧集选择器 → 搜索 `seriesName` → 读 `.drama-row/.drama-title` 匹配选中（L181-251）——上传视频与挂载到剧集是两个独立动作，说明发布走「视频号短剧挂载」形态而非普通投稿；
5. 文件 input 上传 → 等转码/封面生成 → 可选上传 `posterPath` 封面 → 点「发表」等成功（L264-396）。

进度经 `GROVE_MAX_MOUNT_PROGRESS` 通道发 `upload-progress` 事件，终态输出 `published: true` 或错误码 JSON（browser-session.js L244-290）。登录态用 playwright `launchPersistentContext` 复用同一 profile 目录（L138-228），多账号靠不同 profile 隔离。

## 10. 云端服务拓扑与授权体系

| 服务 | 地址 | 用途 | 证据 |
|---|---|---|---|
| 后端 API | `https://api.raycaster.app` | 账号/卡密/设备/错误上报（`/api/auth/ticket`、`/api/client-errors`） | URL+CSP〔已证实〕 |
| 模型中继 | `https://api.grove-max.app` | 文本/生图模型代理（client-config.json modelRelayUrl） | 〔已证实〕 |
| 本地中继 | `http://localhost:3001` | CSP 放行的本地端口（sidecar HTTP 服务）〔推断〕 | CSP 串 |
| 下载 CDN | `https://dl.raycaster.app` + `/api/runtime-downloads/` | 运行环境组件下载 | 〔已证实〕 |
| 应用更新 | 腾讯云 COS `grove-max-downloads-1301443848.cos.ap-guangzhou…/updates/app/internal-beta/` | Tauri updater（internal-beta 通道） | 〔已证实〕 |
| 支付 | `zpayz.cn` / `z-pay.cn` | 卡密/充值（发卡平台） | URL〔已证实〕 |
| 官网 | `https://grove-max.app`（/#downloads）、`gm.raycaster.app` | 官网与账单/账号页 | 〔已证实〕 |

授权体系〔已证实，截图1/13/14/15〕：卡密激活（keyring 存储 `secret_store_*_card_key`，服务名 `app.raycaster.grove-max`）→ session token → `secret_store_installation_id` 设备指纹 → 设备名额（退出解绑弹窗）→ 积分计费（60 积分/次）+ 邀请增长（600 积分互奖）→ 运行环境下载也需有效卡密。

## 11. 日志体系（对前次探查脚本的修正与补充）

1. **无常驻应用日志**〔已证实〕：无 tauri-plugin-log、无文件日志器。
2. **任务状态目录（运行时已证实）**：`<转码工作目录>\.grove\workflows\<32位hex任务ID>\`，实测位于 `D:\下载\.grove\workflows\`；另有 `.grove\projects\<id>\source-materials.json`（素材库项目）。实测两个任务：`696266fa…`（22 集，10-19 21:59，对应 UI 日志那次）与 `6984ab7e…`（51 集，22:42）。
   **修正**：早前「成功后清场」的推断**不成立**——grove-tree 实测显示状态文件全在，是探查脚本模式没覆盖（0 字节的只有 encode-stderr/watermark 日志壳，stderr 空说明错误走结构化文件）。
   实测 V5 工作流产物结构（两任务一致）〔已证实，grove-tree.txt〕：

   ```
   workflows/<id>/
     workflow.json                  任务状态机（核心）
     source-snapshot.json           输入素材快照
     jianying-draft-version.txt     剪映草稿版本
     watermark.png / watermark-result.log / watermark-error.log
     ai-labels.json + ai-label-landscape.png + ai-label-portrait.png   （有 AI 声明的任务）
     cost-report-seal.png + cost-report-signature.png                  （有成本报表的任务）
     pipeline-v5/
       run.lock                     单实例锁
       boundary-request.json        → 向服务端请求分集边界（切点配方）的请求体
       canonical-request.stdout/stderr   服务端签发响应（canonical = 规范化签发）
       analysis-snapshot.json / native-snapshot.json
       analyze-execution.json + analyze.stdout/stderr    镜头扫描编排
       analysis/<N>-command.json + N-metadata.txt + N-errors.txt   每扫描单元的实际命令与元数据（99 个单元）
       analysis/analysis.json + native-analysis.json     扫描结果聚合
       first-encode-execution.json + first-encode.stdout/stderr    第一轮母版编码
       mothers/manifest.json + run.json
       .v5-stage2-<40hex> ×9 / .v5-native-assets-<hash> ×2             阶段暂存资产
       assets/title.png + assets/ai.png    剧名水印 / AI 声明 overlay 图
       overlay-title.stdout/stderr          title_watermark.py 执行
       overlay-ai.stdout/stderr             ai_label.py 执行
       final-encode-execution.json + final-encode.stdout/stderr    第二轮成片编码
       final/manifest.json + run.json + .scan-cache ×73 + final/<N> ×438 文件
   ```

   〔推断〕两轮编码与静态分析的双轮结论互证：first-encode（母版）→ overlay（水印/AI 声明）→ final-encode（成片）。`analysis/99 单元` > 73 集（22+51）说明扫描单元≠集，长素材被切分扫描。
3. **UI 运行日志**〔已证实，日志文件.txt + 截图3 复制日志按钮〕：转码页内嵌「运行日志」面板，事件流形如 `[2026/10/19 21:59:37] 剧集：…`，由后端任务事件推送，用户可一键复制——这就是用户拿到的「日志文件.txt」的来源，**它不落盘为独立文件**。
4. 状态目录命名修正：静态字符串 `.grove/tasks` 与运行时实测 `.grove/workflows` 并存〔已证实〕——tasks〔推断〕为工具箱类短任务，workflows 为生成类长任务。
5. 侧目录：`%APPDATA%\cn.grovemax.review\tracks-draft\runs\`（剪映草稿任务历史）、`%LOCALAPPDATA%\...\runtime\{current, staged-*, verification, runtime-pending-swap.json, selected-components.json}`（运行环境原子交换）、`signature-<sha256>.png` + `cost-report-assets\`（成本报表手写签名，签名文件名即 SHA-256）。

## 12. 可借鉴与避坑（对照本项目「剧备」）

1. **服务端签发切点策略 + 客户端纯执行**（V5）：算法防逆向的典型做法；我们也做重组切点，若需保护配方可借鉴，但要权衡离线可用性。
2. **两轮编码 + 母版续跑 + checkpoint.json**：先产母版保底、昂贵处理断点续跑，比一次性长任务健壮。
3. **rubberband 优先、atempo 链兜底**的变调实现，与任务目录 per-episode 日志命名规范，值得直接吸收。
4. **运行环境全家桶 + 清单哈希校验 + 授权门禁**：我们内置 ffmpeg 的思路更轻；若未来运行时要带 torch/模型，清单校验（verification tmp + manifest hash）是现成范式。
5. **UI 运行日志面板 + 复制日志**：用户反馈问题的成本低（对比我们的「打开日志文件夹」按钮），可补一个可复制的日志面板。
6. **避坑**：CSP 里 `http://localhost:1420` 是开发期残留，生产 CSP 应收紧；错误码族（如 `COMPANY_RECORD_STATE_UNAVAILABLE`）直接把内部符号暴露在用户可见文案旁，建议错误码与文案分层。

## 13. 证据与未知项

- 主要证据：PE 字符串（样本 SHA-256 见探查报告；解包于 `/private/tmp/grovemax-analysis/`）、命令注册表字节区（偏移 33717088–33719500）、tauri context/CSP（偏移 33346903 附近）、MAX1.docx 17 张截图、日志文件.txt（174 行事件流）、**runtime sidecar 第一方源码包（2026-10-20 真机采集，289 文件，本机副本 `/private/tmp/grovemax-analysis/runtime2/`）**。
- 未解项〔待验证〕：① mount 发布入口在 UI 的确切位置（截图未出现，可能在授权后或完成任务后）；② `localhost:3001` 服务的具体协议。其余旧未知项已解决：ASR 引擎＝faster-whisper-large-v3-turbo（speech-recognition/model.json）；STTN 来源＝YaoFANGUK/video-subtitle-remover vendored（sttn.py L1-2），VSR 徽标应即该开源项目名；shotBoundaryModel＝TransNetV2 ONNX（ACQUISITION.json）。
