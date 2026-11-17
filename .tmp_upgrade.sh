#!/bin/bash
# MailPool 升级：备份 → 重建容器 → 健康检查 → 失败自动回滚
set -u

IMAGE="${IMAGE:-zhoushu1/mailpool}"
TAG="${TAG:-1.0}"
CONTAINER=mailpool
HOST_PORT=18090
DATA_DIR=/opt/mailpool/data
ADMIN_PASSWORD=<ADMIN_PASSWORD>
TS=$(date +%Y%m%d-%H%M%S)

log() { echo "[$(date '+%F %T')] $*"; }

log "=== MailPool 升级开始 ==="
log "镜像: ${IMAGE}:${TAG}"

# ---- 1. 备份数据库 ----
if [ -f "$DATA_DIR/mailpool.db" ]; then
  cp -a "$DATA_DIR/mailpool.db" "$DATA_DIR/mailpool.db.bak-prebilling-${TS}"
  log "已备份: mailpool.db.bak-prebilling-${TS}"
fi

# ---- 2. 记录旧镜像（回滚用）----
OLD_ID=$(docker inspect "$CONTAINER" --format '{{.Image}}' 2>/dev/null || echo "")
log "旧镜像: ${OLD_ID:0:19}"

# ---- 3. 拉新镜像 ----
log "拉取镜像..."
if ! docker pull "${IMAGE}:${TAG}"; then
  log "拉取失败，退出（不动现有容器）"
  exit 1
fi
NEW_ID=$(docker image inspect "${IMAGE}:${TAG}" --format '{{.Id}}' 2>/dev/null || echo "")
log "新镜像: ${NEW_ID:0:19}"

if [ "$OLD_ID" = "$NEW_ID" ]; then
  log "镜像没变，无需重建"
else
  log "重建容器..."
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
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
      --health-interval 30s --health-timeout 10s --health-retries 3 \
      --health-start-period 30s \
      "${IMAGE}:${TAG}" >/dev/null
  log "容器已启动"
fi

# ---- 4. 等健康 ----
log "等待健康检查（最多 150 秒）..."
HEALTHY=0
for i in $(seq 1 50); do
  sleep 3
  STATUS=$(docker inspect "$CONTAINER" --format '{{.State.Health.Status}}' 2>/dev/null || echo unknown)
  if [ "$STATUS" = "healthy" ]; then
    HEALTHY=1
    log "健康（${i}x3 秒内）"
    break
  fi
  if [ "$STATUS" = "unhealthy" ]; then
    log "不健康，日志："
    docker logs --tail 50 "$CONTAINER" 2>&1
    break
  fi
done

# ---- 5. 功能探活 ----
if [ "$HEALTHY" = "1" ]; then
  log "功能探活..."
  for p in /login /api/product/info /billing/users /billing/cards /recharges /balances /orders /docs/errors /user/login; do
    CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 "http://127.0.0.1:${HOST_PORT}${p}" || echo "000")
    log "  ${p} → HTTP ${CODE}"
  done
  log "=== 升级完成 ==="
  docker ps --filter "name=$CONTAINER" --format "  {{.Names}} | {{.Status}} | {{.Ports}} | {{.Image}}"
  exit 0
fi

# ---- 6. 失败回滚 ----
log "=== 升级失败，回滚 ==="
if [ -n "$OLD_ID" ]; then
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker run -d \
      --name "$CONTAINER" --restart unless-stopped \
      -p "${HOST_PORT}:18080" \
      -e TZ=Asia/Shanghai \
      -e MAILPOOL_DATA=/app/data \
      -e MAILPOOL_HOST=0.0.0.0 \
      -e MAILPOOL_PORT=18080 \
      -e MAILPOOL_ADMIN_PASSWORD="$ADMIN_PASSWORD" \
      -v "${DATA_DIR}:/app/data" \
      "$OLD_ID" >/dev/null && log "已回滚到旧镜像"
else
  log "无旧镜像可回滚"
fi
exit 1
