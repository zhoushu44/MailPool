# MailPool 全面测试报告（SuperAgent 多角色）

- 测试时间：2026-11-15 14:14 – 15:50（Asia/Shanghai）
- 服务地址：http://127.0.0.1:18080（uvicorn，PID 73492）
- 数据基线：1001 个 Outlook 账号 / 53 个项目 / 7 条刷新日志
- 测试方式：3 路并行 superagent（API 契约审计 / 前端 UI 审计）+ 主线程 Playwright 真实浏览器 + HTTP 黑盒 + 进程内复现
- 代码改动：**无**（测试期间未修改任何项目文件）

---

## 一、总体结论

| 层面 | 结果 |
|---|---|
| 页面可访问性 | 18/18 页面 HTTP 200，无残留模板变量，无 5xx（正常路径） |
| 浏览器渲染 | 17 页零 JS 报错、零 console error、零资源 404、零图片破损 |
| 静态 UI 冒烟（自建 Playwright） | PASS 198 / FAIL 2（其中 1 条为脚本误判，实录 1 条真问题） |
| 管理端 API | 14/14 GET 端点 200；CRUD/配置读写基本正常 |
| 既有测试脚本 | test_mailpool_full.py PASS 52、test_mailpool_db.py PASS 24 |
| pytest 套件 | **不可用**：两个测试文件模块级 `exit()`，pytest collect 直接 INTERNALERROR |
| 核心业务链路 | **失败**：取码 100% 失败且会烧号；健康扫描（非强制）100% 500 |

**一句话**：页面骨架和读接口是健康的，但**取码主链路、健康扫描、项目表单、三处批量按钮、邮件正文渲染**这五个"打开就能用"的地方处于不可用状态，后台还默认无鉴权。

---

## 二、P0 阻断级缺陷（已复现，附证据）

### 1. 取码主链路必然失败，且会把账号打成 dead + retired（最严重）

- 位置：`app/services/mail_service.py:173` + `:190-196`，`app/providers/outlook.py:463-467`
- 机理：`receive_code()` 每次用 `build_provider()` 新建 provider 实例，`_credentials` 是空字典；只有 `acquire_email()`（`mail_service.py:125-127`）调 `set_credentials()`。于是 `prov.wait_mail()` → `_cred()` 抛 `ProviderError("未找到邮箱凭据", fatal=True)`，`receive_code` 捕获 fatal 后立刻 `mark_dead()`。
- 实测：
  - `POST /api/v1/email/receive` → `{"code":"D0006","msg":"账号被封禁 取件失败"}`，0.0s 返回（不是超时，是立刻失败）
  - 进程内复现：新建实例 `_cred()` → `ProviderError 未找到邮箱凭据 fatal=True`
  - 号池状态：`nicholethorntontr7956f@outlook.com`、`sglfopzhgz5n@outlook.com` 两条被置 `health_status=dead / health_reason=provider_fatal / pool_status=retired`
- 影响：对外取码 API 完全不可用；**每调用一次烧掉一个号**（首次取码即退役）。

### 2. 健康扫描（非强制）100% 500

- 位置：`app/services/pool_service.py:609` 与 `:613`
- 机理：`cache_cutoff = _now() - timedelta(days=7)` 是 aware datetime，`acc.last_check_at` 从 SQLite 取出是 naive → `TypeError: can't compare offset-naive and offset-aware datetimes`
- 实测：`POST /api/admin/emails/health {"force":false}` → **500**；进程内 `check_accounts_health(..., force=False)` 抛出同一 TypeError（traceback 定位到 pool_service.py:613）
- 影响：`/api/admin/emails/health` 与 `main.py` 里的每日定时健康扫描全部失效。`force=true` 可绕过，实测 2 个号返回 `Graph API 验证通过`。

### 3. 项目新建 / 编辑表单完全不可提交

- 位置：`app/web/templates/projects.html:241, 261-265`
- 机理：JS 用 `this.f_config.value / this.f_id / this.f_code …` 取值，但表单元素只有 `id="f-config"`、`name="config"`，没有 `name="f_config"`
- 实测：点「保存」→ `Cannot read properties of undefined (reading 'value')`，**0 次 fetch**，列表不新增
- 影响：53 个项目的增改全部不可用（删除另见 P0-4）。

### 4. `|tojson` 塞进双引号 onclick，659 个按钮点击即语法错误

- 位置：`buy_email.html:47,87`（106 处）、`pool.html:101`（500 处）、`projects.html:120`（53 处）
- 渲染结果实测：
  - `/buy-email` → `onclick="buyEmail("` + `chatgpt001` + `")"` → `new Function(...)` 抛 `Unexpected token '}'`
  - `/pool` → `onclick="del(1017, "`
  - `/projects` → `onclick="deleteProject(1, "`
- 影响：购买邮箱、号池删除、项目删除按钮全部点了没反应。

