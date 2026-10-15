#!/usr/bin/env bash
#
# 启用仓库自带的 Git hooks（只改本仓库的 core.hooksPath，不动全局配置）。
#
#   scripts/install-hooks.sh     # 启用
#   git config --unset core.hooksPath   # 取消

set -euo pipefail

cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

git config core.hooksPath .githooks
chmod +x .githooks/pre-commit .githooks/pre-push scripts/check.sh scripts/install-hooks.sh

echo "已启用 Git hooks（core.hooksPath=.githooks）："
echo "  pre-commit  只检查本次改动的相关项（Rust / Web / JSON）"
echo "  pre-push    与 CI 一致的全量检查（含 cargo test）"
echo "临时跳过：SKIP_CHECKS=1 git commit / git push"
