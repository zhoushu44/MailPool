# Changelog

本项目遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

---

## [1.0] - 2026-11-16

首个正式发布版本。

### 新增

**邮箱池**

- 批量导入 Outlook 账号（支持 4 段 / 3 段 / 2 段三种格式）
- 双维度状态管理：`pool_status`（available / using / retired）× `health_status`（unknown / healthy / unhealthy / dead）
- 邮箱单次使用约束：`email_usages` 表永久记录，取号即绑定，任何项目不可复用
- 只发放 `healthy` / `unknown` 账号，异常号不流向客户端
- 账号标签、备注、账号级代理

**取码**

- **旧信防护**：取号瞬间抓取收件箱基线（message_id 集合 + 时间戳），receive 时用「时间窗口 + ID 基线」双判据过滤，早于取号时刻的邮件一律丢弃
- 新增 `mail_baselines` 表存储基线
- Graph API 优先，失败自动降级 IMAP XOAUTH2 / 密码登录
- 支持 inbox / junkemail / deleteditems 三个文件夹
- 24 小时跨次去重（同一封邮件不重复返回验证码）
- 项目级自定义验证码正则，优先于自动提取规则

**账号维护**

- 健康扫描：串行低频检测，7 天缓存，连续 3 个风控自动暂停
- Token 批量刷新：每号间隔 3 秒防限流，进度可查
- 心跳保活：定时静默拉取收件箱
- Device Code Flow 重新授权
- 错误分级：`transient`（限流/网络/5xx，只标记异常不累计判死）、`credential`（凭证失效）、`risk_control`（真风控）
- 单号恢复 / 批量恢复 / 重新导入自动复活

**管理后台**

- 仪表盘、总览、账号、收件箱、项目、邮箱池、购买邮箱、下单流水、费用明细、账户资料、设置
- 号池健康按 `client_id` 维度统计，识别被封的 client_id
- 账号列表支持按池状态 / 健康状态 / 标签 / 关键词筛选
- 邮箱池分页（默认 200 条/页）
- 刷新历史明细（含失败原因）
- 响应式布局，375px 可用

**对外 API**

- MailNest 兼容响应格式
- Bearer API Key 鉴权
- 业务码：`00000` 成功 / `D0002` 库存不足 / `D0003` 项目不存在 / `D0004` 操作不允许 / `D0005` 临时故障 / `D0006` 账号失效
- 公开接口 `/api/product/info` 无需鉴权

**运维**

- Docker 镜像（linux/amd64 + linux/arm64）
- GitHub Actions 自动构建推送（标签 `1.0` + `latest`）
- 容器内置 HEALTHCHECK
- 数据目录 volume 挂载，数据库自动迁移

### 修复

- **取码返回旧验证码**：回收号收件箱里的历史邮件会被当成新信返回，导致注册必然「验证码错误」。改为基线 + 时间窗口双判据
- **限流被误判为风控**：`429` / `403` 曾被归类为 `risk_control`，累计 6 次后物理删除账号。现归为 `transient`，只标记异常
- **异常账号仍被发放**：`claim_account` 只排除 `dead`，`unhealthy` 也会发给客户端。现只发 `healthy` / `unknown`
- **风控自动物理删除**：误判即永久丢号。改为默认只标记，由人工确认后删除
- **Device Code 重授权后无法使用**：`pool_status` 仍为 `retired`。现在未被使用过的账号会自动回池
- **死号无法恢复**：重新导入被当重复跳过。现在会更新凭证并复活
- **健康扫描 500**：`cache_cutoff` 与 `last_check_at` 时区类型不一致导致 `TypeError`
- **健康扫描报错后仍判死**：异常路径未走统一失败处理
- **项目表单不可提交**：JS 用 `this.f_config` 取了不存在的属性
- **659 个按钮失效**：`|tojson` 输出到双引号 `onclick` 属性里导致语法错误
- **收件箱正文空白**：`srcdoc` 未转义引号，HTML 被截断
- **会话密钥可伪造**：曾使用硬编码 `mailpool-dev-secret`
- **后台无鉴权**：任何能访问端口的人都能看到真实账号凭证
- **`data-confirm` 无效**：确认框点取消仍会执行操作
- **`release` 假成功**：`rowcount=0` 时仍返回成功
- **刷新日志截断成非法 JSON**：`detail` 字段被硬截断在 4000 字符
- **注册配置保存丢字段**：部分提交会覆盖掉未提交的配置项
- **定时任务永不触发**：CronTrigger 配置为 `minute="0"` 但窗口判断有误
- **`receive` 耗时翻倍**：Graph 与 IMAP 各自跑满 timeout，且 Graph 无结果后还会用 IMAP 重扫同一邮箱
- **模板 `TemplateResponse` 旧签名**：starlette 1.x 已移除，全站 500

### 性能

- **项目页 6.6s → 0.4s**：53 个项目共用同一号池，原来每个项目都跑一次 `count_claimable`（N+1）
- **首页 4.7s → 0.7s**：同上
- **商城 4.8s → 0.5s**：同上
- **邮箱池 4.3s → 1.0s**：分页下推 SQL + 默认 200 条/页
- 开启 GZip：`/pool` 178KB → 12KB，商城 128KB → 6.7KB
- `count_claimable` 改 SQL `COUNT` 聚合，不再全量拉取 ID
- `list_accounts` 分页下推 SQL `LIMIT/OFFSET`
- 模板关闭 `auto_reload`，避免每请求 stat 全部模板文件
- Google Fonts 改异步加载

### 变更

- `acquired_at` / `received_at` 统一输出带时区的 UTC ISO8601（`+00:00`）
- 空箱 `receive` 返回 `code=00000` + `data.status="timeout"`，不再是 `D0005`
- 软硬错误按业务码区分，不再依赖文案判断
- `release` 返回增加 `reusable` 与说明文案
- `/api/v1/health` 号池型 provider 的 `configured` 改为看池内库存
- 号池健康扫描返回增加 `need_review` 字段
- 邮箱池页默认每页 200 条（可用 `?limit=500` 恢复）

### 移除

- 自动注册模块（`app/register/`、`/register` 页面、`/api/admin/register/*` 接口、`register_config` 配置）
  - 该模块改为纯 HTTP 协议注册后已不再需要 Chrome / xvfb，移除后镜像体积从 ~1.2GB 降到 209MB
- 依赖精简：去掉 `playwright`

### 已知限制

- 号池只减不增：每个邮箱只用一次，`release` 是永久消耗
- 号池账号均为回收号，收件箱必然有历史邮件；已保证不返回旧码，但无法改变账号被用过的事实
- `receive` 实际耗时会超出 `timeout` 约 1~3 秒，客户端读超时建议 `timeout + 30s`
- 单 worker 运行：内置 APScheduler 定时任务与进程内状态，多 worker 会导致任务重复执行
- `/api/v1/mails` 会返回历史邮件记录（含旧验证码），客户端不应直接用它取码

---

## 版本说明

### 版本号规则

- **主版本（1.x）**：不兼容的 API 变更
- **次版本（1.1）**：向后兼容的功能新增
- **修订号（1.0.1）**：向后兼容的缺陷修复

### 镜像标签

| 标签 | 含义 | 更新时机 |
|---|---|---|
| `1.0` | 固定版本，内容不变 | 发布 v1.0 时 |
| `latest` | 最新构建 | 每次 push 到 main/master |
| `sha-xxxxxx` | 具体提交 | 每次 push 到 main/master |

生产环境建议用固定版本号（如 `1.0`），避免 `latest` 意外变更。
