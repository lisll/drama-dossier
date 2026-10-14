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
  echo "==> 初始化并启动 API（Ctrl-C 退出）"
  ADMIN_TOKEN="$("${PYTHON}" -m drama_cloud.cli initdb --admin demo 2>/dev/null | tail -1)"
  echo "管理员令牌：${ADMIN_TOKEN}"
  echo "打开 http://127.0.0.1:8000/ 并把上面的令牌粘进「访问令牌」输入框。"
  "${PYTHON}" -m drama_cloud.cli serve --host 127.0.0.1 --port 8000
fi
