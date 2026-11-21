# Changelog

本项目遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

---

## [1.5] - 2026-11-22

### 修复

**收码轮询节奏太慢：12 秒窗口内只能跑 3 轮**

> 排查结论先说清楚：**瓶颈不是微软 Graph，也不是共用 client_id。**
> 实测（生产容器内，25 个真实账号）：
> - 单次 Graph 查 inbox：**0.21s**（串行均值），并发 15 时 **0.38s**
> - 25 并发持续打 Graph：**83.8 QPS**，平均每账号 6 秒内查 23.8 次
> - token 刷新：**0.40s/次**，15 个账号并发刷新墙钟 **1.41s**
> - 458 次刷新里 **0 次** 429、**0 次** 失败、**0 次** rt 轮换
>
> 那个"单次 Graph 13~17 秒"是**整条收码链路**的耗时，不是单次查询：
> 旧版每轮扫 3 个文件夹（inbox/junkemail/deleteditems）+ 固定 `sleep(4)`。
> 所以 12 秒窗口内只跑得动 **2.9 轮**。

- 现象：并发 25 时注册大量"收码超时"，客户端对同一邮箱反复轮询十几次才拿到码。
- 根因（三条，全部是我们自己的代码）：
  1. **每轮扫 3 个文件夹**，而实测验证码几乎只落 inbox —— 白白 3 倍请求量。
  2. **轮间固定 `sleep(4)`**，一轮 2s 查询 + 4s 睡眠 ≈ 6s，12s 窗口只够 2 轮。
  3. **sleep 在超时判定之后**：`timeout=5` 实际跑 **8.0s**，`timeout=12` 跑
     13.5~17.8s。客户端按 timeout 设的读超时会被超过，看起来像"服务端卡住"。
- 修复：
  - inbox **每轮都查**；junkemail / deleteditems 每 5 轮兜底扫一次（命中率极低，
    但保留兜底，防止验证码被误判进垃圾箱时永远取不到）。
  - 轮间 `sleep` 4s → **1s**（本轮有超时才退避到 3s），且等待时间封顶到剩余预算。
  - 超时判定后立即退出，不再多睡一整段。
- 实测（同一批 25 账号、同样 12s 窗口）：**轮次 2.9 → 10.3（3.5x）**；
  `timeout=5` 的实际耗时 **8.0s → <6s**。
- 测试：`tests/providers/test_poll_cadence.py`（3 例）+ `test_poll_latency.py`（2 例）。
  用旧代码跑会稳定失败（轮次 3 < 8；timeout=5 跑成 8.0s）。

**关于"共用同一个 client_id"**

- 这不是瓶颈。所有号共用 `cid=9e5f94bc`（Thunderbird 公开 client_id）确实存在，
  但微软的限流是按 **token/账号** 而非按 client_id 计，实测 25 并发 84 QPS 无 429。
- 换成每号独立 client_id 反而有风险：`client_id` 与 refresh_token 是绑定的，
  换 cid 会让已有 rt 直接失效（`invalid_client`），要全池重授权。

---

## [1.4] - 2026-11-21

### 修复

**并发取号误报 `D0002 邮箱库存不足`（库存其实充足）**

- 现象：池子里 828 个可发号、`chatgpt001` 自己还剩 710，但压测成功率卡在 8：
  `并发 5 → 5/5`、`并发 9 → 8/9`、`并发 10 → 8/10`、`并发 20 → 8/20`，
  而同一项目**串行 12 → 12/12**。上限恰好等于每轮探测批量 `PROBE_PER_ROUND = 8`。
- 根因：`claim_account` 是**先验 token、再原子占位**。并发线程会读到同一批 8 个候选、
  一起刷新 token，只有一个人抢得到；抢输的线程看到「本轮没有死号」就 `break`
  掉整个循环 → 库存充足却直接报无号。
- 现在：**先原子占位（`UPDATE ... WHERE pool_status='available'`），占到了再验号**。
  抢输的线程直接看下一个候选，不浪费 token 刷新；验出死号照样标 `retired/dead`
  并继续扫下一批（`MAX_ROUNDS` 从 5 提到 20）。
