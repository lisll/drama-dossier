#!/usr/bin/env bash
# 从备份恢复（M7-7）。
#
# **这是破坏性操作**：会把两个库的现有数据替换成备份里的内容，
# 媒体数据目录也会被覆盖。所以要求显式确认。
#
# 用法：
#   ./scripts/restore.sh backups/20261018T101500Z          # 交互确认
#   ./scripts/restore.sh backups/20261018T101500Z --yes    # 脚本里用
#
# 演练建议：先在一个独立 compose 项目上跑（--project-name），
# 确认恢复流程可用再对正式实例操作。

set -euo pipefail

COMPOSE_FILE="${COMPOSE_FILE:-compose.deploy.yaml}"
SOURCE="${1:?用法：./scripts/restore.sh <备份目录> [--yes]}"
CONFIRM="${2:-}"

if [[ ! -d "${SOURCE}" ]]; then
  echo "错误：备份目录不存在：${SOURCE}" >&2
  exit 1
fi

# 校验完整性：备份文件被截断或改过的话，恢复出来的是坏数据，
# 而「恢复成功但数据是坏的」比恢复失败更危险。
if [[ -f "${SOURCE}/SHA256SUMS" ]]; then
  echo "==> 校验备份文件完整性"
  ( cd "${SOURCE}" && sha256sum -c SHA256SUMS )
else
  echo "警告：备份里没有 SHA256SUMS，跳过完整性校验" >&2
fi

if [[ -f .env ]]; then
  # shellcheck disable=SC1091
  set -a && . ./.env && set +a
fi
POSTGRES_USER="${POSTGRES_USER:-drama}"
COMMERCE_DB="${COMMERCE_DB:-drama_commerce}"
MEDIA_DB="${MEDIA_DB:-drama_media}"

if [[ "${CONFIRM}" != "--yes" ]]; then
  echo
  echo "即将用 ${SOURCE} 覆盖以下内容，**现有数据会丢失**："
  echo "  数据库：${COMMERCE_DB}、${MEDIA_DB}"
  echo "  媒体数据：/data（素材、产物、分片）"
  echo
  read -r -p "确认继续？输入 yes：" answer
  if [[ "${answer}" != "yes" ]]; then
    echo "已取消"
    exit 1
  fi
fi

# 先停应用层：恢复期间不该有进程在写库或写数据目录。
echo "==> 停 commerce 与 media（保留 postgres）"
docker compose -f "${COMPOSE_FILE}" stop commerce media

for db in "${COMMERCE_DB}" "${MEDIA_DB}"; do
  dump="${SOURCE}/${db}.dump"
  [[ -f "${dump}" ]] || { echo "错误：缺少 ${dump}" >&2; exit 1; }
  echo "==> 恢复数据库 ${db}"
  # --clean --if-exists：先删同名对象再建，避免「表已存在」导致恢复半途而废。
  docker compose -f "${COMPOSE_FILE}" exec -T postgres \
    pg_restore -U "${POSTGRES_USER}" --clean --if-exists --no-owner --no-acl \
    -d "${db}" < "${dump}"
done

echo "==> 恢复媒体数据"
# 先清空 /data 再解包：不清理会留下「备份里没有、但硬盘上还有」的孤儿文件，
# 那样恢复出来的状态和备份时不一致。
#
# 用 `docker compose cp` 把压缩包送进容器，而不是用 `run -v` 挂宿主目录：
# 挂载路径在 `run` 后面语义容易写错，而且 SOURCE 是绝对路径时拼接也会出错。
docker compose -f "${COMPOSE_FILE}" cp "${SOURCE}/media-data.tar.gz" media:/tmp/restore-media.tar.gz
docker compose -f "${COMPOSE_FILE}" run --rm --no-deps --entrypoint sh media -c \
  'rm -rf /data/* && tar -C /data -xzf /tmp/restore-media.tar.gz && rm -f /tmp/restore-media.tar.gz'

echo "==> 启动服务"
docker compose -f "${COMPOSE_FILE}" up -d --wait

echo
echo "==> 恢复完成。请逐项确认："
echo "  1. commerce 健康检查：curl -k https://<host>/api/v1/health"
echo "  2. 管理后台能登录，用户数与备份前一致"
echo "  3. 抽一个用户，核对积分余额与流水求和一致"
echo "  4. 媒体控制面能列出一个已有素材"
