# MailPool —— Outlook 邮箱池服务
# 构建：docker build -t mailpool .
# 运行：docker run -d -p 18080:18080 \
#         -e MAILPOOL_ADMIN_PASSWORD='你的管理口令' \
#         -v $(pwd)/data:/app/data \
#         mailpool

FROM python:3.12-slim-bookworm

# ---------------------------------------------------------------------------
# 环境变量
# ---------------------------------------------------------------------------
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PYTHONIOENCODING=utf-8 \
    LANG=C.UTF-8 \
    TZ=Asia/Shanghai \
    MAILPOOL_DATA=/app/data \
    MAILPOOL_HOST=0.0.0.0 \
    MAILPOOL_PORT=18080

# ---------------------------------------------------------------------------
# 系统依赖
#   ca-certificates  HTTPS 证书（访问 Graph API / token 端点必需）
#   tzdata           时区数据（容器内 TZ=Asia/Shanghai 要能生效）
#   curl             健康检查探针
# ---------------------------------------------------------------------------
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        tzdata \
        curl \
    && ln -snf /usr/share/zoneinfo/$TZ /etc/localtime \
    && echo $TZ > /etc/timezone \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# ---------------------------------------------------------------------------
# Python 依赖（先装依赖，代码变动时可复用镜像层缓存）
# ---------------------------------------------------------------------------
COPY requirements.txt ./
RUN pip install --no-cache-dir --upgrade pip \
    && pip install --no-cache-dir -r requirements.txt

# ---------------------------------------------------------------------------
# 应用代码
# ---------------------------------------------------------------------------
COPY . .

# ---------------------------------------------------------------------------
# 非 root 用户 + 数据目录
#   数据目录 /app/data 会被挂载成 volume，UID 1000 需要有写权限：
#   docker run 时若宿主目录属主不是 1000，请先 chown -R 1000:1000 ./data
# ---------------------------------------------------------------------------
RUN groupadd --gid 1000 mailpool \
    && useradd --uid 1000 --gid 1000 --create-home --shell /usr/sbin/nologin mailpool \
    && mkdir -p /app/data \
    && chown -R mailpool:mailpool /app

USER mailpool

VOLUME ["/app/data"]

EXPOSE 18080

# ---------------------------------------------------------------------------
# 健康检查
#   用 /login（公开页面，纯模板渲染）而不是 /api/v1/health
#   —— 后者需要 API Key，会返回 401 导致容器永远 unhealthy。
# ---------------------------------------------------------------------------
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
    CMD curl -fsS -o /dev/null "http://127.0.0.1:${MAILPOOL_PORT}/login" || exit 1

# ---------------------------------------------------------------------------
# 启动：监听 0.0.0.0
#   workers=1 是必须的（内置 APScheduler 定时任务 + 进程内状态，
#   多 worker 会导致定时任务重复执行、状态不同步）
# ---------------------------------------------------------------------------
CMD ["sh", "-c", "exec python -m uvicorn app.main:app --host ${MAILPOOL_HOST} --port ${MAILPOOL_PORT} --workers 1 --proxy-headers --forwarded-allow-ips '*'"]
