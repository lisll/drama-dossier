# 角标 / 水印擦除（光流引导时序 inpaint）

> 定位：本文与 `crates/dossier-inpaint`、`tools/model_worker` 一样**只用于技术学习**。
> 允许清理**自有素材**的角标、台标与水印；不提供 DRM 绕过能力，也不处理他人作品的
> 版权标识，结果不用于对外分发。硬字幕擦除见 [SUBTITLE_REMOVAL.md](./SUBTITLE_REMOVAL.md)。

复用 [硬字幕擦除](./SUBTITLE_REMOVAL.md) 的同一条管线（`probe` → 掩码 → 检测 → 时序修复 → 回贴 → 合音轨），
差异集中在**掩码参数**、**ROI 取法**和**编码策略**上。

## 为什么角标是「小 mask 场景」的性价比首选

E2FGVI 这类光流引导的时序修复，质量与 mask 占画面的比例强相关：

- **mask 小**：需要重建的像素少，光流能从邻帧搬运到足够的真实像素。
- **位置固定**：一次定位即可整片复用，可以直接跳过逐帧检测（`--detector fixed`）。
- **静止**：角标不随镜头运动，时序一致性天然成立，不容易出现闪烁。

角标通常只占画面 0.1%~1%，正好落在 E2FGVI 最舒服的区间——这是它比硬字幕更容易做好的原因。

## 与硬字幕的形态差异

| 维度 | 硬字幕 | 角标 / 水印 |
| --- | --- | --- |
| 位置 | 底部居中或条状 | 四角，常贴边 |
| 形状 | 扁长（如 640×40） | 接近方形（如 90×60） |
| 颜色 | 多为白/黑单色 | 常带彩色、半透明、有渐变 |
| 边缘 | 抗锯齿柔和 | 硬边、几何图形 |
| 面积 | 大 | 小 |

两个直接后果：**模型窗口要取方形**、**`feather` 要谨慎**（硬边 + 羽化会在 logo 边缘留一圈轮廓）。

## 原理：光流在 1/4 分辨率上传播

E2FGVI 的流程是「光流估计 → 按光流把邻帧像素传播到 mask 区 → 时序融合重建」：

1. **编码器两次 stride-2**，所以特征图与光流传播都在 **1/4 分辨率**上进行。
   640×64 的扁长 ROI 到传播阶段只剩 160×16，纵向信息量不足，修复质量明显下降。
   **角标 ROI 要接近方形**，短边建议 ≥ 192（包含 logo 本身与足够的上下文画面）。
2. **参考帧是离散偏移**：`neighbor_stride=5`、`num_ref=10` 时参考帧相对当前帧的位移只有
   `0, ±5, ±10`。因此 `--clip-frames` 要明显大于 `2 × num_ref`，**默认 30 刚好够用**；
   调小会让片段中段拿不到两侧参考帧。

## 参数：角标专用

### `--dilate` / `--feather` 的规则（最关键的一条）

`feather` 不是「只往外扩」：它是掩码的**方框模糊**，同时向掩码内侧和外侧各扩 `feather` 像素。
膨胀后的掩码边界往里 `feather` 像素处 alpha 才回到 255，而 logo 的真实边缘位于膨胀边界
再往里 `dilate` 像素处。

**规则：`dilate ≥ feather`；工程上推荐 `dilate ≥ feather + 1` 多留 1 px 余量。**

实测（平坦灰底 + 硬边白色角标，ROI `520,20,90,60`，用无损中间态取回精确像素，不经任何有损编码）：

| `--dilate` | `--feather` | 角标框内残留像素 | 最大残留 |
| --- | --- | --- | --- |
| 0 | 3 | 864 | 114 |
| 1 | 3 | 584 | 94 |
| 2 | 3 | 180 | 59 |
| 2 | 2 | 0 | 0 |
| 3 | 0 | 0 | 0 |
| 3 | 3 | 0 | 0 |
| 4 | 0 | 0 | 0 |
| 4 | 3 | 0 | 0 |

读法：`dilate < feather` 时残留恰好是「(feather − dilate) px 宽的 logo 边缘一圈」——
90×60 的框周长 300，`dilate=1, feather=3` 的残留 ≈ 300×2 = 600（实测 584）。
任何组合下**框外像素都没有被改动**：羽化往外扩的那几像素落在平坦背景上，与修复结果一致。

