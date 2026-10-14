# 硬字幕擦除（DBNet + E2FGVI）

> 定位：本文与 `crates/dossier-inpaint`、`tools/model_worker` 都**只用于技术学习**，
> 只处理自有素材。允许清理自有素材的硬字幕与角标/台标；不提供 DRM 绕过能力，
> 也不处理他人作品的版权标识，结果不用于对外分发。
> 相关背景见 [README 的定位说明](../README.md)、[角标/水印擦除](./WATERMARK_REMOVAL.md)。

目标：把画面底部**固定位置的硬字幕**（烧进画面的字幕）检测出来，用时序模型重建被字幕
挡住的像素，再把结果贴回原视频，同时保持音轨与其他区域原样。

## 为什么是这两个模型

| 环节 | 选型 | 理由 |
| --- | --- | --- |
| 检测 | **DBNet** | 分割式文本检测，对「一行行文字」这种长条目标稳定；ONNX 导出体积小（约 4.7 MB），CPU 就能跑 |
| 修复 | **E2FGVI** | 时序视频 inpainting：利用前后帧的信息补全遮挡区域，比逐帧修复少得多的闪烁与拖影 |

两者都不做「理解文字内容」，只做几何定位与像素重建。

## 整体架构

关键设计是**把「模型推理」和「画面操作」分开**：

- Rust（`crates/dossier-inpaint`）负责编排：ffmpeg 解码/编码、ROI 裁剪、掩码生成与膨胀、
  跨帧聚合、羽化回贴、音轨重封装。它决定「改哪些像素」。
- Python worker（`tools/model_worker`）只负责推理：DBNet 出文本框，E2FGVI 出修复后的 ROI 帧。
  它不决定「贴到哪里」，也不碰整帧。

这样做的收益：模型崩溃不会污染主流程；每一步的中间产物都落在磁盘上、可人工复现；
模型可以用任意语言/框架替换；也让「只改掩码像素」这条不变量集中在 Rust 侧一处实现。

```text
输入视频
   │
   ├─ ffprobe ──────────────► 尺寸 / 帧率 / 是否有音轨
   │
   ├─ 第一遍：定位（probe_frames 帧）
   │    ffmpeg rawvideo ─► 逐帧 DBNet ─► 文本框
   │                                     │
   │                          跨帧聚合 stable_roi
   │                          （丢弃偶发误检，取稳定区域）
   │                                     │
   │                          按 context_scale 外扩 = ROI（给修复模型留上下文）
   │
   └─ 第二遍：修复（按 clip_frames 切片）
        每个片段：裁 ROI ─► 逐帧检测得掩码 ─► 膨胀 ─► E2FGVI 修复 ─► 羽化回贴
                                                              │
                                                    ffmpeg 编码 + 原音轨 copy
                                                              │
                                                          输出视频
```

## 代码位置

| 路径 | 责任 |
| --- | --- |
| `crates/dossier-inpaint/src/probe.rs` | ffprobe 解析（尺寸、帧率、音轨） |
| `crates/dossier-inpaint/src/frames.rs` | bgr24 rawvideo 编解码，ffmpeg 子进程与子进程回收 |
| `crates/dossier-inpaint/src/mask.rs` | 二值掩码、矩形裁剪/外扩、膨胀、羽化、逐像素合成 |
| `crates/dossier-inpaint/src/patch.rs` | ROI 裁剪与回贴 |
| `crates/dossier-inpaint/src/detect.rs` | 检测后端：`FixedRoiDetector` / `WorkerTextDetector`、跨帧 `stable_roi` |
| `crates/dossier-inpaint/src/inpaint.rs` | 修复后端：`ColumnFillInpainter`（基线）/ `WorkerInpainter` |
| `crates/dossier-inpaint/src/pipeline.rs` | 两遍管线与片段调度 |
| `crates/dossier-inpaint/src/main.rs` | CLI |
| `tools/model_worker/worker.py` | worker 入口（读请求 → 跑模型 → 写产物） |
| `tools/model_worker/dbnet.py` | DBNet ONNX 推理与概率图后处理 |
| `tools/model_worker/inpaint_backends.py` | E2FGVI 适配层 + 列填充基线 |
| `tools/model_worker/protocol.py` | 交换协议常量与校验 |