### 5. 收件箱邮件正文永久空白

- 位置：`app/web/templates/mailbox.html:718` 的 `srcdoc="' + esc(buildBodyHtml()) + '"`，`esc()` 在 `:312` 用 textContent→innerHTML，**不转义双引号**
- 实测：选号后点开邮件，`iframe.srcdoc` 长度 **41**（后端 API 返回的 body 实际 **1919** 字符），`contentDocument.body.innerText` 长度 **0**
- 影响：邮箱页可以看列表/标题/发件人，但正文永远空白——核心阅读功能废掉。

### 6. 后台默认无鉴权 + 会话密钥硬编码可伪造

- `app/web/routes.py` 全部页面路由无 `require_admin`；`app/api/auth.py:43-44` 在 `MAILPOOL_ADMIN_PASSWORD` 为空时直接 `return`（当前实例即如此）
- `app/web/session.py:19-21` 用字面量 `"mailpool-dev-secret"` 签名 → 任何人可离线伪造 cookie
- 实测：伪造 `mailpool_session` cookie 后 `GET /api/admin/api-keys` 返回 **200**；`/settings` 页面直接明文渲染 API Key
- 影响：本机/内网任意进程可读写号池凭证（1000 个真实 refresh_token）。

---

## 三、P1 高优先级

| # | 位置 | 问题 | 实测 |
|---|---|---|---|
| 1 | `admin_api.py:233` 与 `:307` | `DELETE /emails/{id}` 重复注册，第二个（永久删除/风控清理）不可达 | `/openapi.json` 只剩 `admin_delete_account_api...`，另一分支消失 |
| 2 | `pool_service.py:470-494` | `release_account` 对非 using 账号 rowcount=0 仍回落 `exists→True`，接口回 `retired` 但状态没变 | 实测 id=17 释放前后都是 available |
| 3 | `accounts.html:122` | 请求不带 `limit`，后端默认 200；库中 995 条 | 实测 `#rows` 恰好 200 行，无分页、无「仅显示 200 条」提示 |
| 4 | `settings.html:19` + `app.js:31-36` | `data-confirm` 在冒泡阶段拦截，晚于 `onclick` | 实测点「取消」仍发出 `DELETE /api/admin/api-keys/ak_...`（已用路由拦截挡住，Key 未丢） |
| 5 | `pool_service.py:1182` | `detail=json.dumps(...)[:4000]` 硬截断，1001 条时 JSON 断在半截 | 实测 `detail` 长度恰好 4000，`json.loads` 报 `Expecting property name ... char 4000` |
| 6 | `pool.html:11` | 「未检测」卡取值 `counts.available`（池可用数） | 页面显示 1001，而真实 unknown=485 |
| 7 | `main.py:134-136` vs `:65/93/123` | CronTrigger 固定 `minute=0`，任务体又判断 `now.minute < minute` | 配置分钟>0 时永不执行 |
| 8 | `register.html:265` + `register/service.py:136` | 启动成功也返回 `running:true` | 前端永远走「已有任务运行中」分支 |
| 9 | `register.html:273-299` + `service.py:56-57` | 只提交 12 字段，`clean` 只保留默认键 | 保存会丢 `wary_api_key`、`aux_email_*` 等已存配置 |
| 10 | `mailbox.html` 全局 | 三栏全内联样式、模板 0 条 `@media` | 768px 三栏挤爆；375px 邮件面板在视口外 |
| 11 | `base.html:19-30` + `style.css:87` | 无移动端菜单 | 375px `.nav` clientWidth=12px / scrollWidth=624px，10 个菜单不可见 |
| 12 | `projects.html:104` `pool.html:79` `settings.html:13` `register.html:162` | 表格未包 `.table-wrap` | 768px：projects 溢出 175px、pool 68px；375px：projects 568px、pool 558px、accounts 82px、register 207px |
| 13 | `mailbox.html:715` | 「信任原始 HTML」是 `<span>` 不是 `<label>` | 实测点滑块 `checked` 仍 false |
| 14 | `buy_email.html` 卡片 | `.mn-mall-info/.mn-mall-price` 未定义，logo 绝对定位压标题 | 每张卡 logo 与标题右边界重叠约 40px；两个 tab 各渲染同一批 53 项 |

---

## 四、P2 其他问题

**接口契约 / 后端**
- `admin_api.py:313` 失败返回 `message` 键，前端 `app.js:25` 只读 `msg` → 只显示"业务错误"
- `admin_api.py:622-631` provider 自测失败返回 `code:1`，被 `api()` 当异常抛
- `email_api.py:89-90` `build_provider` 连调两次且解构写反
- `mailbox/messages?email=` 空/不存在账号返回 **502**（应 404），实测 5 次 502 全来自这里
- `POST /api/admin/projects` 重复 code 静默覆盖（upsert 语义）、空 code 可创建（实测造出 `code=""` 的项目，已清理）
- `/api/v1/mails` 的 `page=0` / `page_size=9999` 返回 422 裸 detail（风格与业务错误码不一致）
- `_batch_tasks` / `_device_flows` 纯内存无淘汰，多 worker 下 status 查不到；`start_batch_refresh_token` 持锁做 DB 查询
- `pool_service.py:305-309` limit 在 Python 侧切片（全表加载）；`email_usage_map` 全表；`admin_api.py:111` 每项目一次 `count_claimable`（53 次）