**角标推荐**：硬边 logo 用 `--dilate 3~4 --feather 0~1`；画面有轻微抖动/缩放时再各加 1 px。

### ROI 取法：`--roi` 定「擦哪里」，`--context` 定「模型看多大」

这两个参数很容易搞反，先记住：

- **`--roi` 要紧贴 logo**。它就是**掩码区域**——`--detector fixed` 下 `--roi` 被整体当作检测框，
  掩码 = 这个框向外膨胀 `--dilate`，框有多大就擦多大。
- **`--context`（默认 1.6）才是给模型的上下文**。它把 `--roi` 按中心等比放大成模型输入窗口，
  **放大出来的那圈真实画面不会被擦除**，只是让 E2FGVI 有东西可传播。
- **不要为了留上下文去放大 `--roi`**：那会把正常画面一起送进掩码擦掉，还挤掉了真正的上下文。

两条约束：

1. `--context` 放出来的边距必须 **≥ `dilate + feather`**，否则掩码会贴到 ROI 边界、
   羽化没有过渡带（`PipelineConfig::context_scale` 的注释就是这么写的）。
   扁长的 logo 尤其危险，纵向边距只有 `(height × context − height) / 2`。
2. 贴边角标：`MaskRect::scale` 按中心外扩后**裁到画面内**，画面外那部分直接丢掉，
   上下文退化成单边。应对办法是**调大 `--context`**，让裁掉一半后内侧仍有足够画面。

实测（`with_logo.mp4`，`--roi 520,20,90,60 --dilate 4 --feather 1`）：

| `--context` | 模型窗口 | 掩码占窗口 | 说明 |
| --- | --- | --- | --- |
| `1.0` | 90×60 @ (520,20) | **100%** | 完全没有上下文，整窗都被擦 |
| `1.6` | 144×96 @ (493,2) | 48.2% | 默认值，四周各留 27 / 18 px |
| `2.0` | 165×120 @ (475,0) | 33.7% | 贴边被裁：宽 180→165，高 120 从 y=0 起 |

模型窗口要**接近方形**（理由见上文 1/4 分辨率）——在 `--roi` 紧贴 logo 的前提下，
这件事由 `--context` 和窗口裁剪共同决定，必要时可把 `--roi` 的下沿稍微向下取一点来补高度。

## 压缩伪影：角标比字幕更麻烦

低码率素材的角标区域本身就带**编码块噪声**。inpaint 分不清「这是画面内容还是编码噪声」，
会把噪声当成纹理一起传播进 mask 区，结局是擦除区出现缓慢漂移的块状纹理。

已经做对的：解码是 `-f rawvideo -pix_fmt bgr24`，**无缩放、无滤镜**，不存在二次损失。

拿不掉的：源里**已经烧进去**的伪影无法在解码阶段还原，只能减少「复发」。

| 手段 | 命令 | 说明 |
| --- | --- | --- |
| 无损中间态 | `--intermediate lossless` | 不在已受损的画面上再叠一代损失（唯一已量化的手段，见下节） |
| 保留颗粒 | `--tune grain` | 不让 x264 把擦除区的细颗粒当平坦区抹平；未量化 |
| 提高码率 | `--crf 12~14` | 默认 16 偏保守，修复区对 CRF 比字幕区敏感；未量化 |
| 全色度中间态 | `--pix-fmt yuv444p` | 见「色度」一节，**只建议用于中间态** |

## 两段式编码：`--intermediate delivery` vs `lossless`

管线分两段：**修复段**（把重建后的帧写成中间视频）与**交付段**（合音轨、出最终文件）。

| 模式 | 中间态 | 有损代数 | 适用 |
| --- | --- | --- | --- |
| `delivery`（默认） | 直接编码成交付格式（libx264） | **1 代** | 一次性出片，省磁盘 |
| `lossless` | FFV1 无损（`.mkv`）→ 再单独做一次交付编码 | **1 代** | 需要反复调交付参数，或要留母版 |

实测（`with_logo.mp4`，50 帧 640×360，`--crf 16`）：

- **两种模式的交付输出逐字节一致**（均为 491115 B）。`lossless` 不是「多一代损失」，
  它多出来的是一次**无损**写入。
- 代价是磁盘：无损母版 **7.35 MB** vs 交付文件 **491 KB**（≈15×）。

