# Python → Rust 迁移文档

> 目标：把 MailPool 从 Python(FastAPI) 逐步换成 Rust(Axum)，**生产不中断、随时可回滚**。

## 1. 为什么能平滑切换

两个版本在设计上刻意做成**可以同时跑**：

| 共享资源 | 兼容性 | 验证方式 |
|---|---|---|
| `mailpool.db` | Rust 版能直接打开 Python 版写出的库，schema 逐列一致 | `rust/src/db.rs` 的迁移逻辑 + 集成测试 |
| `.session_secret` 会话密钥 | 同一份密钥，cookie 双向可读 | `auth::tests::python_itsdangerous_tokens_verify_in_rust` |
| 密码哈希 | `pbkdf2_sha256$200000$...` 格式与算法一致 | `auth::tests::python_password_hash_verifies_in_rust` |
| 模板文件 | 21 个 Jinja 模板 Rust 侧原样加载 | `web::templates::tests::all_templates_render_with_full_context` |
| `settings` 表键名 | 逐字对齐（`provider_config.*` / `billing.*` …） | `services::settings` 单测 |
| 业务错误码 | `D0001`…`D0007` / `99999` 逐字一致 | `error::tests::business_codes_match_python` |

**结论：用户不需要重新登录，数据不需要迁移。**

## 2. 灰度顺序（推荐）

```
                     ┌──────────────────┐
  客户端 ──► Nginx ──┤ /api/v1/*        ├──► Rust  (新)
                     │ /api/admin/*     ├──► Rust  (新)
                     │ /（页面）        ├──► Python(旧)  ← 先留旧，页面最复杂
                     └──────────────────┘
```

**第一步：只切 API，页面留 Python**
- 收益最大：`/api/product/info` 从 173 条 SQL 降到 3 条，吞吐 3-4 → 100+ req/s
- 风险最小：客户端是程序，出问题立刻能看出（错误码/超时）
- 回滚：改一行 Nginx 配置 reload

**第二步：切管理 API**
- 只影响你自己的后台操作，不影响客户

**第三步：切页面**
- 页面是给人看的，问题发现慢，放最后
- 注意此时两个版本渲染同一批模板，样式必然一致

## 3. Nginx 分流配置

```nginx
upstream mailpool_rust   { server 127.0.0.1:18080; }   # Rust
upstream mailpool_python { server 127.0.0.1:18090; }   # Python

server {
    listen 80;
    server_name your-domain.com;

    # ── 已切换：对外 API 走 Rust ──
    location /api/v1/ {
        proxy_pass http://mailpool_rust;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_read_timeout 300s;   # 收码最长 600s，要给足
    }

    # ── 已切换：管理 API 走 Rust ──
    location /api/admin/ {
        proxy_pass http://mailpool_rust;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }

    # ── 未切换：页面走旧版 ──
    location / {
        proxy_pass http://mailpool_python;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

**注意**：两个进程必须共用同一个 `MAILPOOL_DATA` 目录（同一个 db 文件）。
SQLite WAL 模式下两个进程同时写是安全的（`busy_timeout` 30s 已配），
但**不要让两个进程同时跑定时任务** —— 会重复执行。

停用 Python 侧的定时任务：把 `heartbeat_enabled` / `health_check_enabled` /
`refresh_enabled` 设为 `false`，只让 Rust 跑（或反之）。

## 4. Rust 版启动

```bash
cd rust
cargo build --release

# 环境变量与 Python 版完全一致
export MAILPOOL_DATA=/opt/mailpool/data
export MAILPOOL_ADMIN_PASSWORD='<口令>'
export MAILPOOL_PORT=18080
./target/release/mailpool
```

Docker：

```bash
docker build -t mailpool-rust:1.0 -f rust/Dockerfile .
docker run -d --name mailpool-rust \
  -p 18080:18080 \
  -v /opt/mailpool/data:/app/data \
  -e MAILPOOL_ADMIN_PASSWORD='<口令>' \
  mailpool-rust:1.0
```

## 5. 差分测试（切换前必做）

```bash
# 同时起两个版本，指向同一个库副本
cp data/mailpool.db /tmp/diff.db
MAILPOOL_DATA=/tmp/diff.db MAILPOOL_PORT=18090 python run.py &
MAILPOOL_DATA=/tmp/diff.db MAILPOOL_PORT=18080 ./rust/target/release/mailpool &

# 逐接口比对
python rust/scripts/diff_test.py \
  --python http://127.0.0.1:18090 \
  --rust   http://127.0.0.1:18080 \
  --key    <API_KEY>
```

比对规则见 `rust/src/testing.rs`：
- `Missing` / `TypeMismatch` → **阻塞**，必须修
- `ValueMismatch` → 报告但可能是随机量（订单号等）
- `Formatting` → 时间戳格式差异，可接受

## 6. 回滚

任何阶段出问题：

```bash
# 1) 改 Nginx 指回 Python
sed -i 's|mailpool_rust|mailpool_python|g' /etc/nginx/conf.d/mailpool.conf
nginx -s reload

# 2) 停 Rust
docker rm -f mailpool-rust
```

**数据风险为零**：两个版本写的是同一个库、同一套 schema。
Rust 版不引入任何新表/新列（`email_orders` / `email_usages` 的约束重建
是 Python 版本就已经在做的迁移，方向一致）。

## 7. 已知行为差异（有意为之）

| 项 | Python 版 | Rust 版 | 原因 |
|---|---|---|---|
| `pool.html` 状态徽章 | 读遗留 `status` 列 | 读 `pool_status` + `health_status` 派生 | 原版 bug：被占用的号可能显示"可用" |
| `hold` / `charge` 并发安全 | 读-判断-写（可能多冻结） | 条件 UPDATE（原子） | 原版 50 并发下有竞态 |
| `/api/product/info` SQL 数 | 173 条 | 3 条 | 批量聚合代替逐项目全表扫 |
| 模板 `pool.html` / `settings.html` | — | 各改 1-4 行 | minijinja 不支持 `.replace()` 方法与 `.get()` 方法 |

除此之外，**响应结构、字段名、金额格式、错误码全部逐字一致**。

## 8. 性能预期（基于实测）

| 指标 | Python 版 | Rust 版（预期） |
|---|---|---|
| `/api/product/info` 服务端耗时 | 215-415 ms | < 30 ms |
| 该接口吞吐 | 3-4 req/s（CPU 打满） | 100+ req/s |
| 常驻内存 | 85-136 MB | 15-30 MB |
| 镜像体积 | ~150 MB（Python 基础镜像） | ~20 MB（scratch/distroless） |
| 冷启动 | ~1.5 s | < 50 ms |
