# model_worker

`dossier-inpaint` 的模型侧 worker：**DBNet 文本检测** + **E2FGVI 时序视频修复**。

Rust 侧负责编排、掩码、ROI 裁剪、帧回贴与羽化；这里只做「读请求 → 跑模型 → 写产物」。
两边通过 `work_dir` 里的文件交换，协议见 `protocol.py` 顶部，好处是崩溃隔离、
可离线复现、也便于单独调试模型。

## 运行

worker 由 Rust 侧调用，不需要手工执行：

```bash
cargo run -p dossier-inpaint -- \
  --input in.mp4 --output out.mp4 \
  --detector worker --inpainter worker \
  --worker-program python3 \
  --worker-args tools/model_worker/worker.py
```

单独调试某一步时可以直接跑：

```bash
python3 worker.py <work_dir>                       # mode 取自 request.json
python3 worker.py <work_dir> --mode detect         # 强制按检测解释
python3 worker.py <work_dir> --detect-engine dbnet --dbnet-model models/ch_PP-OCRv4_det_infer.onnx
```

## 引擎选择

| 环节 | `--*-engine` | 依赖 | 说明 |
| --- | --- | --- | --- |
| 检测 | `dbnet` | `onnxruntime` | 正式方案：DBNet 概率图 + 连通域后处理 |
| 检测 | `bright-rows` | `numpy` | 调试用启发式：找「整行高亮」的连续行带 |
| 修复 | `e2fgvi` | `torch` + 权重 | 正式方案：时序视频 inpainting |
| 修复 | `column` | `numpy` | 基线：逐列取掩码外邻行中位数，与 Rust 侧同语义 |

E2FGVI 相关参数：`--e2fgvi-repo` / `--e2fgvi-ckpt` 指仓库与权重，`--e2fgvi-num-ref`（默认 10）
控制参考帧数；`neighbor_stride` 固定为 5，于是参考帧相对当前帧的位移是 `0, ±5, ±10`。
适配层用 `inspect.signature` 探测模型 `forward` 的调用约定（`e2fgvi_hq` 等版本只接受
`(frames, masks)`），并把实际约定记进 `E2fgviInpainter.model_signature`。

`auto`（默认）优先用真实模型，依赖或权重缺失时降级到基线并打印告警；
显式指定 `dbnet` / `e2fgvi` 时缺失即报错退出，不会静默降级。

> `bright-rows` 只是为了让整条链路在没有模型时也能跑通与自测。
> 它假设「字幕整行高亮」，遇到半透明字幕、花体字、亮背景就会失效。

## 获取模型

权重体积大、许可证独立于本项目，**一律不入库**（见仓库根 `.gitignore`）。

```bash
tools/model_worker/fetch_models.sh detect     # DBNet
PYTHON=/path/to/venv/bin/python tools/model_worker/fetch_models.sh detect
tools/model_worker/fetch_models.sh inpaint    # E2FGVI 指引
tools/model_worker/fetch_models.sh status
```

- **DBNet**：脚本优先从本地已安装的 `rapidocr-onnxruntime` 包里复制
  `ch_PP-OCRv4_det_infer.onnx`（约 4.7 MB）到 `models/`；也可用 `DBNET_URL=<直链>` 自行下载。
  worker 在未显式指定 `--dbnet-model` 时会自动使用 `models/` 下的这个约定路径。
- **E2FGVI**：仓库 <https://github.com/mc-e/Enhancing-Video-Inpainting>，
  权重见该仓库 README 的下载入口；放到 `models/` 后用 `--e2fgvi-repo` / `--e2fgvi-ckpt` 指给 worker。

## 依赖

```bash
python3 -m pip install -r requirements-detect.txt    # onnxruntime + numpy
python3 -m pip install -r requirements-inpaint.txt   # torch（需要 GPU 机器）
```

要求 **Python ≥ 3.8**（用到 `dataclasses` 与 `from __future__ import annotations`）。

## 测试

```bash
cd tools/model_worker && python3 -m unittest discover -s tests -v
```

- 只依赖 numpy 即可跑；装了 onnxruntime 与 DBNet 权重时，会额外跑一个真实模型冒烟测试，
  否则自动跳过。
- E2FGVI 权重无法在开发机/CI 获取，因此适配层用一个「假模型」验证张量变换、
  补边裁回、以及「只在洞内合成」这几条不变量——真正不可验证的只剩权重本身。

## 协议要点（容易踩的地方）

- 颜色通道统一 **BGR**（与 ffmpeg `bgr24` 对齐），写回时不要改成 RGB。
- 修复请求的 `out/frame_%04d.raw` **帧数与字节数必须与输入完全一致**，
  尺寸不同 Rust 侧会直接报错。
- 坐标是整帧像素坐标，必须落在画面内；越界的框会被裁掉或被丢弃。
- 硬字幕在片段内位置固定，因此 T 个时刻共用同一张掩码。

## 角标 / 水印场景

同一条链路，但跳过检测、直接给 E2FGVI 一个接近方形的模型窗口（`--detector fixed`）：

- `--roi x,y,w,h` 要**紧贴角标**——它就是掩码区域，框有多大就擦多大。
- `--context` 才是给模型的上下文：按中心等比放大 `--roi` 作为模型窗口，放大出来那圈**不会被擦**。
  默认 1.6，贴边角标建议调到 2.0 左右。**不要**靠放大 `--roi` 去留上下文。
- 编码器两次 stride-2，光流传播在 **1/4 分辨率**上进行，所以模型窗口短边建议 ≥ 192；
  窗口过扁时修复质量会明显下降。
- `--e2fgvi-num-ref 10` 时 `--clip-frames` 必须明显大于 20，默认 30 刚好够用。
- 掩码规则 `--dilate ≥ --feather`（推荐 +1）；`feather` 是方框模糊，会**向内**也扩 `feather` 像素。

完整参数表、压缩伪影对策与实测数据见
[docs/WATERMARK_REMOVAL.md](../../docs/WATERMARK_REMOVAL.md)。
