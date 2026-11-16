# MailPool 修复记录（2026-11-15）

对照 `docs/test-report-2026-11-15.md` 的缺陷清单，本次修复 11 项，全部已回归验证。

---

## P0 阻断级（全部修复）

### 1. 取码主链路必然失败并烧号
- **文件**：`app/services/mail_service.py`
- **改法**：
  - `receive_code()` 内新增号池凭证注入：`pool_service.get_account_credentials(db, email)` → `prov.set_credentials(email, cred)`
  - 无凭证时抛**非致命** `ProviderError`（原先是 fatal，直接烧号）
  - 新增 `_is_credential_fatal()` 白名单：只有 `invalid_grant` / `service abuse` / `aadsts70000` / `IMAP 登录失败` 等明确凭证失效才 `mark_dead`
- **验证**：
  ```
  buy x2 → 200 ['sglfopzhgz5n@outlook.com', 'nicholethorntontr7956f@outlook.com']
  receive → 200 status=success code='812273' 1.4s
  receive → 200 status=success code='542085' 1.5s
  release → 200
  ```
  修复前：`{"code":"D0006","msg":"账号被封禁 取件失败"}` 0.0s + 账号被打成 dead

### 2. 健康扫描 100% 500
- **文件**：`app/services/pool_service.py`
- **改法**：新增 `_naive_utc()` 统一 aware/naive；`cache_cutoff` 与 `last_check_at` 比较、`recover_abandoned_claims` 的 `cutoff` 全部走该函数
- **验证**：`POST /api/admin/emails/health {"force":false}` → 200（`{"checked":0,"skipped":6}`，7 天缓存正常生效）

### 3. 项目新建/编辑表单完全不可提交
- **文件**：`app/web/templates/projects.html:239-296`
- **改法**：`this.f_config` 等 → `document.getElementById('f-config')`；补 JSON 解析 try-catch 与 `code` 必填校验
- **验证**：填表点保存 → 列表出现新项目（0 JS 错误）；删除按钮正常

### 4. `|tojson` 塞进双引号 onclick（659 个失效按钮）
- **文件**：`buy_email.html:47,87`、`pool.html:101`、`projects.html:120`
- **改法**：`onclick="fn({{ x|tojson }})"` → `onclick='fn({{ x|tojson }})'`（单引号属性）
- **验证**：`new Function()` 语法检查
  - buy-email：108 个按钮 **0 语法错误**（原 106 个坏）
  - pool：抽查 200 个删除按钮 **0 错误**（原 500 个坏）

### 5. 收件箱正文永久空白
- **文件**：`app/web/templates/mailbox.html:312,721`
- **改法**：新增 `escAttr()`（在 `esc` 基础上补 `"` → `&quot;`、`'` → `&#39;`），`srcdoc` 改用 `escAttr(buildBodyHtml())`
- **验证**：`iframe.srcdoc` 长度 **41 → 4756**，`contentDocument.body.innerText` **0 → 610**

### 6. 后台默认无鉴权 + 会话密钥可伪造
- **文件**：`app/web/session.py`、`app/api/auth.py`、`app/main.py`
- **改法**：
  1. `session.py`：密钥改为 `MAILPOOL_SESSION_SECRET` → `data/.session_secret`（首次随机生成 64 字符并持久化，0600）→ `MAILPOOL_ADMIN_PASSWORD`，**删除硬编码 `"mailpool-dev-secret"`**
  2. `auth.py`：新增 `check_admin_access()`；未设口令时**仅放行回环地址**（`127.0.0.1`/`::1`/`localhost`），非本机返回 403
  3. `main.py`：新增 `admin_page_guard` 中间件，除 `/static`、`/docs`、`/api`、`/login`、`/logout` 外全部校验
- **验证**：
  - 旧密钥伪造 cookie → `BadSignature` 被拒（进程内验证）
  - 伪造 `X-Forwarded-For: 8.8.8.8` → **403**（不再信任可伪造头）
  - 本机访问 → 200（保持自用便利）

---

## P1 高优先级（全部修复）

### 7. `DELETE /emails/{id}` 重复注册
- `admin_api.py` 删除第二处重复路由，只保留一处，内部统一走 `pool_service.delete_account`
- 验证：`DELETE /api/admin/emails/999999` → 404 `{"detail":"账号不存在"}`（语义正确，不再是 code=404 的 200）

### 8. `release_account` 假成功
- `pool_service.py:490-502`：rowcount=0 时不再回落 `exists → True`，改为记日志 + 返回 False
- 验证：对 available 账号调 release → 404（原先回 `retired` 但状态没变）

