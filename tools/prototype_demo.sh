#!/usr/bin/env bash
# 硬字幕擦除原型的一键体验脚本（学习用）。
#
# 做三件事：
#   1. 造一段带中文硬字幕的测试素材（动态背景 + 宋体字幕）；
#   2. 用「固定区域 + 本地基线」跑通整条链路（不需要任何模型）；
#   3. 若本机能用 DBNet，则再用 DBNet 检测跑一遍。
#
# 用法：
#   tools/prototype_demo.sh [输出目录]        # 默认 /tmp/dossier-inpaint-demo
# 环境变量：
#   PYTHON=<解释器>   指定 Python（需要 numpy；跑 DBNet 还需要 onnxruntime）
set -euo pipefail

OUT_DIR="${1:-/tmp/dossier-inpaint-demo}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKER="${REPO_DIR}/tools/model_worker/worker.py"
PYTHON="${PYTHON:-python3}"
FONT="/usr/share/fonts/chinese/simsun.ttc"
[[ -f "${FONT}" ]] || FONT="/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

command -v ffmpeg > /dev/null || { echo "缺少 ffmpeg；请先安装后重试。" >&2; exit 1; }
command -v cargo  > /dev/null || { echo "缺少 cargo；请先安装 Rust 工具链。" >&2; exit 1; }

mkdir -p "${OUT_DIR}"
SOURCE="${OUT_DIR}/source.mp4"

step "1/4 生成带硬字幕的测试素材"
ffmpeg -v error -f lavfi -i "mandelbrot=size=640x360:rate=25:maxiter=120" \
  -f lavfi -i "sine=frequency=330:duration=3" -t 3 \
  -vf "drawtext=fontfile=${FONT}:text='这是一句测试字幕':fontcolor=white:fontsize=30:x=(w-text_w)/2:y=h-78:borderw=2:bordercolor=black,drawtext=fontfile=${FONT}:text='字幕擦除功能验证':fontcolor=white:fontsize=30:x=(w-text_w)/2:y=h-42:borderw=2:bordercolor=black" \
  -map 0:v -map 1:a -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest -y "${SOURCE}"
echo "素材：${SOURCE}"
echo "字幕大致位于 x=100..540, y=270..350"

step "2/4 构建 CLI"
cargo build -q -p dossier-inpaint
CLI="${REPO_DIR}/target/debug/dossier-inpaint"

step "3/4 固定区域 + 本地基线（无需模型）"
"${CLI}" --input "${SOURCE}" --output "${OUT_DIR}/fixed-roi.mp4" \
  --roi 100,272,440,84 --detector fixed --inpainter column \
  --clip-frames 25 --probe-frames 20 --dilate 4 --feather 3
echo "输出：${OUT_DIR}/fixed-roi.mp4"

step "4/4 DBNet 检测 + 本地基线修复（需要 onnxruntime）"
if PYTHON="${PYTHON}" "${REPO_DIR}/tools/model_worker/fetch_models.sh" detect > /dev/null 2>&1 \
   && "${PYTHON}" -c "import onnxruntime" > /dev/null 2>&1; then
  "${CLI}" --input "${SOURCE}" --output "${OUT_DIR}/dbnet.mp4" \
    --detector worker --inpainter worker \
    --worker-program "${PYTHON}" --worker-args "${WORKER}" \
    --clip-frames 25 --probe-frames 20 --dilate 4 --feather 3
  echo "输出：${OUT_DIR}/dbnet.mp4"
else
  echo "跳过：未安装 onnxruntime 或未取到 DBNet 权重。"
  echo "启用方式："
  echo "  ${PYTHON} -m pip install -r ${REPO_DIR}/tools/model_worker/requirements-detect.txt"
  echo "  PYTHON=${PYTHON} ${REPO_DIR}/tools/model_worker/fetch_models.sh detect"
fi

printf '\n\033[1m完成。\033[0m输出目录：%s\n' "${OUT_DIR}"
echo "用播放器对比 source.mp4 与 fixed-roi.mp4 即可看到字幕被擦除。"
echo "把 --inpainter worker 加上 E2FGVI 权重即可换成时序模型，见 docs/SUBTITLE_REMOVAL.md。"