- 顺带：`count_claimable` 与 `claim_account` 口径保持一致，页面库存数不再是「假的总量」。
- 测试：`tests/api/test_claim_concurrency.py` 3 例（并发 20/* 全部拿到号 /
  只有 5 个号时如实超发数为 0 / 一整批死号后继续扫）。用旧代码跑该测试会
  稳定复现 7/20 误报 `no_stock`。
- 实测（生产镜像，828 库存池）：修复前并发 20 → 8/20，修复后同条件 20/20。

**第二个瓶颈：并发 50 时连接池耗尽（HTTP 500）**

- 现象：修完上面的竞态后并发 20 已 20/20，但 **并发 50 → 29/50**，
  失败全是 `HTTP 500`，而且每个请求都卡满 75s/110s。
- 根因：SQLAlchemy 默认 `pool_size=5 + max_overflow=10 = 15` 条连接。
  取号是同步阻塞路由（跑在线程池里），`claim_account` 在**持着连接做网络调用**
  （刷新 token、抓收件箱基线），50 并发时连接被占满，其余请求等 30 秒后抛
  `QueuePool limit of size 5 overflow 10 reached`。
- 修复：
  - `claim_account` 预检前 `db.expunge(acc)`，让 commit 先把连接还池；
    预检结果改用 `UPDATE` 写回（顺带修了一个**真 bug**：预检轮换出的
    新 `refresh_token` 原来根本没落库）。
  - `snapshot_mail_baseline` 在 Graph 网络调用前 `db.commit()` 还连接。
  - 连接池放宽到 `pool_size=20 / max_overflow=40 / pool_timeout=30`，
    加 `pool_recycle=1800` + `pool_pre_ping`（SQLite+WAL 读并发足够）。
- 实测（1.4.1 生产镜像）：并发 50 → **50/50（11.8s）**，
  并发 80 → **80/80（12.9s）**。修复前同条件 29/50（112s）。

**`database.py` 的 SQLite `busy_timeout` 修复未入库**

- 线上升级过的镜像里有 `connect_args timeout=30` + `PRAGMA busy_timeout=30000`
  （修 `database is locked` 的那次改动），但**没有进任何 git commit**，
  本地/CI 重新构建会把该修复丢掉。现已补回并纳入版本管理。

---

## [1.3] - 2026-11-21

### 修复

**超时占用只在「有人取号」时才回收，池子一闲就永久占库存**

- 现象：`recover_abandoned_claims` 只在 `claim_account` 里顺带调用。生产实测 49 个 `using`
  里 38 个早过了 15 分钟租约仍在占用 —— 每个项目的库存都被白扣 38 个。
- 现在：`main.py` 加独立定时任务 `_reclaim_expired_claims_job`（`CronTrigger(minute="*")`，
  每分钟一次，不依赖配置窗口、也不依赖有没有人取号），回收时打日志
  `超时占用回收: N 个号回池（租约 15 分钟）`。
- `claim_account` 里的那次顺带调用保留（双保险，回收逻辑本身幂等）。
- 测试：`tests/api/test_claim_recovery.py` 4 例（只收超租约的 / 旧 status 同步 /
  定时任务能独立回收 / 回收后其他项目库存可见）。

---

## [Unreleased]

### 变更

**邮箱复用改为「按项目一次性」**

- 之前：邮箱被**任何**项目用过就全局作废（释放也不回池），号池消耗极快
- 现在：A 项目用过的邮箱只有 A 不能再取，**其他项目照常可取**，释放即回池
- `email_usages` / `email_orders` 唯一约束从 email 单列改为 (email, project_code) 组合，`init_db` 自动迁移老库（数据不动）
- `/api/product/info` 的 `stock` 按项目各自统计（每个项目只扣自己用掉的）
- 计费 `charge`/`release` 按项目匹配订单，防同邮箱多项目订单串单

**移除临时邮箱源**

- 删除 MailNest 临时邮箱 / CF Worker 临时邮箱 / TempMail.lol 三个 provider（代码 + UI + 文档）
- 邮箱源只剩 Outlook 私有号池

---

## [1.1] - 2026-11-18

### 新增

**计费体系（充值抄 new-api，扣费抄 MailNest）**

- 金额一律用**整数厘**存储（`MONEY_SCALE = 1000`），杜绝 float 累加漂移，取整走 `ROUND_HALF_UP`
- 新增 6 张表：`users`、`api_keys`、`recharge_orders`、`redeem_cards`、`email_orders`、`balance_logs`
- **三段式扣费**（MailNest 模型）：取号只冻结（`frozen += 单价`，`balance` 不变，订单 `pending`）→ 收到验证码才真扣（`charged`）→ 释放或超时只解冻（`free`）
- 冻结超时自动释放（默认 1200 秒，调度器每分钟检查），防止用户取了号不收码把余额锁死
- **充值**（new-api 模型）：易支付（epay）MD5 签名下单 + 回调幂等到账、`trade_no` 唯一约束、充值卡批量生成/兑换、管理员人工补单/置失败
- 回调流程：验签 → 查单 → **校验金额** → 幂等状态检查 → 加钱写流水（金额校验必须在状态检查之前，防篡改金额的重复回调）
- 客户自助页：`/user/login` 登录、`/recharges`（余额与充值）、`/balances`（费用明细）
- 管理页：`/billing/users`（用户余额 + 流水 + 调整 + 签发 Key）、`/billing/cards`（充值卡生成 + 计费配置）
- API Key 绑定用户身份，扣费自动记到该用户名下；老的裸 Key 视为匿名
- 充值接口认两种身份：绑定用户的 API Key **或**网页登录会话（客户在后台页面上自己充值）
- `D0001` 由「预留位」改为真实的**余额不足**（不发货），新增 `D0007` 充值/支付流程错误
- 计费**默认关闭**（`billing.enabled = false`），老部署行为完全不变
- 余额六栏（balance/available/frozen/scan/card/award）全部从 `balance_logs` 流水推导，流水是唯一事实来源
- `DELETE /api/admin/billing/users/{id}?force=true` 删用户（连带 Key/流水/订单；有余额时默认 400，要清零须显式 force）
- 设计文档：`docs/billing.md`；参考源码：`docs/reference/`
- 生产部署：CI 构建镜像 → 服务器 `upgrade.sh` 拉取重建（备份 + 探活 + 失败回滚），详见 `docs/deployment.md`

**测试**

- `tests/api/test_billing.py` —— 16 个计费端到端用例（临时库 + 保险丝，绝不碰生产库）
- `tests/api/test_billing_ui.py` —— 4 个 Playwright 真实页面交互用例（自起隔离服务）
- `tests/api/test_billing_admin_extra.py` —— 6 个管理 API 补充用例（删用户/重名/零金额/充值边界）
- 测试库隔离统一到 `tests/conftest.py`（之前各模块各设各的 `MAILPOOL_DATA` 会互相覆盖，保险丝直接炸）
- 全量 `python -m pytest tests/ -q` → **45 passed**

### 修复

**测试污染生产库（高危）**

- `tests/api/test_mailpool_db.py` 原来是「脚本」：import 时直连**生产库** `data/mailpool.db` 并 `exit()`，
  pytest 收集到它会 `INTERNALERROR`，整套测试根本跑不起来。
  改为正经 pytest 用例：只读、复制生产库到临时目录再查、失败用 assert 报告。
- `tests/api/test_mailpool_full.py` 更危险：`BASE` 写死 `127.0.0.1:18080`，
  会 **PATCH 生产账号的标签**、还触发 **995 个号的全量刷新**。
  改为自起 uvicorn 子进程 + 独立临时库，种子数据走应用自己的导入 API，用例只碰自己的种子数据。

### 变更

- 管理视角的「下单流水 / 费用明细 / 余额与充值」现在看**全部客户**的数据（原来只按自己的 user_id 过滤，管理员看是空的）
- 「累计充值」改为由 `credit()` 统一维护（充值/兑换/赠送/管理员手动充值都计入）

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
