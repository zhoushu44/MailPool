# 账号错误处理修复记录（2026-11-15）

针对「账号出错后无法处理」的问题，修复 5 处。

---

## 修复前的问题

| # | 问题 | 后果 |
|---|---|---|
| 1 | `429` 限流、`403` 被归类为 `risk_control` | 攒够 6 次 → `dead` → 下次扫描**物理删除**，一次限流批量丢号 |
| 2 | `claim_account` 只排除 `dead` | 标记「异常」的账号照样发给客户，取件必失败 |
| 3 | Device Code Flow 重授权后 `pool_status` 仍是 `retired` | 授权成功但账号永远取不到 |
| 4 | 死号没有任何恢复途径 | 重新导入被当重复跳过，只能手动改库 |
| 5 | `_purge_dead_risk_control` 每次扫描自动删 `dead+risk_control` | 误判即永久丢号 |

---

## 修复内容

### 1. 错误分类重写（`pool_service.py`）

```python
_RISK_CONTROL_TOKENS = ("service abuse mode", "service_abuse", "aadsts70000", "service abuse")
_TRANSIENT_TOKENS   = ("429", "too many requests", "rate limit", "throttl",
                       "403", "forbidden", "timeout", "timed out", "connection",
                       "network", "503", "502", "504", "temporarily", "try again",
                       "连接", "超时")
```

新增分类 `transient`（临时性错误），并新增严格判定 `is_risk_control()`。

**关键行为**：`transient` 错误只标记 `unhealthy`，**不累加到 dead 阈值**。

| 错误 | 修复前 | 修复后 |
|---|---|---|
| `HTTP 429 Too Many Requests` | `risk_control` → 可能删除 | `transient` → 只标记异常 |
| `HTTP 403 Forbidden` | `risk_control` | `transient` |
| `HTTP 503` | `check_failed` → 累计判死 | `transient` |
| `service abuse mode` | `risk_control` | `risk_control`（不变） |
| `invalid_grant` | `credential` | `credential`（不变） |

验证：9/9 分类正确；连续 8 次 429 后仍是 `unhealthy`，没有变 `dead`。

### 2. 异常账号不再发给客户（`claim_account`）

```python
CLAIMABLE_HEALTH_STATUSES = ("healthy", "unknown")
...
EmailAccount.health_status.in_(CLAIMABLE_HEALTH_STATUSES)   # 原 not_in(("dead",))
```

`count_claimable` 同步改成同一口径（否则页面显示有货但实际取不到）。

验证：把账号设为 `unhealthy` 后，可 claim 数从 995 → 994，`claim_account` 发放的是另一个号。

### 3. Device Flow 重授权自动复活（`poll_device_code_flow`）

授权成功且账号为 `retired` 时：
- 若该邮箱**从未被任何项目使用过**（`email_usages` 无记录）→ `pool_status` 拉回 `available`
- 若已被使用过 → 保持 `retired`（用过即作废的规则不放宽），并记日志

验证：`retired + dead` → 授权后 `available + unknown`。

### 4. 新增恢复能力

**单个恢复 API**：`POST /api/admin/emails/{id}/revive`
- 重置为 `available / unknown`，清零 `fail_count`、`health_reason`
- 返回 `used_by` 提示该邮箱被哪个项目用过

**批量恢复 API**：`POST /api/admin/emails/revive-batch`
```json
{"provider":"outlook","health_reason":null,"dry_run":true,"limit":5000}
```
- **默认 `dry_run: true` 只预览**，必须显式传 `false` 才落库
- 可按 `health_reason` 精确恢复（如只恢复 `transient` 的）

**重新导入自动复活**：导入时若邮箱已存在且状态是 `retired`/`dead`/`unhealthy`
→ 更新凭证 + 重置为 `available/unknown`，而不是简单跳过。
导入结果新增 `revived` 字段，提示文案会显示「复活 N 条（原失效/退役）」。

**UI**：
- 账号页每行：失效/退役账号多一个绿色「恢复」按钮
- 账号页工具栏：「批量恢复失效」按钮（先预览再确认）
- 池状态筛选新增「已退役」选项

验证：
- 单号恢复：`{"pool_status":"available","health_status":"unknown","fail_count":0}`
- 批量恢复：预览 `candidates:1` → 执行 `revived:1`
- 重新导入：`inserted:0, revived:1`，凭证已更新
- UI 点击「恢复」→ toast「已恢复：xxx（available / unknown）」

### 5. 风控账号不再自动物理删除

```python
def _purge_dead_risk_control(...):
    # 原：delete_account(...)  →  现：只统计，不删除
    purged.append({... "action": "marked_only_需人工确认删除"})
```

`_apply_check_result` 的 `auto_delete_risk` 默认值从 `True` 改为 **`False`**，
所有调用点同步改为 `False`。真风控仍然标记 `dead + retired`，但**不再删除**。

扫描返回新增 `need_review` 字段（待人工确认数量）；pool 页扫描结果里 `dead` 状态显示为「⛔ 失效（未删除）」。

---

## 修复后的状态流转

```
检测失败
  ├─ transient（429/403/网络/5xx）→ unhealthy，不计入 dead 阈值
  ├─ credential（invalid_grant/401）→ fail_count+1
  └─ risk_control（service abuse）  → dead + retired（不删除）
          ↓
     fail_count ≥ 6 → dead + retired（不删除）
          ↓
恢复手段：
  ① 账号页「恢复」按钮 → available/unknown
  ② 「批量恢复失效」→ 先预览再批量重置
  ③ Device Flow 重授权 → 自动回池（未被使用过时）
  ④ 重新导入同一邮箱 → 更新凭证并复活
  ⑤ 检测成功 → 自动从 retired 拉回 available
```

---

## 回归验证

| 项 | 结果 |
|---|---|
| 错误分类 9 个样本 | **9/9 正确** |
| 连续 8 次 429 | 保持 `unhealthy`，未变 `dead` ✅ |
| 真风控（service abuse） | 标记 `dead`，未删除 ✅ |
| 恢复健康后自动回池 | `retired/dead` → `available/healthy` ✅ |
| `unhealthy` 不发放 | 可 claim 995→994，发给其他号 ✅ |
| 恢复 API（单号） | 200，状态正确 ✅ |
| 批量恢复 dry_run | 预览 1 个 → 执行 revived=1 ✅ |
| 重新导入复活 | `revived:1`，凭证已更新 ✅ |
| UI 恢复按钮 | toast 正确，无 JS 错误 ✅ |
| 全页面回归（12 页 + 移动端） | **PASS 52 / FAIL 0** ✅ |

数据状态：995 个账号全部 `available + healthy`，失败计数全 0，项目 53 个。