`lossless` 的真正价值在于**检测 + 修复是最贵的一步**：留下无损母版后，换 CRF / preset /
`--tune` / `--pix-fmt` 重出片不需要重跑 inpaint，也不会再叠一代损失。角标调参通常要来回试很多次，
收益很明显。

```bash
# 一次性出片（默认，最省磁盘）
... --intermediate delivery --crf 14

# 先落无损母版，之后反复重出
... --intermediate lossless --intermediate-pix-fmt gbrp --crf 14 --keep-temp
# 重出：从 <output>.inpaint-tmp/video.mkv 直接编码，无需重跑 inpaint
```

## 色度：`--pix-fmt`

源素材多为 `yuv420p`。解码时色度被上采样到 bgr24，若再编码回 `yuv420p`，就是**第二次色度抽样**。
角标常带彩色，对色度偏移比对亮度更敏感。

实测（高饱和素材，x264 无损 `-crf 0`，与 bgr24 母版逐像素比——即纯粹的第二次抽样代价）：

| 交付 `--pix-fmt` | 最大误差 | 平均绝对误差 | 误差 > 2 的像素占比 |
| --- | --- | --- | --- |
| `yuv420p` | 34 | 1.58 | 21.4% |
| `yuv444p` | 2 | 0.38 | 0% |

`yuv444p` 的误差已经只剩取整级别，代价是体积（无损对比：146 KB vs 92 KB）。
但 **H.264 4:4:4 播放器兼容性差**，所以：**`yuv444p` 只作中间态**，最终交付仍用 `yuv420p`。

## 一个值得记下的坑：BGR→YUV 的隐式转换路径

`--intermediate` 两种模式最初实测出的画质并不一致（`delivery` 相对无损母版 37.85 dB，
`lossless` 39.63 dB，体积还更大）。定位结果：

- libx264 的 BGR→YUV 转换发生在**滤镜图**里，全局 `-sws_flags` **传不进去**（加了完全没变化，
  输出逐字节相同）。
- 默认 flags 的 4:2:0 色度下采样比 `full_chroma_int+accurate_rnd` 粗糙：同一份 bgr24 帧，
  两条转换路径下 Y 相差 ≤1（系统性 +1）、色度最大相差 43。
- 只有显式的 `-vf scale=flags=full_chroma_int+accurate_rnd` 才生效。

修复后两种模式输出逐字节一致，`delivery` 相对无损母版 **39.63 dB**（+1.78 dB），体积也从
505 KB 降到 491 KB。该 flags 已固定在 `crates/dossier-inpaint/src/frames.rs` 的 `encoder_command`。

## 无损中间态可以当「显微镜」用

FFV1 对 8-bit RGB 逐字节无损，两种像素格式实测都成立：

```text
bgr24 → ffv1 -pix_fmt bgr0 → bgr24 : 34560000 bytes 逐字节一致
bgr24 → ffv1 -pix_fmt gbrp → bgr24 : 逐字节一致
```

所以可以 `--intermediate lossless --intermediate-pix-fmt gbrp --keep-temp`，再从
`<output>.inpaint-tmp/video.mkv` 取回**精确的**修复像素来量 mask 参数——
本文的 `dilate/feather` 表就是这么测的。

## 快速开始

```bash
# 1) 本机可跑（无需模型）：固定角标区域 + 本地基线，先把 ROI 与 mask 参数调对
#    --roi 紧贴角标（决定擦哪里），--context 2.0 给模型留上下文（不擦）
cargo run -p dossier-inpaint -- \
  --input in.mp4 --output out.mp4 \
  --roi 520,20,90,60 --context 2.0 \
  --detector fixed --inpainter column \
  --dilate 4 --feather 1

# 2) 完整方案（需 x86_64 + GPU）：留无损母版，之后反复调交付参数不用重跑
cargo run -p dossier-inpaint -- \
  --input in.mp4 --output out.mp4 \
  --roi 520,20,90,60 --context 2.0 \
  --detector fixed --inpainter worker \
  --worker-program python3 \
  --worker-args tools/model_worker/worker.py,--e2fgvi-repo,/path/to/E2FGVI,--e2fgvi-ckpt,/path/to/e2fgvi_ckpt.pth \
  --clip-frames 30 --dilate 4 --feather 1 \
  --intermediate lossless --intermediate-pix-fmt gbrp --keep-temp
```

