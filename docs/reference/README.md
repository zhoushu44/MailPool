# 参考实现源码（充值抄 new-api，扣费抄 MailNest）

本目录是设计 MailPool 计费体系时抓下来的**第三方参考源码**，只读，不参与构建。
用法：查字段契约、查接口签名、查幂等处理顺序。

来源：

- **new-api**（Go）— `https://github.com/QuantumNous/new-api`
- **MailNest**（Vue 前端 chunk）— `https://mailnest.top`

---

## new-api —— 充值侧参考

| 文件 | 看什么 |
|---|---|
| `newapi_model_topup.go` | `TopUp` 模型：trade_no 唯一约束、amount / money 双字段、状态机 |
| `newapi_controller_topup.go` | 充值回调处理：**验签 → 查单 → 校验金额 → 幂等状态检查 → 加额度** |
| `newapi_model_user.go` | `User` 的 quota 字段、`max_quota` 溢出保护 |
| `newapi_model_log.go` | `Log` 类型枚举（充值 / 消费 / 系统） |
| `newapi_model_redemption.go` | 兑换码模型（card_key 唯一） |
| `newapi_controller_user.go` | 用户管理、额度调整、手动充值 |
| `newapi_controller_misc.go` | 手动 top-up 接口（对应本项目的 `AdminCompleteTopUp`） |
| `newapi_router_api-router.go` | 路由与中间件挂载顺序 |
| `newapi_common_constants.go` | 常量与错误码 |

**抄的核心**：回调的**幂等顺序**。new-api 是「先查单 → 校验金额 → 再看状态」，
金额校验必须在状态校验之前，否则篡改金额的重复回调能混过去加钱。

## MailNest —— 扣费侧参考

| 文件 | 看什么 |
|---|---|
| `mailnest_recharges.js` | 充值页：余额六栏、卡密兑换、扫码充值、订单列表 |
| `mailnest_balances-D-pX0nl6.js` | 费用明细页：流水表格（变动前/后余额与冻结） |
| `mailnest_orders-BWFchRw2.js` | 下单流水页：单价 + 计费状态 |
| `mailnest_chunks.txt` | 抓下来的 chunk 清单（哪些文件是哪些页面） |

**抄的核心**：**三段式扣费**。

| 阶段 | 触发 | 余额 | 冻结 | 订单状态 |
|---|---|---|---|---|
| hold | 取号 | 不变 | +单价 | pending |
| charge | 收到验证码 | -单价 | -单价 | charged |
| release | 释放 / 超时 | 不变 | -单价 | free |

余额六栏字段契约：`balance` / `available_balance` / `frozen_balance` /
`scan_recharge` / `card_recharge` / `award_amount`。

---

## 与本项目实现的对应关系

| 参考 | MailPool 实现 |
|---|---|
| new-api `TopUp` | `app/models.py` → `RechargeOrder`（`trade_no` UNIQUE） |
| new-api 兑换码 | `app/models.py` → `RedeemCard`（`card_key` UNIQUE） |
| new-api 回调幂等顺序 | `app/services/payment_service.py` → `process_notify()` |
| new-api 手动 top-up | `app/api/billing_admin_api.py` → `POST /orders/complete` |
| MailNest 三段式 | `app/services/billing_service.py` → `hold()` / `charge()` / `release()` |
| MailNest 余额六栏 | `app/services/billing_service.py` → `balance_snapshot()` |
| MailNest 日志类型 | `app/models.py` → `LOG_*` 常量 |

设计文档见 [`../billing.md`](../billing.md)。
