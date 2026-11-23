# ---------------------------------------------------------------------------
# MailPool Rust 版镜像
#
# 构建：docker build -t mailpool-rust:1.0 -f rust/Dockerfile .
#
# 设计取舍：
#   - 多阶段构建：builder 里编译，runtime 用 debian-slim（要 libssl3 动态库）
#   - 静态链接 musl：产物是单文件，能跑在任何 Linux 上
#   - 非 root 运行（uid 1000），数据目录需要可写
#   - 模板与静态资源**必须**带进镜像（不像 Python 版能直接挂仓库目录）
# ---------------------------------------------------------------------------

# ── 阶段 1：编译 ────────────────────────────────────────────────────────
FROM rust:1.100-bookworm AS builder

WORKDIR /build

# 构建期依赖：
#   pkg-config / libssl-dev —— imap 走 native-tls → openssl-sys，没有头文件直接编译失败
#   ca-certificates        —— 拉 crates.io 用
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# 先只拷依赖清单，让 cargo 层可缓存（改代码不用重下依赖）
COPY rust/Cargo.toml rust/Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# 再拷真实源码
COPY rust/src ./src
# 让 cargo 认为 main.rs 变了，强制重新编译
RUN touch src/main.rs && cargo build --release

# ── 阶段 2：运行 ────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# ca-certificates：Graph/IMAP 的 TLS 需要根证书
# tzdata：TZ=Asia/Shanghai 生效（日志时间、定时任务窗口）
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata libssl3 \
    && rm -rf /var/lib/apt/lists/*

# 非 root 用户（与 Python 版 Dockerfile 的 UID 保持一致，宿主目录权限不用改）
RUN groupadd --gid 1000 mailpool \
    && useradd --uid 1000 --gid 1000 --create-home --shell /usr/sbin/nologin mailpool

WORKDIR /app

# 二进制
COPY --from=builder /build/target/release/mailpool /app/mailpool

# 模板 + 静态资源（Rust 版不做模板缓存，直接从磁盘读）
COPY app/web/templates /app/app/web/templates
COPY app/web/static /app/app/web/static

# 数据目录
RUN mkdir -p /app/data && chown -R mailpool:mailpool /app
VOLUME ["/app/data"]

ENV MAILPOOL_DATA=/app/data \
    MAILPOOL_HOST=0.0.0.0 \
    MAILPOOL_PORT=18080 \
    TZ=Asia/Shanghai \
    RUST_LOG=mailpool=info

USER mailpool
EXPOSE 18080

# 健康检查：/login 是公开页面，不需要鉴权
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/app/mailpool", "--healthcheck"]

ENTRYPOINT ["/app/mailpool"]
