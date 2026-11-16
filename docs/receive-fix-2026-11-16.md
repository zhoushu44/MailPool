# 取码逻辑修复记录（4 个坑）

针对外部 provider 反馈的 4 个服务端问题，全部修复并验证。

---

## 坑 1（最严重）：`receive` 返回收件箱里的**旧验证码**

### 现象
刚取到的号收件箱里还留着 8~14 天前的旧信，第一次 `receive` 在 1.9 秒内就返回
`success`，实际返回的是 11-08 那封旧信的码。拿去注册必然报"验证码错误"，
但现象看起来像串号。

### 根因
`_graph_wait_code` / `_imap_wait_code` 遍历收件箱时，**任何能提取出验证码的邮件都会返回**，
唯一的防护是进程内 24h 去重缓存（`_is_returned`），而重启后缓存清空、
新号也从没被"返回过"，所以旧信全部命中。

### 修复
新增 **时间窗口 + message_id 基线双判据**（任一不满足就跳过）：

```python
# app/providers/outlook.py
MAIL_FRESH_SLACK_SECONDS = 60      # 服务器时钟误差容忍
MAIL_FRESH_MAX_AGE_MINUTES = 30    # 无基线时最多认 30 分钟内的信

def is_fresh_mail(received_at, baseline_at, ...) -> bool:
    dt = parse_mail_time(received_at)
    if dt is None:
        return False                      # 时间不可解析 → 当旧信（保守）
    if baseline_at is not None:
        return dt >= baseline_at - timedelta(seconds=slack)
    return dt >= now - timedelta(minutes=max_age_minutes)
```

Graph 路径三重过滤：
```python
if mid in baseline:            continue   # ① 取号时就在箱里 = 旧信
if _is_returned(email, mid):   continue   # ② 24h 内返回过
if not is_fresh_mail(recv, baseline_at): continue   # ③ 早于取号时刻 = 旧信
```

IMAP 路径同样加了 `mid_str in self._baseline_ids` + `is_fresh_mail(mail_date, ...)`。

### 验证（真实账号，收件箱 17 封旧信）
```
[outlook-graph] 轮询开始 基线=17封 baseline_at=2026-11-16 03:14:02+00:00
[outlook-graph] 跳过基线旧信 folder=inbox mid=AQMkADAwATM0MDAAMi1h...
（17 封全部跳过）
[outlook-graph] 轮询超时未取到验证码（已扫描 17 封，跳过旧信 17 封）
[outlook-imap] 跳过超时旧信 folder=INBOX date=Sun, 15 Nov 2026 05:43:11 -0800
...
```
**结果：返回 `status=timeout`，不再返回任何旧码。**

---

## 坑 2：收件箱懒同步，导致"取号时拍基线"失效

### 现象
取号那一刻查询 `/api/v1/mails` 得到空箱，旧信要等 `receive` 才从上游拉进来，
这些旧信的 id 不在基线里，"不在基线就算新信"会误判。

### 根因
`/api/v1/mails` 只读本地 `mail_records`（历史缓存），不会主动拉取上游；
而 `receive` 是直连 Graph/IMAP 实时拉的。两者数据源不同。

### 修复
1. **在 claim 时主动抓基线**（不是等 receive）：
   ```python
   # app/services/mail_service.py: acquire_email()
   pool_service.snapshot_mail_baseline(db, claimed["email"], project.code)
   ```
   `snapshot_mail_baseline()` 直连 Graph 拉 inbox/junkemail/deleteditems，
   把当前所有 message_id 和最新邮件的 receivedDateTime 一起存进 `mail_baselines` 表。

2. **判据不只靠 id**：即使基线抓取失败（`captured=0`），
   仍有 `baseline_at` 时间窗口兜底 —— 早于取号时刻的邮件一律丢弃。

3. **兜底**：老数据没有基线记录时，`receive_code()` 会用 `claimed_at` 作为
   时间窗口起点现抓一次。

### 新表
```sql
CREATE TABLE mail_baselines (
    id INTEGER PRIMARY KEY,
    email VARCHAR(254) UNIQUE,
    project_code VARCHAR(64),
    baseline_at DATETIME,        -- 抓取时刻（naive UTC）
    message_ids TEXT,            -- 基线里的 message_id，逗号分隔
    latest_received_at DATETIME, -- 基线里最新一封的时间
    captured INTEGER             -- 是否抓取成功（0=降级为纯时间判据）
);
```

---

## 坑 3：空箱返回 `D0005` 业务错误，而不是 `status=timeout`

### 现象
`data` 是 `null`。如果当成失败抛异常，注册就会在"对方还没发信"时中断。
更糟的是 `D0006`（账号被封禁）的文案也含"取件失败"，只能靠业务码区分。

### 修复
1. **空箱走正常返回**（`mail_service.receive_code`）：
   ```python
   # 空箱/没等到新信 —— 这是正常轮询结果，不是错误
   return {"email": email, "code": "", "status": "timeout",
           "elapsed": ..., "message": f"等待 {tmo:.0f}s 未收到验证码"}
   ```
   → API 层返回 `code=00000` + `data.status="timeout"`。**永远不会 `data=null`。**

2. **软硬错误按业务码分**（新增 `kind` 字段）：
   ```python
   # app/api/email_api.py: _map_provider_error
   if exc.fatal or kind in ("credential", "banned"):
       return err(CODE_BANNED, "账号被封禁 取件失败")     # D0006 硬错误
   if kind in ("transient", "network", "fetch"):
       return err(CODE_FETCH_FAILED, "取件失败 请稍后再试") # D0005 软错误
   ```
   库存不足单独用 `kind="no_stock"` → `D0002`。