E2FGVI 需要 torch，本机（aarch64、无 GPU）装不上，只能在 x86_64 + GPU 上端到端验证。
本机能验证的是 CLI、帧 IO、mask、worker 协议与 `column` 基线全链路。

角标场景常用的补充参数：

```bash
# 台标 + 角标两处同时擦（--roi 可重复）
--roi 900,30,120,60 --roi 40,40,100,40 --detector fixed --inpainter column

# 两处标识离得远：按面积阈值拆成多个裁剪作业，各自独立送模型
--region-group-area-ratio 2.0

# 常驻 worker：整段视频只启动一次解释器 + 模型（角标逐帧检测时收益最明显）
--persistent-worker --worker-program python3 \
  --worker-args tools/model_worker/worker.py,--serve,--inpaint-engine,e2fgvi

# 也可选择其他修复后端（需要各自的权重与依赖）：
#   --worker-args tools/model_worker/worker.py,--inpaint-engine,lama
#   --worker-args tools/model_worker/worker.py,--inpaint-engine,propainter

# 只处理片头片尾之间的正片（跳过无角标的片头）
--start 45.0 --duration 1500.0

# 擦除结果自动校验：角标是小面积标识，残留比字幕更容易肉眼看漏
--verify-residual-ratio-fail 0.05 --report-json out.report.json
```

角标擦除的确认流与硬字幕共用同一套实现（残留 / 闪烁 / 接缝 / 一致性四项指标 +
`pass/warn/fail` 退出码 0/3/2），见
[SUBTITLE_REMOVAL.md 的「擦除结果自动校验（确认流）」](./SUBTITLE_REMOVAL.md#擦除结果自动校验确认流)。
小 mask 场景下 `--temporal-smooth`（默认 1 帧）尤其值得保留：角标通常全程静止，
漏检一帧就会在回放里闪一下。

## 已知局限与后续

- **~~第二遍仍是整帧检测 + 整帧送 worker~~（已实现）**：检测后端 `prefer_roi_detection()`
  为真时，第二遍与自动校验都只在 ROI 裁剪图上跑模型，检测框再平移回整帧坐标。
- **~~worker 每帧一次进程启动~~（已实现）**：`--persistent-worker` 让整段视频只启动一次
  worker（`worker.py --serve`）。实测 20 帧样例 8.08 s → 1.14 s（约 7×）。
- **~~多区域仍需一次裁剪~~（可拆分）**：`--region-group-area-ratio`（默认 2.0）会按面积
  阈值把离得远的标识拆成多个裁剪作业，各自独立送模型；两处标识离得远时收益最大。
- **光流传播在 1/4 分辨率**，ROI 短边过小时质量会明显下滑，只能靠取大 ROI 缓解。
- **片段按固定帧长切分**，镜头切换处不切分，片段边界可能出现时序不一致（与硬字幕相同）。
- **压缩伪影只能减复发**，无法还原源中已经丢失的信息。
- **区域分组是贪心启发式**：按 `x,y` 排序后逐个尝试合并，不保证全局最优分组；
  阈值只约束「面积代价」，不感知画面内容（例如被前景物体隔开的两个区域）。
- **worker 修复仍是单掩码。** 逐帧掩码（`Inpainter::inpaint_frames`）只有 `column` 基线
  用上了；E2FGVI / LaMa / ProPainter 走的是并集掩码，改协议才能吃到逐帧掩码的收益。
- **单帧模型（LaMa）没有时序约束**，逐帧调用快但更容易闪；角标静止时可用，
  画面轻微运动时仍应选视频模型。ProPainter 需要模型仓库自带 `dossier_backend.py`。

## 参考

- [SUBTITLE_REMOVAL.md](./SUBTITLE_REMOVAL.md)：硬字幕擦除、架构、worker 交换协议
- `crates/dossier-inpaint/src/mask.rs`：`feather` 实现（方框模糊）与单测
- `crates/dossier-inpaint/src/frames.rs`：`encoder_command` 固定 swscale flags
- `tools/model_worker/inpaint_backends.py`：修复后端适配层（E2FGVI 视频 / LaMa 单帧 /
  ProPainter 外部适配 / 列填充基线），由 `backend` 参数选择
- `crates/dossier-inpaint/src/worker.rs`：常驻 worker 会话（stdio 行协议）
- `crates/dossier-inpaint/src/verify.rs`：擦除结果自动校验（残留/闪烁/接缝/一致性/SSIM）
