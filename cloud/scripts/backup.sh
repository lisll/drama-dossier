#!/usr/bin/env bash
# 备份整套服务（M7-7）。
#
# 备份内容：
#   1. 两个 PostgreSQL 库（commerce 与 media）——账号、卡密、积分、任务、素材元数据；
#   2. 媒体控制面的数据目录（素材、产物、分片上传的临时文件）。
#
# **不备份**签名私钥与 .env：它们不该和备份放在一起（一份泄露就全泄露）。
# 密钥要单独保管，见 docs/KEY_ROTATION.md。
#
# 用法：
#   ./scripts/backup.sh                    # 备份到 ./backups/<时间戳>/
#   ./scripts/backup.sh /path/to/backups   # 指定备份根目录
#
# 恢复见 scripts/restore.sh。

set -euo pipefail

COMPOSE_FILE="${COMPOSE_FILE:-compose.deploy.yaml}"
BACKUP_ROOT="${1:-./backups}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
TARGET="${BACKUP_ROOT}/${STAMP}"

# .env 里的库名与账号；没有就用默认值。
if [[ -f .env ]]; then
  # shellcheck disable=SC1091
  set -a && . ./.env && set +a
fi
POSTGRES_USER="${POSTGRES_USER:-drama}"
COMMERCE_DB="${COMMERCE_DB:-drama_commerce}"
MEDIA_DB="${MEDIA_DB:-drama_media}"

mkdir -p "${TARGET}"

echo "==> 备份到 ${TARGET}"

# --- 1. 两个库 ---
# 用 custom 格式（-Fc）：压缩过、且 restore 时可以只恢复部分表。
for db in "${COMMERCE_DB}" "${MEDIA_DB}"; do
  echo "    导出数据库 ${db}"
  docker compose -f "${COMPOSE_FILE}" exec -T postgres \
    pg_dump -U "${POSTGRES_USER}" -Fc --no-owner --no-acl "${db}" \
    > "${TARGET}/${db}.dump"
  # 立刻确认导出的不是空文件：空的 dump 在恢复时才会暴露，那时已经晚了。
  if [[ ! -s "${TARGET}/${db}.dump" ]]; then
    echo "错误：${db} 的导出是空文件" >&2
    exit 1
  fi
done

# --- 2. 媒体数据目录 ---
echo "    打包媒体数据（素材 / 产物 / 分片）"
docker compose -f "${COMPOSE_FILE}" exec -T media \
  tar -C /data -czf - . > "${TARGET}/media-data.tar.gz"

# --- 3. 校验与清单 ---
echo "    记录校验和"
( cd "${TARGET}" && sha256sum ./* > SHA256SUMS )

cat > "${TARGET}/MANIFEST.txt" <<MANIFEST
备份时间（UTC）：${STAMP}
数据库：${COMMERCE_DB}、${MEDIA_DB}（custom 格式，pg_dump -Fc）
媒体数据：/data（tar.gz，含素材、产物、分片上传）
未包含：签名私钥、.env（刻意不放在备份里，见 docs/KEY_ROTATION.md）
恢复：./scripts/restore.sh ${TARGET}
MANIFEST

echo "==> 完成：${TARGET}"
du -sh "${TARGET}"
