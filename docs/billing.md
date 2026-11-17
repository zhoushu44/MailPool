# MailPool 计费体系设计

> 充值侧抄 new-api，扣费侧抄 MailNest。
> 取号不扣钱，只冻结；收到验证码才真扣。

## 0. 一句话

| 环节 | 参照 | 实现位置 |
|---|---|---|
| 充值（扫码 / 充值卡 / 管理员补单） | new-api | `app/services/payment_service.py` |
| 扣费（冻结 → 扣款 → 释放） | MailNest | `app/services/billing_service.py` |

## 1. 金额单位：整数厘

```python
MONEY_SCALE = 1000   # app/models.py
```

**为什么不用 float / Decimal：**

- `0.018 * 1000 = 17.999999999999996` —— float 累加必然漂。
- SQLite 的 `Numeric` 实际存的是 float（SQLAlchemy 2.x 对 SQLite 会警告并退化成 float）。
- 银行卡/支付网关对账要求分厘精确，取整必须用 `ROUND_HALF_UP`（银行家舍入会把 0.0005 舍成 0，丢钱）。

所以：**库里一律存整数厘**，只在渲染层转字符串。

```python
to_milli("0.018") -> 18         # Decimal + ROUND_HALF_UP
fmt_milli(18)    -> "0.018"     # 3 位小数
```

## 2. 数据模型

| 表 | 关键字段 | 说明 |
|---|---|---|
| `users` | `username` 唯一, `password_hash`, `role`, `status`, `balance`, `frozen_balance`, `total_recharged`, `total_charged` | 客户账户 |
| `api_keys` | `user_id`, `key` 唯一, `enabled`, `last_used_at` | 绑定用户的 Key（计费身份） |
| `recharge_orders` | `trade_no` **唯一**, `amount`, `money`, `pay_type`, `pay_provider`, `status` | 充值订单（new-api TopUp） |
| `redeem_cards` | `card_key` 唯一, `batch_id`, `amount`, `status`, `used_by` | 充值卡（new-api Redemption） |
| `email_orders` | `email` 唯一, `price`, `billing_status`, `hold_amount`, `charged_amount` | 取号订单 + 计费状态 |
| `balance_logs` | `log_type`, `amount`, `balance_before/after`, `frozen_before/after` | 余额流水（不可变账本） |

### 2.1 关键约束

- `recharge_orders.trade_no` 唯一 —— new-api 的幂等基石。
- `redeem_cards.card_key` 唯一 —— 一卡一用。
- `email_orders.email` 唯一 —— 一个邮箱一条订单，`charged` 幂等靠它。

## 3. 充值侧（new-api 风格）

### 3.1 扫码充值（易支付 / epay）

```
POST /api/v1/recharge/create  {amount, pay_type}
  → 建 RechargeOrder(status=pending, trade_no=MP{uid}NO{ts}{rand})
  → 签 MD5：把参数按 key 升序拼成 k=v&k=v，末尾拼 &key={商户KEY}，md5 小写
  → 返回 {url, params, key:"urlscheme"}，前端自动提交表单跳网关
```

回调：

```
POST /api/v1/recharge/notify   （public_router，无鉴权）
  1. 验签 → 不过直接 D0007
  2. 按 trade_no 找订单 → 找不到 D0003
  3. 校验金额与订单一致 → 不一致 D0007   ← 必须在状态检查之前！
  4. 状态检查：已 success → 返回"重复回调，已处理"（幂等）
  5. 加余额 + 写流水 + 订单置 success
```

> **第 3 步的顺序是老子踩过坑的**：一开始金额校验写在状态检查后面，结果篡改金额的重复回调能混过。加钱之前必须先验金额。

金额校验规则（对齐 MailNest 前端 `^\d+(\.\d{1,2})?$`）：
- 只允许 1~2 位小数
- `min_topup` ≤ amount ≤ `max_topup`（默认 1 ~ 1000 元）

### 3.2 充值卡

```
管理员：POST /api/admin/billing/cards/generate  {amount, count}
  → 生成 count 张卡，batch_id 形如 B{ts}{rand}
  → 卡密形如 MP{金额分}-{8位}-{8位}

用户：POST /api/v1/recharge/redeem-card  {card_key}
  → 卡不存在 → D0007
  → 已使用 → D0007（"该卡密已被使用"）
  → 否则置 used，加余额，写 redeem 流水
```

### 3.3 管理员补单

```
POST /api/admin/billing/orders/complete  {trade_no}
  → 幂等：已 success 直接返回
  → 加余额 + 流水 + 置 success
```

> 人工补单是 new-api `AdminCompleteTopUp` 的翻版，跨境/ crypto 收款走这条。

## 4. 扣费侧（MailNest 三段式）

```
                 取号 buy                    收码 receive            释放 release
                    │                            │                        │
   balance ────────┼──── 不变 ────────────────► │                        │
   frozen  ── +P ──►│─────────────────────────► │                        │
                    │                            ├── -P ──► balance       │
                    │                            └── -P ──► frozen        │
   order   ─────────┴──── pending ─────────────► │                        │
                    │                            └── charged              │
                    │                                                     │
                    └────────────── release ◄───────────────────────────┘
                                     frozen -P, balance 不变, order=free
```

- **hold**（`billing_service.hold`）：`frozen += P`，`balance` 不动，订单 `pending`。
- **charge**（`billing_service.charge`）：`frozen -= P`，`balance -= P`，订单 `charged`。
- **release**（`billing_service.release`）：`frozen -= P`，`balance` 不动，订单 `free`。

### 4.1 超时自动释放

`app/main.py` 的调度器每分钟跑 `_release_expired_job`：

