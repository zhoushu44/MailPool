# MailPool 邮池 · v1.0

**一个号池，收所有验证码。**

MailPool 是一个面向自动化注册场景的 Outlook 邮箱池服务。把批量导入的邮箱集中托管起来，对外提供「取号 → 等码 → 取码 → 释放」的标准 API，注册机、自动化脚本直接接入即可，不用再自己维护邮箱。

---

## 它解决什么问题

自己攒一堆邮箱做自动化注册，最烦的从来不是「拿到邮箱」，而是这几件事：

- 邮箱里的旧验证码被当成新码返回，注册永远报「验证码错误」
- Token 过期了不知道，等报错才发现整批号都废了
- 微软限流一次，脚本把几百个号全标死了
- 并发取号抢到同一个邮箱
- 号码状态、用没用过、还能不能用，全靠 Excel 记

MailPool 就是把这堆麻烦收进一个服务里。

---

## 核心特性

### 邮箱池

- 支持 `email----password----client_id----refresh_token` 四段式批量导入
- 每个邮箱**只用一次**：取号即绑定，任何项目都不能再取（`email_usages` 永久记录）
- 池状态 `available / using / retired`，健康状态 `unknown / healthy / unhealthy / dead` 双维度独立管理
- 只发放 `healthy` / `unknown` 的号，异常号不会流到客户端

### 取码

- **旧信防护**：取号瞬间抓取收件箱基线（message_id 集合 + 时间戳），`receive` 使用「时间窗口 + ID 基线」双判据，早于取号时刻的邮件一律丢弃
- Graph API 优先，自动降级 IMAP XOAUTH2 / 密码登录
- 多格式时间解析（ISO8601 / RFC2822 / unix 时间戳），全部归一为带时区的 UTC
- 验证码提取支持项目自定义正则，优先于自动规则

### 账号维护

- 健康扫描：串行低频检测，7 天缓存，连续风控自动暂停
- Token 批量刷新：每号间隔 3 秒防限流
- 心跳保活：定时静默拉取收件箱
- 错误分级：`transient`（限流/网络，只标记异常不判死）/ `credential`（凭证失效）/ `risk_control`（真风控）
- 恢复手段：单号恢复、批量恢复、Device Code 重授权、重新导入自动复活

### 管理后台

- 仪表盘、账号管理、收件箱、项目管理、邮箱池、下单流水、费用明细、设置
- 号池健康按 `client_id` 维度统计，快速识别被封的 client_id
- 响应式布局，手机可用

### 对外 API

- 对齐 MailNest 格式：`{"code": "00000", "msg": "", "type": "", "data": ...}`
- Bearer API Key 鉴权
- 业务码区分软硬错误：`D0002` 库存不足 / `D0003` 项目不存在 / `D0005` 临时故障（可重试）/ `D0006` 账号失效（换号）
- 空箱返回 `code=00000` + `data.status="timeout"`，是正常轮询结果而非错误

---

## 快速开始

### Docker 部署

```bash
docker pull <你的DockerHub用户名>/mailpool:1.0

docker run -d --name mailpool \
  -p 18080:18080 \
  -e MAILPOOL_ADMIN_PASSWORD='换成你的管理口令' \
  -v $(pwd)/data:/app/data \
  --restart unless-stopped \
  <你的DockerHub用户名>/mailpool:1.0
```

打开 `http://服务器IP:18080/`，用管理口令登录。

> 数据目录 `/opt/mailpool/data` 需要 UID 1000 可写：
> `chown -R 1000:1000 ./data`

### 环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `MAILPOOL_ADMIN_PASSWORD` | 空 | 管理后台口令。**为空时仅允许回环地址访问** |
| `MAILPOOL_DATA` | `/app/data` | 数据目录（SQLite + 会话密钥） |
| `MAILPOOL_HOST` | `0.0.0.0` | 监听地址 |
| `MAILPOOL_PORT` | `18080` | 监听端口 |
| `MAILPOOL_SESSION_SECRET` | 自动生成 | 会话签名密钥，不设则首次启动随机生成并持久化到 `data/.session_secret` |
| `TZ` | `Asia/Shanghai` | 时区 |

