#!/usr/bin/env bash
# 端到端演示：造素材 → 登记 → 创建任务 → 跑 Worker → 质量门禁 → 取产物。
#
#   ./cloud/demo.sh              # 一键跑完（不需要起 HTTP 服务）
#   ./cloud/demo.sh --http       # 另外演示 API + Web 客户端（会临时占用 127.0.0.1:8000）
#   ./cloud/demo.sh --keep       # 保留演示用的临时文件
#
# 仅用于技术学习与自有素材。
set -euo pipefail

CLOUD_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${CLOUD_DIR}/.." && pwd)"
VENV="${REPO_DIR}/.venv"
PYTHON="${VENV}/bin/python"
PIP="${VENV}/bin/pip"

HTTP=0
PASSTHRU=()
for arg in "$@"; do
  case "${arg}" in
    --http) HTTP=1 ;;
    *) PASSTHRU+=("${arg}") ;;
  esac
done

if [[ ! -x "${PYTHON}" ]]; then
  echo "==> 创建虚拟环境 ${VENV}"
  python3.9 -m venv "${VENV}"
  "${PIP}" install --quiet --upgrade pip
  "${PIP}" install --quiet -e "${CLOUD_DIR}[dev]"
fi

export PYTHONPATH="${CLOUD_DIR}/src${PYTHONPATH:+:${PYTHONPATH}}"

echo "==> 运行端到端演示"
"${PYTHON}" -m drama_cloud.cli demo "${PASSTHRU[@]+"${PASSTHRU[@]}"}"

echo
echo "==> 队列概览"
"${PYTHON}" -m drama_cloud.cli status

if [[ "${HTTP}" == "1" ]]; then
  echo
  echo "==> 准备 API 演示"
  # M5b 起认证走 commerce 内省：API 需要一个 commerce 地址与内部密钥，
  # 令牌也从 commerce 登录拿，不再是本地 initdb 打印的那串。
  if [[ -z "${DRAMA_CLOUD_COMMERCE_URL}" || -z "${DRAMA_CLOUD_INTERNAL_SECRET}" ]]; then
    echo "跳过 HTTP 演示：没有配置 commerce。"
    echo "需要这两个变量（值见 cloud/docs/COMMERCE.md）："
    echo "  export DRAMA_CLOUD_COMMERCE_URL=http://127.0.0.1:8100"
    echo "  export DRAMA_CLOUD_INTERNAL_SECRET=<与 commerce 的 DRAMA_COMMERCE_INTERNAL_SECRET 相同>"
    echo "然后用桌面端（或 curl 调 /api/v1/auth/login）登录 commerce 拿访问令牌。"
    exit 0
  fi
  echo "把 commerce 登录得到的访问令牌粘进 http://127.0.0.1:8000/ 的「访问令牌」输入框。"
  "${PYTHON}" -m drama_cloud.cli serve --host 127.0.0.1 --port 8000
fi