3. 接口 docstring 明确写清楚三种情况的语义。

### 验证
```
receive: HTTP 200 code=00000 status=timeout code_val=''
✅ 返回 00000 + status=timeout，不是 D0005，data 不为 null
```

---

## 坑 4：时间字段格式混乱，偏 8 小时

### 现象
`acquired_at` 同一部署先后给过 `+08:00` 和 `+00:00`，`received_at` 却不带时区。
直接相减整体偏 8 小时，把刚到的信判成旧信，导致永远取不到码。

### 修复
新增 `app/timeutil.py` 作为**唯一时间入口**：

```python
def normalize_utc(value) -> datetime | None:
    """支持 ISO8601（含 Z）、RFC2822、unix 时间戳、naive（按 UTC）→ 全转 aware UTC"""

def utc_iso(value) -> str:
    """统一输出带时区的 ISO8601（UTC）"""
```

所有对外时间字段统一走 `utc_iso()`：
- `acquired_at` / `created_at` / `received_at`

### 验证
```
2026-11-08T10:23:00Z            -> 2026-11-08T10:23:00+00:00
2026-11-08T10:23:00+08:00       -> 2026-11-08T02:23:00+00:00   ✅ 正确换算
Sat, 8 Nov 2026 10:23:00 +0800  -> 2026-11-08T02:23:00+00:00   ✅ RFC2822 正确
2026-11-08 10:23:00             -> 2026-11-08T10:23:00+00:00   （naive 按 UTC）
三者等价: True
```

---

## 次要问题

### `receive` 耗时超出 timeout 3~17 秒
**根因有两个**：
1. Graph 和 IMAP **各自跑满一个 timeout**（原本是串行两轮）
2. 更严重：Graph 正常跑完（只是没新信）后**还会再走一遍 IMAP**，
   同一个邮箱扫两遍，而 IMAP 刷新 token 本身要 10~15 秒

**修复**：
- Graph 正常执行完（无论有没有新信）→ **直接返回**，不再走 IMAP
- 只有 Graph 抛异常才降级 IMAP，且用剩余预算
- IMAP 循环内每封邮件都检查 deadline（原来只在 while 顶部检查）

**效果**：
| timeout | 修复前 | 修复后 |
|---|---|---|
| 5s | 16.6s | 6.4s |
| 10s | ~25s | 12.3s |

残留 +1~3 秒是单轮 Graph 请求（3 个文件夹 × HTTP 往返）的固定开销，无法再省。

### `release` 是永久消耗
文档/返回字段已明确：
```json
{"status":"retired","pooled":true,"permanently_consumed":true,
 "reusable":false,"message":"号池邮箱已永久消耗，释放后不会回到可领取池"}
```

### `/api/v1/health` 显示 `outlook configured=false`
**根因**：号池型 provider 的账号在 `email_accounts` 表里，不在 provider 配置里，
`bool(cfg)` 永远 false。

**修复**：号池型改为看池里有没有可用账号：
```json
{"id":"outlook","pooled":true,"configured":true,"source":"email_pool","stock":965}
```

### "固定号池未用过的账号"这个承诺不成立
**这是产品语义问题，不是 bug**：号池里都是回收号，收件箱必然有历史邮件。
本修复保证的是**不会返回旧验证码**，但无法改变"账号被用过"这个事实。
stock 数字代表"池里还剩多少可用号"，不代表"全新未用过的号"。

---

## 回归测试

### 时间判据（10 个样本）
```
✅ True  - 取号后30秒到达的新信
✅ True  - 取号后2分钟
✅ True  - 北京时间写法（=UTC 11:22）
✅ True  - IMAP RFC2822 写法
✅ True  - 取号前30秒（时钟误差容忍内）
✅ False - 真实旧信（11-15）
✅ False - 真实旧信（11-08）
✅ False - 取号前20分钟
✅ False - 时间缺失
✅ False - 时间为 None
10/10 通过（新信不误杀，旧信 100% 拦截）
```

### 端到端（服务器 <SERVER_HOST>:18090）
```
取号: adrianreynoldsf2804@outlook.com
       acquired_at=2026-11-16T03:37:42.973817+00:00   ← 带时区
receive: HTTP 200, code=00000, status=timeout, 耗时 16.4s
         ✅ 旧信全部挡住，返回正常超时
health:  {'id':'outlook','pooled':true,'configured':true,'source':'email_pool','stock':965}
release: {'status':'retired','permanently_consumed':true,'reusable':false}
```

---

## 改动文件

```
app/timeutil.py                 新增：统一时间解析/输出
app/models.py                   新增 MailBaseline 表
app/database.py                 init_db 建表（已含新表）
app/providers/outlook.py        is_fresh_mail / parse_mail_time；Graph+IMAP 双判据；
                                去掉重复的 IMAP 扫描；IMAP 循环内检查 deadline
app/services/mail_service.py    receive_code 注入基线；空箱返回 timeout；
                                acquire_email 抓基线；release 语义字段
app/services/pool_service.py    snapshot_mail_baseline / save_mail_baseline /
                                get_mail_baseline
app/api/email_api.py            软硬错误按 kind 分；时间字段 utc_iso；
                                health 号池型 configured；receive docstring
```

## 已知限制

1. `receive` 仍会超出 `timeout` 1~3 秒（Graph 单轮固定开销），
   客户端读超时建议按 **timeout + 30s** 设置。
2. 基线抓取需要 Graph 权限；只有 IMAP 密码的账号会走 `captured=0`
   降级为纯时间窗口判据（仍能挡住旧信，只是依赖服务器时钟准确）。
3. 时间判据依赖服务器时钟与微软一致（已留 60 秒容忍）。
