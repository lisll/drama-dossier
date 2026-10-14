#!/usr/bin/env bash
# 获取 DBNet 与 E2FGVI 权重。
#
# 权重体积大且许可证独立于本项目，因此一律不入库；本脚本只负责「放到正确位置」
# 或「打印准确的获取指引」，不会把下载内容写进 git。
#
# 用法：
#   tools/model_worker/fetch_models.sh detect [目标目录]
#   tools/model_worker/fetch_models.sh status
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODEL_DIR="${MODEL_DIR:-${SCRIPT_DIR}/models}"

status() {
  local det="${MODEL_DIR}/ch_PP-OCRv4_det_infer.onnx"
  echo "模型目录：${MODEL_DIR}"
  if [[ -f "${det}" ]]; then
    echo "  [有] DBNet: ${det}"
  else
    echo "  [缺] DBNet: ${det}"
  fi
  if compgen -G "${MODEL_DIR}/*e2fgvi*" > /dev/null; then
    echo "  [有] E2FGVI 权重"
  else
    echo "  [缺] E2FGVI 权重（见下方说明）"
  fi
}

fetch_detect() {
  mkdir -p "${MODEL_DIR}"
  local dest="${MODEL_DIR}/ch_PP-OCRv4_det_infer.onnx"
  if [[ -f "${dest}" ]]; then
    echo "DBNet 权重已存在：${dest}"
    return 0
  fi

  # 首选：RapidOCR 的 Python 包自带该模型，无需手工下载，也不需要额外的下载源。
  local site=""
  # 允许用 PYTHON 指定解释器（例如 venv 里的 python）
  for py in "${PYTHON:-}" python3 python3.9 python3.10 python3.11 python3.12; do
    [[ -n "${py}" ]] || continue
    if command -v "${py}" > /dev/null 2>&1; then
      site="$("${py}" -c 'import rapidocr_onnxruntime,os;print(os.path.dirname(rapidocr_onnxruntime.__file__))' 2>/dev/null || true)"
    fi
    [[ -n "${site}" ]] && break
  done
  if [[ -n "${site}" && -f "${site}/models/ch_PP-OCRv4_det_infer.onnx" ]]; then
    cp "${site}/models/ch_PP-OCRv4_det_infer.onnx" "${dest}"
    echo "已从 rapidocr-onnxruntime 复制：${dest}"
    return 0
  fi

  # 次选：由调用方提供直链，避免在本脚本里写死可能失效的第三方地址。
  if [[ -n "${DBNET_URL:-}" ]]; then
    echo "从 \$DBNET_URL 下载…"
    if command -v curl > /dev/null 2>&1; then
      curl -fL --retry 3 -o "${dest}" "${DBNET_URL}"
    else
      wget -O "${dest}" "${DBNET_URL}"
    fi
    echo "已下载：${dest}"
    return 0
  fi

  cat >&2 <<'MSG'
未能自动取得 DBNet 权重。二选一：

1) 用 Python 包自带模型（最简单，推荐）：
     python3 -m pip install rapidocr-onnxruntime
     tools/model_worker/fetch_models.sh detect

2) 自行提供直链：
     DBNET_URL=<你的下载地址> tools/model_worker/fetch_models.sh detect

模型来自 PaddleOCR / RapidOCR 的 DBNet 检测模型（PP-OCRv4 det），
体积约 4.7 MB，输入为归一化 RGB 图像，输出为单通道概率图。
MSG
  return 1
}

e2fgvi_hint() {
  cat <<'MSG'

E2FGVI 权重需要手工获取（本项目不代为分发）：

  仓库：https://github.com/mc-e/Enhancing-Video-Inpainting
  权重：见该仓库 README 提供的下载入口（通常为网盘/云盘链接）

下载后放到此处即可被 pipeline 使用：
  tools/model_worker/models/e2fgvi_ckpt.pth

然后用 --e2fgvi-repo / --e2fgvi-ckpt 指给 worker，例如：
  --worker-args tools/model_worker/worker.py,--e2fgvi-repo,<仓库路径>,--e2fgvi-ckpt,tools/model_worker/models/e2fgvi_ckpt.pth

前置条件：需要 torch。参见 requirements-inpaint.txt 的说明。
MSG
}

case "${1:-}" in
  detect) fetch_detect; e2fgvi_hint ;;
  inpaint) mkdir -p "${MODEL_DIR}"; e2fgvi_hint ;;
  status) status ;;
  *)
    echo "用法：$0 detect|inpaint|status" >&2
    exit 2
    ;;
esac
