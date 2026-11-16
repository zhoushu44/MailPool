# 页面加载性能优化记录（2026-11-16）

## 问题

远程访问 `http://<SERVER_HOST>:18090/` 各页面打开很慢，最慢 6.6 秒。

## 诊断过程

### 1. 对比本地与服务器单函数耗时

| 函数 | 本机 | 服务器容器 | 倍数 |
|---|---|---|---|
| `count_claimable` ×1 | 1 ms | 7 ms | 7× |
| `count_claimable` ×53 | 78 ms | **3599 ms** | 46× |
| `list_accounts(500)` | 6 ms | **700 ms** | 117× |
| `health_summary` | 2 ms | 98 ms | 49× |

**服务器单核 CPU 慢 44 倍**（同样 30 万次循环：本机 16ms，服务器 700ms）。
且主机 `loadavg` 一度 200+，`top` 显示 **44% steal**（宿主机超卖，CPU 时间被抢走）。

### 2. 找到真正的大坑：N+1 查询

`/projects`、`/`、`/buy-email` 三个页面都要给 53 个项目显示"可用数量"，
每个项目都单独跑一次 `count_claimable()`：

```
53 个项目 × 共享同一个 outlook 池 = 53 次完全相同的全表扫描
→ 服务器上 3.5 秒
```

`/pool` 另外还有 `list_accounts()` 把整表 995 行全部 ORM 实例化后再切片。

### 3. 传输体积

| 页面 | 优化前 |
|---|---|
| `/pool` | 427 KB |
| `/buy-email` | 128 KB |
| `/projects` | 89 KB |

服务器公网上行带宽有限，427KB 要传 2 秒以上。

### 4. Google Fonts 阻塞渲染

`base.html` 里同步加载 `fonts.googleapis.com` 的 Inter 字体。
国内访问 Google 经常超时 1-3 秒，**期间整页白屏**。

---

## 优化内容

### 1. `count_claimable` 改 SQL 聚合

```python
# 前：把全部 id 拉到 Python 里 len()
rows = db.scalars(select(EmailAccount.id).where(...)).all()
return len(rows)

# 后：SQL COUNT
return int(db.scalar(select(func.count(EmailAccount.id)).where(...)) or 0)
```

### 2. 项目数量按 provider 缓存（消除 N+1）

```python
avail_cache: dict[str, int] = {}
for p in projects:
    _attach_project_meta(db, p, avail_cache)   # 同一 provider 只查一次
```

同步改了 4 处：`routes.py` 的 `/`、`/projects`、`/buy-email`，
`email_api.py` 的 `/api/product/info`，`admin_api.py` 的 `/api/admin/projects`。

### 3. `list_accounts` 分页下推 SQL

```python
# 前：全表 995 行 ORM 实例化 → Python 切片
accounts = list(db.scalars(stmt)); accounts = accounts[offset:offset+limit]

# 后：SQL LIMIT/OFFSET（tag 过滤才回退全量）
stmt = stmt.limit(limit).offset(offset)
```

### 4. `/pool` 默认分页 200 条

原来固定 500 条 → HTML 403KB / 6600 DOM 节点。
现在默认 200 条，页面底部有「上一页 / 下一页 / 显示 500 条」，
标题显示「本页 200 条 / 共 995 条 第 1 页」。

### 5. 开启 GZip 压缩

```python
from starlette.middleware.gzip import GZipMiddleware
app.add_middleware(GZipMiddleware, minimum_size=1024, compresslevel=6)
```

### 6. 模板关闭 auto_reload

```python
templates.env.auto_reload = False
```

原来每个请求都要 stat 一遍全部模板文件。

### 7. Google Fonts 改非阻塞

```html
<link href="..." rel="stylesheet" media="print" onload="this.media='all'">
```

字体异步加载，失败也不影响首屏（CSS 里已有 `system-ui` 兜底）。

---

## 优化效果（真实浏览器实测）

| 页面 | 优化前 | 优化后 | 提升 |
|---|---|---|---|
| 仪表盘 `/` | 4701 ms | **695 ms** | **6.8×** |
| 项目 `/projects` | 6598 ms | **404 ms** | **16.3×** |
| 邮箱池 `/pool` | 4340 ms | **1005 ms** | **4.3×** |
| 商城 `/buy-email` | 4754 ms | **512 ms** | **9.3×** |
| 账号 `/accounts` | 518 ms | **295 ms** | 1.8× |
| 设置 `/settings` | 900 ms | **388 ms** | 2.3× |
| 收件箱 `/mailbox` | 813 ms | **403 ms** | 2.0× |
| 注册 `/register` | 785 ms | **398 ms** | 2.0× |

**平均 538 ms**（优化前平均约 2.2 秒）

### gzip 压缩效果

| 页面 | 原始 | gzip 后 |
|---|---|---|
| `/pool` | 178 KB | **12 KB** |
| `/buy-email` | 128 KB | **6.7 KB** |
| `/projects` | 89 KB | **10.3 KB** |
| `/` | 50 KB | **3.8 KB** |

---

## 剩余瓶颈（服务器本身）

1. **CPU 被超卖**：`top` 显示 35~44% steal，`loadavg` 一度 200+。
   这台机器上跑着 60+ 个容器，MailPool 只占 0.6% CPU / 97MB 内存，
   但拿到 CPU 时间片要排队。**换台配置更实在的机器会有本质改善。**
2. `/pool` 仍约 1 秒：TTFB 700ms（Python 渲染 200 行表格 + 6 个统计查询
   在这颗被超卖的 Xeon 6230 上要这么久）。
3. 报表类页面（`/orders`）偶尔 800ms+，同样是 CPU 抢占导致抖动。

---

## 验证

- 13 个页面全部 200，JS 错误 0、console 错误 0、5xx 0
- 分页正确：`/pool` 第 1/2/5 页分别 200/200/195 行，ID 不重叠
- 账号页仍显示「共 995 条，当前仅显示最新 500 条」

## 改动文件

```
app/services/pool_service.py   count_claimable 改 COUNT；list_accounts 分页下推
app/web/routes.py              avail_cache 缓存；/pool 分页参数；auto_reload=False
app/api/email_api.py           /api/product/info 加 stock_cache
app/api/admin_api.py           /api/admin/projects 加 avail_cache
app/main.py                    加 GZipMiddleware
app/web/templates/base.html    Google Fonts 改非阻塞
app/web/templates/pool.html    分页控件 + 计数文案
```
