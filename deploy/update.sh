#!/bin/bash
# =============================================================================
# MailPool 自动更新脚本
#
# 用途：拉取 Docker Hub 上最新的 1.0 镜像，重建容器。
#       由 GitHub Actions 在推送成功后通过 SSH 调用，也可手动执行。
#
# 用法：
#   bash /opt/mailpool/update.sh              # 更新到 1.0
#   bash /opt/mailpool/update.sh latest       # 更新到 latest
#   IMAGE=zhoushu1/mailpool TAG=1.0 bash /opt/mailpool/update.sh
#
# 幂等：重复执行只会拉取镜像并重启容器，数据目录不受影响。
# =============================================================================
set -euo pipefail

# ---------------------------------------------------------------------------
# 配置（可用环境变量覆盖）
# ---------------------------------------------------------------------------
IMAGE="${IMAGE:-zhoushu44/mailpool}"
TAG="${1:-${TAG:-1.0}}"
CONTAINER="${CONTAINER:-mailpool}"
HOST_PORT="${HOST_PORT:-18090}"
DATA_DIR="${DATA_DIR:-/opt/mailpool/data}"
# 管理口令必须由环境变量提供，不要再往仓库里写默认值：
#   export MAILPOOL_ADMIN_PASSWORD='<你的口令>'
ADMIN_PASSWORD="${MAILPOOL_ADMIN_PASSWORD:?请先设置环境变量 MAILPOOL_ADMIN_PASSWORD}"
BACKUP_KEEP="${BACKUP_KEEP:-7}"

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"; }

log "=== MailPool 更新开始 ==="
log "镜像: ${IMAGE}:${TAG}"
log "容器: ${CONTAINER}  端口: ${HOST_PORT}  数据: ${DATA_DIR}"

# ---------------------------------------------------------------------------
# 1. 前置检查
# ---------------------------------------------------------------------------
if ! command -v docker >/dev/null 2>&1; then
    log "❌ 未安装 docker"
    exit 1
fi

if [ ! -d "$DATA_DIR" ]; then
    log "⚠️  数据目录不存在，创建: $DATA_DIR"
    mkdir -p "$DATA_DIR"
fi

# ---------------------------------------------------------------------------
# 2. 备份数据库（保留最近 N 份）
# ---------------------------------------------------------------------------
if [ -f "$DATA_DIR/mailpool.db" ]; then
    BACKUP="$DATA_DIR/mailpool.db.bak-$(date +%Y%m%d-%H%M%S)"
    cp "$DATA_DIR/mailpool.db" "$BACKUP"
    log "✅ 已备份数据库: $(basename "$BACKUP")"

    # 清理旧备份
    ls -1t "$DATA_DIR"/mailpool.db.bak-* 2>/dev/null | tail -n +$((BACKUP_KEEP + 1)) | while read -r old; do
        rm -f "$old"
        log "   清理旧备份: $(basename "$old")"
    done
else
    log "ℹ️  首次部署，无数据库可备份"
fi

# ---------------------------------------------------------------------------
# 3. 拉取镜像
# ---------------------------------------------------------------------------
log "拉取镜像..."
if ! docker pull "${IMAGE}:${TAG}"; then
    log "❌ 拉取失败（检查镜像名/标签是否正确，仓库是否公开）"
    exit 1
fi

NEW_ID=$(docker image inspect "${IMAGE}:${TAG}" --format '{{.Id}}' 2>/dev/null || echo "")
log "✅ 镜像就绪: ${NEW_ID:0:19}"

# ---------------------------------------------------------------------------
# 4. 记录旧容器状态（用于回滚判断）
# ---------------------------------------------------------------------------
OLD_ID=""
if docker inspect "$CONTAINER" >/dev/null 2>&1; then
    OLD_ID=$(docker inspect "$CONTAINER" --format '{{.Image}}' 2>/dev/null || echo "")
    log "旧容器镜像: ${OLD_ID:0:19}"
fi

# ---------------------------------------------------------------------------
# 5. 重建容器
# ---------------------------------------------------------------------------
log "停止旧容器..."
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true

log "启动新容器..."
docker run -d \
    --name "$CONTAINER" \
    --restart unless-stopped \
    -p "${HOST_PORT}:18080" \
    -e TZ=Asia/Shanghai \
    -e MAILPOOL_DATA=/app/data \
    -e MAILPOOL_HOST=0.0.0.0 \
    -e MAILPOOL_PORT=18080 \
    -e MAILPOOL_ADMIN_PASSWORD="$ADMIN_PASSWORD" \
    -v "${DATA_DIR}:/app/data" \
    --health-cmd 'curl -fsS -o /dev/null http://127.0.0.1:18080/login || exit 1' \
    --health-interval 30s \
    --health-timeout 10s \
    --health-retries 3 \
    --health-start-period 30s \
    "${IMAGE}:${TAG}" >/dev/null

log "✅ 容器已启动"

# ---------------------------------------------------------------------------
# 6. 等待健康
# ---------------------------------------------------------------------------
log "等待健康检查（最多 90 秒）..."
HEALTHY=0
for i in $(seq 1 30); do
    sleep 3
    STATUS=$(docker inspect "$CONTAINER" --format '{{.State.Health.Status}}' 2>/dev/null || echo "unknown")
    if [ "$STATUS" = "healthy" ]; then
        HEALTHY=1
        log "✅ 容器健康（${i}0 秒内）"
        break
    fi
    if [ "$STATUS" = "unhealthy" ]; then
        log "❌ 容器不健康，日志："
        docker logs --tail 40 "$CONTAINER" 2>&1
        break
    fi
done

# ---------------------------------------------------------------------------
# 7. 功能探活
# ---------------------------------------------------------------------------
if [ "$HEALTHY" = "1" ]; then
    log "功能探活..."
    LOGIN=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 "http://127.0.0.1:${HOST_PORT}/login" || echo "000")
    INFO=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 "http://127.0.0.1:${HOST_PORT}/api/product/info" || echo "000")
    log "  /login            → HTTP $LOGIN"
    log "  /api/product/info → HTTP $INFO"

    if [ "$LOGIN" = "200" ] && [ "$INFO" = "200" ]; then
        log "=== ✅ 更新完成 ==="
        docker ps --filter "name=$CONTAINER" --format "  {{.Names}} | {{.Status}} | {{.Ports}}"
        exit 0
    fi
    log "⚠️  探活未全部通过，但容器已启动，请手动检查"
    exit 0
fi

# ---------------------------------------------------------------------------
# 8. 失败：尝试回滚到旧镜像
# ---------------------------------------------------------------------------
log "=== ❌ 更新失败 ==="
if [ -n "$OLD_ID" ]; then
    log "尝试回滚到旧镜像 ${OLD_ID:0:19}..."
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    docker run -d \
        --name "$CONTAINER" \
        --restart unless-stopped \
        -p "${HOST_PORT}:18080" \
        -e TZ=Asia/Shanghai \
        -e MAILPOOL_DATA=/app/data \
        -e MAILPOOL_ADMIN_PASSWORD="$ADMIN_PASSWORD" \
        -v "${DATA_DIR}:/app/data" \
        "$OLD_ID" >/dev/null && log "✅ 已回滚到旧镜像"
else
    log "无旧镜像可回滚（首次部署失败）"
fi
exit 1
