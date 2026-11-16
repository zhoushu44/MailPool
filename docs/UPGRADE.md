# 更新说明

## 升级到 v1.0 需要注意

### 1. 数据目录权限

镜像以非 root 用户（UID 1000）运行。宿主机挂载的 `data` 目录如果是 root 创建的，
容器会写不进去、启动报「数据库只读」。

```bash
chown -R 1000:1000 /opt/mailpool/data
```

### 2. 必须设置管理口令

```bash
-e MAILPOOL_ADMIN_PASSWORD='你的口令'
```

不设时服务**只允许 127.0.0.1 访问**，外网打开会直接 403。

### 3. 数据库自动迁移

首次启动会自动补建新表和新列，无需手工执行 SQL：

- 新表 `mail_baselines`（旧信防护的收件箱基线）
- `email_accounts` 的新增列（`pool_status` / `health_status` / `fail_count` 等）
- `mail_records` 的新增列（`message_id` / `provider` / `folder` 等）

从老版本升级前建议先备份：

```bash
cp data/mailpool.db data/mailpool.db.bak-$(date +%Y%m%d)
```

### 4. 老数据里的旧验证码

升级前的 `mail_records` 里可能存有历史邮件和它们的验证码（旧版本会把回收号里的
旧信当成新信返回）。这些记录**不会自动删除**，`/api/v1/mails` 仍能查到。

它们不影响 `receive`（新版有基线过滤），但如果你的客户端习惯直接查
`/api/v1/mails` 找码，需要自己过滤 `received_at` 时间。建议清理：

```sql
DELETE FROM mail_records WHERE received_at < '升级日期';
```

---

## 从旧版本迁移

### 容器方式

```bash
# 1. 备份
cp /opt/mailpool/data/mailpool.db /opt/mailpool/data/mailpool.db.bak

# 2. 拉新镜像
docker pull <你的DockerHub用户名>/mailpool:1.0

# 3. 重建容器（数据目录不变）
docker rm -f mailpool
docker run -d --name mailpool \
  -p 18090:18080 \
  -e TZ=Asia/Shanghai \
  -e MAILPOOL_ADMIN_PASSWORD='你的口令' \
  -e MAILPOOL_DATA=/app/data \
  -v /opt/mailpool/data:/app/data \
  --restart unless-stopped \
  --health-cmd 'curl -fsS -o /dev/null http://127.0.0.1:18080/login || exit 1' \
  --health-interval 30s --health-timeout 10s --health-retries 3 --health-start-period 25s \
  <你的DockerHub用户名>/mailpool:1.0
```

### 源码方式

```bash
git pull
pip install -r requirements.txt
# 重启 uvicorn
```

---

## 回滚

```bash
# 镜像回滚
docker rm -f mailpool
docker run -d --name mailpool ... <你的DockerHub用户名>/mailpool:<旧版本tag>

# 数据库回滚
cp /opt/mailpool/data/mailpool.db.bak /opt/mailpool/data/mailpool.db
docker restart mailpool
```

> 新版数据库会自动加列，旧版镜像读取不受影响（多出来的列会被忽略），
> 所以镜像回滚通常不需要回滚数据库。

---

## API 变更

### 兼容性

v1.0 的对外 API 路径和响应格式**完全向后兼容**，老客户端不用改代码。

### 行为变更（重要）

| 项目 | 旧行为 | v1.0 行为 |
|---|---|---|
| 空箱 `receive` | 返回 `code=D0005`，`data=null` | 返回 `code=00000` + `data.status="timeout"`，`data` 不为 null |
| 旧验证码 | 回收号的旧信会被当成新信返回 | 基线 + 时间窗口双判据，旧信一律丢弃，宁可超时 |
| `acquired_at` | 有时 `+08:00` 有时 `+00:00` | 统一 `+00:00`（UTC 带时区） |
| `received_at` | 不带时区 | 统一 `+00:00`（UTC 带时区） |
| `release` | 只返回 `status` | 增加 `reusable:false` + 说明文案 |
| `receive` 耗时 | 可能是 `timeout` 的 2 倍以上 | 超出 `timeout` 约 1~3 秒 |

### 新增字段

**`POST /api/v1/email/temporary/buy`**
```json
{
  "code": "00000",
  "data": {
    "email": "xxx@outlook.com",
    "provider": "outlook",
    "project_code": "chatgpt001",
    "project_name": "ChatGPT",
    "account_id": 42,
    "status": "holding",
    "acquired_at": "2026-11-16T04:54:37.275003+00:00"
  }
}
```

**`POST /api/v1/email/receive`** — 成功时多返回 `received_at`
```json
{
  "code": "00000",
  "data": {
    "email": "xxx@outlook.com",
    "code": "809630",
    "status": "success",
    "elapsed": 8.6,
    "received_at": "2026-11-16T06:13:56+00:00"
  }
}
```

**`POST /api/v1/email/release`**
```json
{
  "code": "00000",
  "data": {
    "email": "xxx@outlook.com",
    "status": "retired",
    "pooled": true,
    "permanently_consumed": true,
    "reusable": false,
    "message": "号池邮箱已永久消耗，释放后不会回到可领取池"
  }
}
```

**`GET /api/v1/health`** — 号池型 provider 增加了 `source` / `stock`
```json
{
  "id": "outlook",
  "name": "Outlook 私有邮箱池",
  "pooled": true,
  "configured": true,
  "source": "email_pool",
  "stock": 966
}
```

---

## 客户端适配建议

### 1. 读超时

`receive` 的实际耗时可能比 `timeout` 多 1~3 秒。如果 `timeout=120`，客户端读超时应设为 **150 秒以上**。

### 2. 区分软硬错误

```python
resp = call_receive(...)
if resp["code"] == "00000":
    data = resp["data"]
    if data["status"] == "success":
        use_code(data["code"])
    elif data["status"] == "timeout":
        # 正常：对方还没发信，可以再调一次 receive（邮箱仍被占用着）
        retry_later()
elif resp["code"] == "D0005":
    # 临时故障：网络/上游抖动，可重试同一个邮箱
    retry_same_email()
elif resp["code"] == "D0006":
    # 账号失效：这个号废了，丢弃换号
    discard_and_rebuy()
elif resp["code"] == "D0002":
    # 库存不足
    wait_for_stock()
```

### 3. 不要依赖 `release` 回池

号池邮箱是**一次性**的。`release` 只是标记用完，号不会回来。
把它当成「释放资源引用」，不要当成「归还后可复用」。