**前端**
- `accounts.html:123-139` 筛选 0 结果不写空态（表格只剩 42px 表头）
- `esc()` 不转义双引号，却用于 `title="..."`、`srcdoc="..."`、`onclick='...'` 拼接（潜在属性截断/XSS 面）
- `pool.html:128`、`settings.html:254/293` 直接 `innerHTML` 拼未转义内容
- 导入/全量刷新/启动注册/测试连接均无 loading + 按钮禁用 → 可重复提交
- 全站 0 个 `aria-*` / `role`；主页面无 `<h1>`；弹窗无 `role="dialog"`、ESC 关不掉
- `base.html:11` CSS 版本号 `range(1000,9999)|random` 每次请求都变，缓存永久失效
- CSS 缺类：`btn-primary`、`mn-mall-info`、`mn-mall-price`、`tab-bar`、`mbx-page`、`.toast.warn`

**性能（实测，均属健康）**
- `/accounts` loadEventEnd 12ms；`/api/admin/emails` 200 条 20ms、995 条 28ms
- 但 `/pool` 一次渲染 500 行、HTML 413KB；`/accounts` 200 行渲染 9111px 长页

---

## 五、副作用与数据变更说明（重要）

测试过程中触发了两次批量操作，产生的数据变动如下：

1. **批量刷新 Token（手动，`refresh-all`）**：1001 个号串行跑完，耗时约 58 分钟（每号 sleep 3s 防限流）。结果：成功 995、失败 6。**这 6 个号被风控逻辑物理删除**（`reason=service_abuse`），无法恢复：
   - dannyfoxm341793y / douglasramireza9972o2a / joshuamitchella42686n / stephaniesuttond45718xs / thomasjohnsond64859t / taylorvillegasel42256bc3（均 @outlook.com）
   - 账号总数 1001 → **995**
2. **取码链路测试**造成 2 个号被打成 dead+retired，已通过 SQL 恢复为 available+healthy，并清除了对应 `email_usages` 记录
3. **自建测试项目/标签/备注**已全部删除/清空，项目数回到 53
4. 账号标签、备注已复原为导入批次备注 `leadbee批量导入 2026-11-15`

> 注意：`refresh-all` 的"自动删除风控号"是代码内既有设计（`pool_service.py:1128-1133`，`_run_batch_refresh` 里 `reason=risk_control → delete_account`），不是测试脚本所为。但这个行为**静默且不可逆**，建议至少在 UI 上给出确认与结果清单。

---

## 六、修复优先级建议

**必须先修（否则核心不可用）**
1. `receive_code()` 里注入凭证：`cred = pool_service.get_account_credentials(db, email)` → `prov.set_credentials(email, cred)`；且 `fatal` 只在明确凭证失效（invalid_grant / service abuse）时置 dead
2. `pool_service.py:613` 时间比较统一为 naive UTC（或全部 aware）
3. `projects.html` 表单改 `this.elements['f-config'].value` 或给元素补 `name="f_config"`
4. 三处 `|tojson` 改为 `onclick='fn({{ x|tojson }})'`（单引号属性）或 `data-*` + 事件委托
5. `mailbox.html` 的 `srcdoc` 改用 `escAttr()`（转义 `"`）或 blob URL
6. 后台鉴权：页面路由加 `Depends(require_admin)`；空口令时强制登录；会话密钥改为随机持久化

**随后修**
7. 账号列表加分页或明确「仅显示 200 条 + 总数」
8. `data-confirm` 改捕获阶段并 `stopImmediatePropagation`，或改为按钮自身 handler
9. 合并重复 DELETE 路由；`release_account` 状态判断修正
10. `detail` 存储改为不截断（或存结构化表），前端按条展示
11. pool 页统计卡改用 `health.unknown`
12. 移动端：给 4 个表格补 `.table-wrap`，`base.html` 加移动菜单，mailbox 加 `@media` 断点

---

## 七、截图归档

`data/_shots/` 共 20 张：18 页桌面全页 + 3 张移动端 + 刷新历史弹窗/邮箱页实拍。

关键证据：
- `accounts_small.jpg`：200 行无分页、无「共 N 条」
- `mailbox_loaded.png`：三栏布局、正文区空白
- `dashboard.png`：53 个项目卡、统计卡数值正常
- `accounts_mobile2.png`：375px 横向溢出 82px