```python
billing_service.release_expired(db, timeout_seconds=cfg["hold_timeout"])  # 默认 1200 秒
```

避免用户取了号不收码，冻结额一直挂着把余额锁死。

### 4.2 幂等

`charge` 判的是订单状态：只有 `pending` 能转 `charged`。重复收码不重复扣。

## 5. 身份与鉴权

三层：

1. **管理员**：`MAILPOOL_ADMIN_PASSWORD`（Cookie 登录或 `X-Admin-Password` 头），能进所有后台页。
2. **客户**：网页登录（`/user/login`，PBKDF2-SHA256 200k 轮），会话里存 `user_id`。
3. **API Key**：`api_keys` 表里绑了 `user_id` 的 Key → API 调用带这个 Key 就等于该用户，扣费记他头上。老的 `settings.api_keys` 里的裸 Key 视为匿名。

**充值接口认两种身份**（`require_api_key_or_session`）：绑定用户的 API Key **或**网页登录会话。
对外客户端用 Key，客户在「余额与充值」页自己充值用会话。两者都没有 → 401。

客户自助页（`/recharges`、`/balances`）只要客户登录就能进；管理页（`/billing/*`、`/users`）必须管理员。

**管理员与客户身份可以共存**：管理员登录时合并会话而不是覆盖，所以后台登录后切到客户身份仍然能看自己的余额。

## 6. 接口一览

### 客户 API

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/v1/recharge` | 余额总览（6 栏：balance/available/frozen/scan/card/award） |
| GET | `/api/v1/recharge/channels` | 支付方式列表 |
| POST | `/api/v1/recharge/create` | 创建充值订单，返回网关跳转参数 |
| GET | `/api/v1/recharge/query?id=` | 查单笔充值订单 |
| GET | `/api/v1/recharge/orders` | 充值订单列表 |
| POST | `/api/v1/recharge/redeem-card` | 充值卡兑换 |
| POST | `/api/v1/recharge/notify` | 支付回调（无鉴权） |

### 管理 API

| 方法 | 路径 | 说明 |
|---|---|---|
| GET/POST | `/api/admin/billing/users` | 用户列表 / 建用户 |
| POST | `/api/admin/billing/users/{id}/balance` | 调整余额（正充负扣） |
| GET | `/api/admin/billing/users/{id}/logs` | 用户流水 |
| GET/POST/DELETE | `/api/admin/billing/api-keys` | Key 列表 / 签发 / 吊销 |
| POST | `/api/admin/billing/cards/generate` | 批量生成充值卡 |
| GET | `/api/admin/billing/cards/query` | 查卡密 |
| GET | `/api/admin/billing/orders` | 充值订单列表 |
| POST | `/api/admin/billing/orders/complete` | 人工补单 |
| POST | `/api/admin/billing/orders/fail` | 人工置失败 |
| GET/PUT | `/api/admin/billing/config` | 计费配置 |

## 7. 配置项（存 settings 表，key = `billing.<name>`）

| key | 默认 | 说明 |
|---|---|---|
| `enabled` | `false` | 计费总开关。**默认关闭**，老部署行为不变 |
| `default_price` | `0.018` | 默认单价（元/个），项目可覆盖 |
| `min_topup` | `1.00` | 单笔最小充值 |
| `max_topup` | `1000.00` | 单笔最大充值 |
| `hold_timeout` | `1200` | 冻结超时秒数 |
| `epay_url` / `epay_pid` / `epay_key` | 空 | 易支付网关配置（空则只支持充值卡） |
| `site_url` | 空 | 本站对外地址，用于拼回调 URL |
| `award_percent` | `0` | 充值赠送百分比 |

## 8. 上线步骤

1. 后台「充值卡」页填易支付网关地址 / PID / KEY，填本站对外地址（回调地址必须能被网关访问到）。
2. 「计费配置」把 `enabled` 打开。
3. 「用户余额」页建客户，签发绑定 Key。
4. 客户端用 `Authorization: Bearer <key>` 调用，扣费自动记客户账上。
5. 客户可访问 `/user/login` 自行查余额、充值。

## 9. 测试

| 文件 | 覆盖 |
|---|---|
| `tests/api/test_billing.py` | 16 个用例：金额换算、epay 签名/验签、回调到账+幂等、坏签名拒绝、金额不符拒绝、充值卡、补单幂等、hold→charge→幂等、hold→release、余额不足不发货、超时释放、流水类型、登录与访问控制 |
| `tests/api/test_billing_ui.py` | 4 个 Playwright 用例：真页面建用户/签 Key/调余额/查流水/生成卡/查卡/存配置/客户登录/客户兑换 |

两个套件都用**临时数据库**：

- API 套件在 import `app.*` 之前设 `MAILPOOL_DATA`，并有 `_assert_isolated` 保险丝 —— engine 指向生产库就直接拒绝跑测试。
- UI 套件自己起 uvicorn 子进程（独立 `MAILPOOL_DATA` + 随机端口）。

> `app.config` 在 import 时就把 `DATA_DIR` 定死了。`MAILPOOL_DATA` 设晚了，测试就会直接写进生产 `data/mailpool.db` —— 这是老子踩过的坑。

## 10. 参考实现

抓取到的源码在 `docs/reference/`：

- `newapi_controller_topup.go` / `newapi_model_topup.go` —— 充值单模型 + 幂等回调
- `newapi_model_redemption.go` —— 充值卡
- `newapi_controller_user.go` —— 管理员补单
- `newapi_common_constants.go` —— 支付方式常量
- `mailnest_recharges.js` / `mailnest_balances-*.js` / `mailnest_orders-*.js` —— MailNest 前端（字段名、校验规则、状态枚举的来源）
