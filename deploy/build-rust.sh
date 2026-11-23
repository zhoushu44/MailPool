#!/bin/bash
# =============================================================================
# MailPool Rust 版构建/部署脚本（在服务器上跑，从本地 git 仓库构建）
#
# 用途：把 /opt/mailpool-src 更新到最新提交，构建镜像，替换容器。
#       全程不依赖 GitHub（服务器到 GitHub 不稳），代码来源是本机 push 上来的
#       /opt/mailpool.git 裸仓库。
#
# 用法：
#   bash /opt/mailpool-src/deploy/build-rust.sh              # 构建并部署最新 main
#   TAG=v3.2.0 bash deploy/build-rust.sh                     # 指定标签
#   DRY=1 bash deploy/build-rust.sh                          # 只构建不切换
#   ROLLBACK=1 bash deploy/build-rust.sh                     # 回滚上一个镜像
#
# 与 deploy/update.sh 的区别：
#   update.sh    从 Docker Hub 拉现成镜像（Python 版 CI 流程）
#   build-rust.sh 在本机编译（Rust 版没有 CI 镜像，且模板要打进镜像）
# =============================================================================
set -euo pipefail

REPO_DIR="${REPO_DIR:-/opt/mailpool-src}"
BARE_DIR="${BARE_DIR:-/opt/mailpool.git}"
CONTAINER="${CONTAINER:-mailpool}"
HOST_PORT="${HOST_PORT:-18090}"
CONTAINER_PORT="${CONTAINER_PORT:-18091}"
DATA_DIR="${DATA_DIR:-/opt/mailpool/data}"
VERSION_LABEL_PREFIX="${VERSION_LABEL_PREFIX:-rust}"

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"; }

# 管理口令必须从环境变量来，不写死在脚本里
ADMIN_PASSWORD="${MAILPOOL_ADMIN_PASSWORD:?请先 export MAILPOOL_ADMIN_PASSWORD='<你的口令>'}"

# ── 回滚模式：直接用上一个镜像重建容器 ──
if [ "${ROLLBACK:-0}" = "1" ]; then
    PREV=$(docker images --format '{{.Repository}}:{{.Tag}}' | grep "^mailpool-rust:" | sed 's/.*://' | sort -V | tail -2 | head -1)
    if [ -z "$PREV" ]; then log "❌ 找不到可回滚的镜像"; exit 1; fi
    log "回滚到 mailpool-rust:$PREV"
    IMAGE="mailpool-rust:$PREV"
    TAG="$PREV"
else
    # ── 1. 更新源码到最新 main ──
    log "=== 更新源码 ==="
    if [ ! -d "$REPO_DIR/.git" ]; then
        log "检出目录不存在，从裸仓库 clone"
        rm -rf "$REPO_DIR"
        git clone -q "$BARE_DIR" "$REPO_DIR"
    fi
    cd "$REPO_DIR"
    git fetch -q origin
    BEFORE=$(git rev-parse --short HEAD)
    git reset --hard -q origin/main
    AFTER=$(git rev-parse --short HEAD)
    log "  代码: $BEFORE -> $AFTER"

    TAG="${TAG:-$(git describe --tags --abbrev=0 2>/dev/null || echo "dev-$(date +%Y%m%d-%H%M%S)")}"
    IMAGE="mailpool-rust:$TAG"

    # ── 2. 构建 ──
    log "=== 构建 $IMAGE ==="
    log "  commit: $(git log --oneline -1)"
    # 构建上下文必须是仓库根（Dockerfile 里是 rust/... 和 app/web/...）
    if ! docker build -f rust/Dockerfile -t "$IMAGE" . > /tmp/mailpool-build.log 2>&1; then
        log "❌ 构建失败，最后 20 行："
        tail -20 /tmp/mailpool-build.log
        exit 1
    fi
    log "  ✓ 构建完成 $(docker images --format '{{.Size}}' "$IMAGE" | head -1)"
fi

if [ "${DRY:-0}" = "1" ]; then
    log "DRY=1，不切换容器。镜像已就绪：$IMAGE"
    exit 0
fi

# ── 3. 备份当前容器配置 ──
TS=$(date +%Y%m%d-%H%M%S)
mkdir -p /opt/mailpool
if docker inspect "$CONTAINER" >/dev/null 2>&1; then
    docker inspect "$CONTAINER" > "/opt/mailpool/container-before-$TS.json"
    log "已备份容器配置: /opt/mailpool/container-before-$TS.json"
fi

# ── 4. 替换容器 ──
log "=== 切换容器 ==="
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" \
    -p "${HOST_PORT}:${CONTAINER_PORT}" \
    -v "${DATA_DIR}:/app/data" \
    -e MAILPOOL_ADMIN_PASSWORD="$ADMIN_PASSWORD" \
    -e MAILPOOL_DATA=/app/data \
    -e MAILPOOL_PORT="$CONTAINER_PORT" \
    -e TZ=Asia/Shanghai \
    -e RUST_LOG=mailpool=info \
    --restart unless-stopped \
    --label "mailpool.version=${VERSION_LABEL_PREFIX}-${TAG}" \
    "$IMAGE"

# ── 5. 健康检查（最多等 40 秒）──
log "=== 等待就绪 ==="
for i in $(seq 1 20); do
    CODE=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:${HOST_PORT}/login" 2>/dev/null || echo 000)
    if [ "$CODE" = "200" ]; then
        log "  ✓ /login 200（第 ${i} 次探测）"
        break
    fi
    if [ "$i" = "20" ]; then
        log "  ❌ 40 秒内未就绪，最后日志："
        docker logs "$CONTAINER" 2>&1 | tail -20
        log "  可用 ROLLBACK=1 回滚"
        exit 1
    fi
    sleep 2
done

# ── 6. 清理旧镜像（保留最近 3 个）──
docker images --format '{{.Repository}}:{{.Tag}} {{.ID}}' \
    | grep "^mailpool-rust:" \
    | sort -V | head -n -3 \
    | awk '{print $2}' | xargs -r docker rmi -f >/dev/null 2>&1 || true

log "=== 部署完成 ==="
log "  镜像:   $IMAGE"
log "  容器:   $(docker ps --filter "name=$CONTAINER" --format '{{.Status}}')"
log "  内存:   $(docker stats --no-stream --format '{{.MemUsage}}' "$CONTAINER")"
log "  版本:   $(docker inspect "$CONTAINER" --format '{{index .Config.Labels "mailpool.version"}}')"