## 用法

```bash
# 先用固定区域 + 本地基线跑通链路（不需要任何模型）
cargo run -p dossier-inpaint -- \
  --input in.mp4 --output out.mp4 \
  --roi 120,300,400,40 --detector fixed --inpainter column

# 用 DBNet 检测 + 列填充修复
cargo run -p dossier-inpaint -- \
  --input in.mp4 --output out.mp4 \
  --detector worker --inpainter column \
  --worker-program python3 --worker-args tools/model_worker/worker.py

# 完整方案：DBNet + E2FGVI
cargo run -p dossier-inpaint -- \
  --input in.mp4 --output out.mp4 \
  --detector worker --inpainter worker \
  --worker-program python3 \
  --worker-args tools/model_worker/worker.py,--e2fgvi-repo,/path/to/E2FGVI,--e2fgvi-ckpt,/path/to/e2fgvi_ckpt.pth \
  --clip-frames 30 --probe-frames 60 --dilate 4 --feather 2 --context 1.6
```

常用参数：

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `--detector` | `fixed` | `fixed`（需 `--roi x,y,w,h`）或 `worker` |
| `--inpainter` | `column` | `column`（本地基线）或 `worker` |
| `--clip-frames` | `30` | 每个片段的帧数；E2FGVI 建议 ≥ 10（`num_ref` 默认 10） |
| `--probe-frames` | `60` | 定位阶段抽检帧数 |
| `--dilate` | `4` | 掩码膨胀半径，覆盖描边与抗锯齿 |
| `--feather` | `2` | 回贴羽化半径，避免边界硬切 |
| `--context` | `1.6` | ROI 相对检测框的外扩比例，给模型留上下文 |
| `--min-score` | `0.5` | 检测置信度下限 |
| `--keep-temp` | 关 | 保留中间视频与 worker 交换目录，便于排查 |
| `--fps` | 自动 | 素材帧率不可信时手动指定（管线按固定帧率处理） |
| `--intermediate` | `delivery` | `delivery` 直接出片；`lossless` 先落 FFV1 无损母版再单独交付编码 |
| `--intermediate-pix-fmt` | `bgr0` | 无损中间态的像素格式（`gbrp` 可做到逐字节无损） |
| `--preset` | `medium` | x264 preset（无损中间态不使用） |
| `--tune` | 无 | 编码 tune，角标场景常用 `grain` 保留颗粒 |
| `--pix-fmt` | `yuv420p` | 交付像素格式；`yuv444p` 仅建议用于中间态 |
| `--crf` | `16` | 编码 CRF；`0` 为 x264 无损 |

`--intermediate lossless` 让「检测 + 修复」这一最贵步骤只跑一次：之后换 CRF / preset /
`--tune` / `--pix-fmt` 重出片不必重跑 inpaint。两条路径的交付输出实测逐字节一致，
代价只是磁盘（无损母版约为交付文件的 15 倍）。角标场景的完整参数表见
[WATERMARK_REMOVAL.md](./WATERMARK_REMOVAL.md)。

## worker 交换协议

两边通过 `work_dir` 里的文件交换，协议实现见 `tools/model_worker/protocol.py`。

**检测**（`mode = "detect"`）

```text
输入：in/frame.raw     整帧 bgr24，长度 = width*height*3
      request.json     {"mode","width","height","frameFile","outputFile"}
输出：boxes.json        {"boxes":[{"x","y","width","height","score"}]}
```

传整帧而不是 ROI，是因为 DBNet 是逐帧检测的，位置抖动需要 Rust 侧跨帧聚合来处理。

**修复**（`mode = "inpaint"`）

```text
输入：in/frame_%04d.raw  ROI 尺寸 bgr24，从 frame_0000 连续编号
      mask.pgm           P5 灰度，255 = 需要修复，0 = 保留
      request.json       {"mode","width","height","frameCount","inputDir","outputDir","maskFile"}
输出：out/frame_%04d.raw 帧数与字节数必须与输入完全一致
```