### 9. 刷新历史 `detail` 硬截断成非法 JSON
- `pool_service.py:_save_refresh_log`：改为 `{summary, results}` 结构，只存异常项 + 20 条成功样本，保持 JSON 完整可解析（上限 200KB，超限再收缩但始终合法）
- `accounts.html`：面板新增失败明细渲染（兼容旧的纯数组格式）
- 验证：`json.loads(detail)` → `{ok: true, keys: ['summary','results']}`

### 10. 账号列表只显示 200 条无提示
- `accounts.html`：显式 `limit=500`，新增 `#listHint` 显示「共 995 条，当前仅显示最新 500 条」；0 结果显示「暂无匹配账号」
- 验证：行数 500，提示正确，空态正常

### 11. `data-confirm` 形同虚设
- `app.js`：改为**捕获阶段**监听 + `stopImmediatePropagation()`
- 验证：点「取消」→ **0 次请求**（原先照发 DELETE）

---

## 顺带修复

| 项 | 文件 | 改法 |
|---|---|---|
| 定时任务永不执行 | `main.py` | `CronTrigger(hour="*",minute=0)` → `minute="*/5"`；新增 `_in_window()` 判断配置窗口 |
| pool 页「未检测」取错字段 | `pool.html:11` | `counts.available` → `health.unknown` |
| pool 导入结果被 reload 抹掉 | `pool.html` | 结果存 sessionStorage，刷新后恢复；错误用 `esc()` 转义 |
| release 无二次确认 | `pool.html:161` | 加 confirm（提示永久退役） |
| 注册配置保存丢 6 个键 | `register/service.py` + `registrar.py` | 合并库中原值而非覆盖；`REGISTER_DEFAULTS` 补 `oauth_scope`/`oauth_tenant`/`bot_protection_wait` 等 |
| 注册任务永远提示"已在运行" | `register/service.py` + `register.html` | 新增 `started` 字段，前端据此区分 |
| 移动端横向溢出 | `style.css` + 4 个模板 | `.btn-row{flex-wrap:wrap}`、新增 900px/640px 断点、`.card{padding:14px}`、dashboard/pool/projects/settings/register 补 `.table-wrap` |
| 恶意/异常属性值截断 | `accounts.html` | 新增 `escAttr()`，onclick 参数改用 `JSON.stringify + escAttr` |
| CSS 缓存永久失效 | `base.html` + `routes.py` | 随机版本号 → `asset_version()`（文件 mtime） |
| 信任 HTML 开关点不动 | `mailbox.html:718` | `<span class="trust-row">` → `<label class="trust-row">` |

---

## 回归测试结果

| 项目 | 结果 |
|---|---|
| Python 编译 | `compileall app` exit 0 |
| 全页面冒烟（17 页 × 4 项） | **PASS 68 / FAIL 0**（HTTP 200、无 JS 错误、无 console 错误、无 5xx） |
| 响应式 375px / 768px（各 8~12 页） | **全部溢出 0px** |
| 取码 E2E（buy→receive→release） | 2/2 成功，1.4s / 1.5s 取到真实验证码 |
| 健康扫描非 force | 200（7 天缓存正常跳过） |
| 项目 CRUD（UI） | 创建 + 删除均成功，0 JS 错误 |
| 邮件正文 | srcdoc 4756 字符，正文 610 字符可读 |
| confirm 拦截 | 取消 → 0 请求 |

---

## 已知未修（P2，建议后续处理）

1. `_batch_tasks` / `_device_flows` 纯内存、无淘汰、多 worker 下 status 查不到
2. `list_accounts` 的 limit 在 Python 侧切片（全表加载）
3. `pool_service.py:712/893` 的 `time.sleep` 会让单请求最长十几分钟
4. 全站无 `aria-*` / `role`，弹窗无 focus trap
5. `buy_email.html` 两个 tab 各渲染同一批项目（106 张卡、HTML 125KB）
6. mailbox 三栏在窄屏仍是挤压而非堆叠（已不溢出，但体验一般）
7. 注册 `country` 等字段在页面上无对应控件

---

## 数据变更说明

修复验证过程中：
- 取码测试取走 2 个号后已手动回池（`pool_status=available`），删除对应的 `email_usages` 记录
- 项目/标签/备注测试数据已清理，项目数 53、账号数 995、无测试标签
- 注册配置已恢复为测试前的 28 个键原值
- 历史刷新日志 #7 的损坏 detail 已重建为合法 JSON 摘要（原失败明细不可恢复）