### 首次使用

1. 登录后台 → **设置** → 生成 API Key
2. **项目** → 新建项目（填 `project_code`、名称、邮箱源选 `outlook`）
3. **邮箱池** → 粘贴账号文本批量导入
4. **邮箱池** → 点「健康扫描」确认账号可用
5. 调用 API 开始取号

---

## API 速览

```bash
API_KEY="ak_xxxxxxxx"
BASE="http://your-server:18080"
PROJECT="chatgpt001"

# 1. 取号
curl -X POST "$BASE/api/v1/email/temporary/buy" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"project_code": "'$PROJECT'", "count": 1}'

# 2. 取码（timeout 是等待秒数，建议客户端读超时设为 timeout + 30s）
curl -X POST "$BASE/api/v1/email/receive" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"project_code": "'$PROJECT'", "email": "xxx@outlook.com", "timeout": 120}'

# 3. 释放（号池为永久消耗，号不会回池）
curl -X POST "$BASE/api/v1/email/release" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"project_code": "'$PROJECT'", "email": "xxx@outlook.com"}'
```

完整文档见后台 `/docs/api-overview`。

---

## 目录结构

```
app/
├── api/           对外 API + 管理 API + 鉴权
├── providers/     邮箱源实现（outlook / mailnest / cf_temp / tempmail）
├── services/      业务编排（取号 / 取码 / 号池 / 项目 / 设置）
├── web/           管理后台页面（Jinja2 模板 + 静态资源）
├── models.py      ORM 模型
├── database.py    引擎与建表迁移
├── timeutil.py    统一时间处理
└── main.py        FastAPI 入口
data/              SQLite 数据库与运行时数据（不提交）
docs/              部署、修复、性能等文档
.github/workflows/ GitHub Actions（自动构建镜像）
```

---

## CI/CD

推送到 `main` / `master` 分支时，GitHub Actions 自动：

1. Python 语法与导入检查
2. 构建 `linux/amd64` + `linux/arm64` 双架构镜像
3. 打上 **`1.0`** 与 **`latest`** 两个标签
4. 推送到 Docker Hub
5. 拉取刚推的镜像跑冒烟测试（确认 0.0.0.0:18080 可访问）

需要的 GitHub Secrets：

| Secret | 说明 |
|---|---|
| `DOCKER_HUB_USERNAME` | Docker Hub 用户名 |
| `DOCKER_HUB_TOKEN` | Docker Hub Access Token（不是登录密码） |

---

## 已知限制

1. **号池只减不增**：每个邮箱只用一次，`release` 是永久消耗，不是归还
2. **号池里的号都是回收号**：收件箱必然有历史邮件。已保证不会返回旧验证码，但无法改变账号被用过的事实
3. **`receive` 会超出 `timeout` 1~3 秒**（Graph 单轮 3 个文件夹的固定开销），客户端读超时请按 `timeout + 30s`
4. **单 worker 运行**：内置 APScheduler 定时任务与进程内状态，多 worker 会导致任务重复执行

---

## 许可

[MIT](LICENSE)

## 安全

部署前请先读 [SECURITY.md](SECURITY.md)。MailPool 会持有大量邮箱 OAuth 凭据，
**务必设置 `MAILPOOL_ADMIN_PASSWORD`**，并且不要把 `data/` 提交进仓库。

## 贡献

见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 声明

本项目仅供**自有邮箱的集中管理与自动化测试**使用。使用者应自行确保：

- 对所导入的邮箱拥有合法使用权
- 使用行为符合目标网站的服务条款与当地法律法规
- 不用于垃圾邮件、欺诈或任何违法用途

作者不对使用者的具体使用方式承担责任。