约定：颜色通道统一 **BGR**（对齐 ffmpeg `bgr24`）；坐标是整帧像素坐标且必须落在画面内；
硬字幕在片段内位置固定，因此 T 个时刻共用同一张掩码。

## 几条关键不变量

1. **只改掩码覆盖的像素。**
   E2FGVI 这类模型会重建整个 ROI，因此「洞外的像素是否被改写」不能靠模型自觉。
   回贴时用 `feather(mask)` 作为逐像素 alpha，只有掩码覆盖处（含羽化过渡带）才写入；
   羽化带之外逐字节保留原值。
   这条由 `pipeline::tests::only_mask_region_changes_on_textured_background` 用
   「会改写整个 ROI 的假模型 + 渐变背景」逐字节守住——去掉 alpha 兜底该测试就会失败。
2. **ROI ≠ 掩码。** ROI 按 `context_scale` 外扩，是喂给模型的上下文窗口；
   掩码只覆盖检测到的文字（再做 `dilate` 膨胀）。
3. **跨帧聚合。** 单帧偶发误检不应扩大掩码，`stable_roi` 用多帧一致性过滤。
4. **帧数守恒。** 输出帧数与输入一致；`out/` 缺帧或尺寸不符时 Rust 侧直接报错，而不是静默输出坏帧。
5. **音轨不重新编码。** 视频重编码后与原素材用 `-c copy` 重新封装音轨。

## 环境与算力现实（实测）

测试机：openEuler 22.03 / **aarch64 / 无 GPU / 无 CUDA**，32 核 CPU，ffmpeg 4.2.4。

- **DBNet 可行。** `onnxruntime` 官方提供 `manylinux2014_aarch64` wheel，CPU EP 直接跑通，
  对真实文字（含中文）置信度约 0.96–0.97。
- **E2FGVI 在本机不可行。** 它依赖 PyTorch，而 aarch64 Linux 没有官方 torch wheel。
  要跑完整方案需要一台带 GPU 的 x86_64 机器（或自行解决 aarch64 的 torch 构建）。

实测耗时（aarch64 CPU）：

| 操作 | 耗时 |
| --- | --- |
| DBNet 单帧 640×360 | 约 172 ms |
| DBNet 单帧 1080p（长边缩到 ≤960） | 约 389 ms |
| DBNet 单帧 ROI 640×64 | 约 30 ms |
| DBNet 单帧 ROI 439×111 | 约 41 ms |
| 列填充基线 ROI 439×111 单帧 | 约 15 ms |
| worker 单次进程调用（含 `import onnxruntime` 约 0.21 s） | 约 0.6 s |

由此得到两条重要结论：

- **第二次遍历应只在 ROI 上检测。** ROI（如 439×111，约 41 ms）比全帧 1080p（约 389 ms）
  便宜近 10 倍，而且位置已经确定，没有理由每帧再扫全图。
  当前实现为了简单仍在第二遍检测整帧，这是首要的优化点。
- **每次调用都起一个新进程代价很高。** 一次 75 帧、640×360 的完整运行实测约 61 s，
  其中绝大部分是 ~98 次 worker 进程启动（0.6 s × 98 ≈ 59 s），而不是模型本身。
  按 1080p 补番 45 分钟估算，逐帧起进程会让单集耗时达到数小时量级。

## 已知局限

- **列填充基线会留竖向拖影。** 它逐列取上方背景的中位数，纹理/渐变背景上会出现竖条
  （见「实测记录」的对比图）。它的定位是「没有模型时把链路跑通」的参照，不是质量方案。
- **整帧重编码带来全画幅噪声。** 目前是「解码整帧 → 修改掩码区 → 整帧重编码」，
  所以掩码之外的像素虽然**逐字节未被管线改写**，但重编码后仍会与原始像素有差异
  （实测与「纯重编码基准」同级：锐利边缘上约 80–90 个像素差 >24，最大约 85）。
  若要进一步逼近无损，可以只重编码受影响的区域再拼接，或改用无损中间格式。
- **E2FGVI 路径未在本机端到端验证。** 适配层的张量变换、补边裁回、「只在洞内合成」
  都用假模型离线单测，但权重本身在此环境跑不起来。
- **薄 ROI 对时序模型不友好。** 字幕条常只有 40–60 px 高，而 E2FGVI 在较大空间尺度上
  训练；实践中应把 ROI 高度留足（`--context` 调大），不要只框住文字本身。
- **一条 ROI 只能覆盖一个位置。** 字幕位置跳动、多行不同位置的字幕目前会合并成一个
  大 ROI，代价是修复范围变大。
- **硬字幕与镜头运动同时存在时**，逐片段修复在片段边界可能出现时序不一致。

## 后续工作（按收益排序）

1. **第二遍只在 ROI 内检测**，把检测代价降到约 1/10。
2. **减少 worker 进程启动开销**：改成一次运行常驻一个 worker，或让单次调用批量处理多帧。
3. **掩码细化**：现在用「矩形外接 + 膨胀」，可换成 DBNet 收缩轮廓 + unclip，
   贴合斜体/描边字幕，减少无谓修复面积。
4. **区域化重编码**，消除掩码外的重编码差异。
5. **在 GPU 机器上验证 E2FGVI**，并比较不同 `clip_frames` / `num_ref` 的时序稳定性。
6. **质量评估**：构造「有字幕 / 无字幕」成对素材，用 PSNR/SSIM 在字幕区域内量化修复质量，
   而不是只靠肉眼。

## 实测记录（可复现）

```bash
# 1) 造一段带中文硬字幕的测试素材（mandelbrot 动态背景 + 宋体两行字幕）
F=/usr/share/fonts/chinese/simsun.ttc
ffmpeg -v error -f lavfi -i "mandelbrot=size=640x360:rate=25:maxiter=120" \
  -f lavfi -i "sine=frequency=330:duration=3" -t 3 \
  -vf "drawtext=fontfile=$F:text='这是一句测试字幕':fontcolor=white:fontsize=30:x=(w-text_w)/2:y=h-78:borderw=2:bordercolor=black,drawtext=fontfile=$F:text='字幕擦除功能验证':fontcolor=white:fontsize=30:x=(w-text_w)/2:y=h-42:borderw=2:bordercolor=black" \
  -map 0:v -map 1:a -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest -y cnsub.mp4

# 2) 取 DBNet 权重（来自 rapidocr-onnxruntime 自带模型）
PYTHON=python3 tools/model_worker/fetch_models.sh detect

# 3) 完整链路：DBNet 检测 + worker 修复
cargo run -p dossier-inpaint -- \
  --input cnsub.mp4 --output cnfixed.mp4 \
  --detector worker --inpainter worker \
  --worker-program python3 --worker-args tools/model_worker/worker.py \
  --clip-frames 25 --probe-frames 20 --dilate 4 --feather 3 --context 1.8
```

观察到的结果：

- DBNet 定位到字幕区域 `439x111 @ (101,249)`，两行中文字幕被完整框住；
- 75 帧全部处理，输出帧数与音轨保持；
- 字幕文字被完全移除，掩码之外的画面**逐字节未被改写**；
- 列填充基线的代价是背景上出现竖向拖影——这正是需要换成 E2FGVI 的原因。

单元测试：

```bash
cargo test -p dossier-inpaint                              # Rust 侧 43 个
cargo clippy -p dossier-inpaint --all-targets -- -D warnings
cd tools/model_worker && python3 -m unittest discover -s tests -v   # Python 侧 30 个
```

## 使用边界

- 只处理**自有素材**；允许清理自有素材的硬字幕与角标/台标，但不用于去除他人作品的
  水印、台标、版权标识，也不做 DRM 绕过。角标/水印擦除见 [WATERMARK_REMOVAL.md](WATERMARK_REMOVAL.md)。
- 擦除字幕不改变内容的权属，也不替代必要的授权与审核。
- 模型权重来自第三方，许可证与本项目无关，使用前请自行确认。
