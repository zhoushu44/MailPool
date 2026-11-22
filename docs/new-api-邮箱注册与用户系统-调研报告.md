# new-api（Calcium-Ion/new-api）邮箱注册 / 邮箱验证码 / 用户系统 调研报告

> 调研对象：`github.com/Calcium-Ion/new-api`（one-api 的 fork，现由 QuantumNous 维护，Go module 名 `github.com/QuantumNous/new-api`）
> 代码基线：`main` 分支快照（2026-11-20 抓取，`go.mod` = `go 1.26.10`）。
> 方法：本地 `docs/reference/*.go` 5 个文件已与上游逐一比对，**内容完全一致**（`api-router.go` 仅差 3 行 `DisableCache()`，与邮箱无关）；缺口部分从上游完整源码补齐。
> 标注约定：**【确认】** = 直接读到的代码；**【推测】** = 由代码推断、未运行验证。

---

## 0. 一句话结论

new-api 的"邮箱注册"本质是 **注册前先拿邮箱验证码**：`GET /api/verification?email=` 发 6 位码（存 **进程内 RAM map**，10 分钟有效），`POST /api/user/register` 时把 `verification_code` 一起提交校验。
它**没有** `email_verified` 字段、**没有**"邮箱验证码登录"、**验证码不落库也不进 Redis**、**限流只按 IP（2 次/30 秒）不按邮箱**。
数据库默认 **SQLite**，可用 MySQL / PostgreSQL（三选一，非"只能用 MySQL"）。

---

# A. 邮箱注册 / 验证码流程

## A1. 完整调用链（路由 → controller → 发信）

### 发码：`GET /api/verification?email=xxx&turnstile=xxx`

| 层 | 位置 | 动作 |
|---|---|---|
| 路由 | `router/api-router.go:46` | `apiRouter.GET("/verification", middleware.EmailVerificationRateLimit(), middleware.TurnstileCheck(), controller.SendEmailVerification)` |
| 限流中间件 | `middleware/email-verification-rate-limit.go:54-65` | 按 IP 固定/滑动窗口，2 次 / 30 秒 |
| 人机校验 | `middleware/turnstile-check.go` | `TurnstileCheckEnabled` 为真时 POST Cloudflare `siteverify`，`remoteip` 传 `c.ClientIP()` |
| Controller | `controller/misc.go:219-252` | `SendEmailVerification` |
| 校验邮箱策略 | `controller/misc.go:220` → `service/email_binding.go:36-55` | `ValidateAccountEmail`：格式 + 长度 ≤50 + 域名白名单 + 别名限制 |
| 查重 | `controller/misc.go:226-229` | `model.IsEmailAlreadyTaken(email)`（含软删用户，见 `model/user.go:319` 的 `Unscoped()`） |
| 生成码 | `controller/misc.go:230` | `common.GenerateVerificationCode(6)` |
| 存码 | `controller/misc.go:231` | `common.RegisterVerificationCodeWithKey(email, code, common.EmailVerificationPurpose)` |
| 渲染 | `controller/misc.go:234-241` | go-i18n 取 subject/3 段正文 |
| 发信 | `controller/misc.go:242` → `common/email.go:78-150` | `common.SendEmail(subject, email, content)` → `net/smtp` |

`GenerateVerificationCode`（`common/verification.go:26-33`）实现要点：
```go
code := uuid.New().String()            // 32 hex
code = strings.Replace(code, "-", "", -1)
return code[:length]                   // length=6 → 取前 6 个十六进制字符
```
**【确认】这不是 `rand.Int`，而是 UUIDv4 前 6 个 hex 字符**，字符集 `0-9a-f`，理论空间 16^6 = 16,777,216。前端 `OTP_REGEX = /^\d{6}$/`（`web/src/features/auth/constants.ts`）与后端不匹配 —— 因为注册页用的是普通 `<Input>`（`sign-up-form.tsx` 的 `verificationCode` 字段，无 `inputMode`/正则），所以能输入 a–f；但**若你照抄前端 OTP 组件会收不到码**。这是 new-api 的一个真实不一致点。

### 校验 + 建号：`POST /api/user/register`

| 层 | 位置 | 动作 |
|---|---|---|
| 路由 | `router/api-router.go:79` | `userRoute.POST("/register", middleware.CriticalRateLimit(), anonymousRequestBodyLimit, middleware.TurnstileCheck(), controller.Register)` |
| Controller | `controller/user.go:217-336` | `Register` |
| 开关 | `controller/user.go:218-225` | `RegisterEnabled` / `PasswordRegisterEnabled` |
| 反序列化 | `controller/user.go:226-231` | `DecodeJson(&user)` —— **直接绑定到 `model.User`** |
| 归一化 | `controller/user.go:232-233` | `TrimSpace(username)`、`NormalizeEmail(email)`（小写 + trim） |
| 必填 | `controller/user.go:234-237` | username 非空 |
| 结构校验 | `controller/user.go:238-241` | `common.Validate.Struct(&user)` → 只有 `max=20` / `min=8,max=128` / `max=50` 这类 tag |
| 邮箱分支 | `controller/user.go:242-259` | `EmailVerificationEnabled` 为真才要求 email + code |
| 验证码 | `controller/user.go:247` | `common.VerifyCodeWithKey(user.Email, user.VerificationCode, common.EmailVerificationPurpose)` |
| 邮箱查重（事务外） | `controller/user.go:251-258` | `model.EnsureEmailAvailable(user.Email, 0)` |
| 用户名/邮箱查重 | `controller/user.go:260-273` | `CheckUserExistOrDeleted(username, email)` |
| 邀请码 | `controller/user.go:274-275` | `GetUserIdByAffCode(affCode)` |
| 白名单建对象 | `controller/user.go:276-285` | 只取 username/password/display_name/inviter_id/role/email，**其余字段一律丢弃**（防提权） |
| 落库 | `controller/user.go:286-293` | `cleanUser.Insert(inviterId)` |
| 默认令牌 | `controller/user.go:301-329` | `constant.GenerateDefaultToken` 为真时建一个永不过期、`UnlimitedQuota` 的初始 token |

> **【确认】注册成功后不会自动登录**，只返回 `{"success":true,"message":""}`；前端 `sign-up-form.tsx` 收到成功后 `redirectToLogin()`。

## A2. 验证码存哪？有效期多久？

**【确认】进程内 RAM，`sync.Mutex` + `map[string]verificationValue`，无 Redis、无数据库、无持久化。**

`common/verification.go`：
```go
16: const (
17:   EmailVerificationPurpose = "v"
18:   PasswordResetPurpose     = "r"
19: )
21: var verificationMutex sync.Mutex
22: var verificationMap map[string]verificationValue
23: var verificationMapMaxSize = 10
24: var VerificationValidMinutes = 10          // ← 10 分钟
...
35: func RegisterVerificationCodeWithKey(key, code, purpose string) {
38:   verificationMap[purpose+key] = verificationValue{code, time.Now()}
42:   if len(verificationMap) > verificationMapMaxSize { removeExpiredPairs() }
...
47: func VerifyCodeWithKey(key, code, purpose string) bool {
50:   value, okay := verificationMap[purpose+key]
52:   if !okay || int(now.Sub(value.time).Seconds()) >= VerificationValidMinutes*60 { return false }
55:   return code == value.code
```

要点与坑（**【确认】** 全部来自代码）：
1. **key = `purpose + email`**，即 `"v" + "a@b.com"`；注册与找回密码互不覆盖。
2. **`VerificationValidMinutes = 10`**（分钟），写死在代码里，**不是配置项**，无法通过 `OptionMap` 改。
3. **不删码**：`VerifyCodeWithKey` 只比对、不 `delete`，`Register` 校验通过后也不删 → **同一个码在 10 分钟内可重复使用**（唯一性靠用户名/邮箱查重兜底，但"换用户名、同邮箱"会被 `EnsureEmailAvailable` 拦住）。
4. **多实例部署直接失效**：码在 A 节点内存，请求落到 B 节点就"验证码错误"。new-api 靠 `MEMORY_CACHE_ENABLED` / Redis 做的缓存**不覆盖验证码**。这是它最明显的架构缺陷。
5. `verificationMapMaxSize = 10` 且**清理是"超了才清"**：`removeExpiredPairs` 只删过期项。若 10 个以上**未过期**的码同时存在，map 会持续增长且不做容量上限（只在 `>10` 时触发一次遍历）。**【推测】** 高并发下这是无界增长点，只是条目小、过期即被回收。
6. **对比登录验证码（不是邮箱）**：`service/login_verification.go:12` 的 `LoginVerificationTTL = 5 * time.Minute` 是 **2FA / Passkey 登录挑战**的 TTL，落在 `auth_flows` 表（`model/auth_flow.go:46-58`），与邮箱验证码无关。别混。

## A3. 防刷：`EmailVerificationRateLimit` 到底是什么

**【确认】代码里没有名为 `EmailVerificationRateLimit` 的配置项**（全仓 grep `"EmailVerificationRateLimit"` 无 OptionMap 命中）。它是一个**中间件函数名**，限额是**编译期常量**：

`middleware/email-verification-rate-limit.go:11-15`
```go
const (
    EmailVerificationRateLimitMark = "EV"
    EmailVerificationMaxRequests   = 2   // 30秒内最多2次
    EmailVerificationDuration      = 30  // 30秒时间窗口
)
```

| 维度 | 结论 |
|---|---|
| 次数 | **2 次 / 30 秒** |
| 维度 | **按 `c.ClientIP()`，纯 IP，不按邮箱** |
| 计数键 | Redis：`rateLimit:v2:ip:EV:<ip>`；内存：`EV:<ip>` |
| Redis 实现 | 固定窗口（`redisFixedWindowTake`，Lua：INCR + EXPIRE + TTL），边界可双倍突发（注释明说） |
| 内存实现 | `common.InMemoryRateLimiter.Request` = **滑动窗口**（链表队列），与 Redis 语义不同 |
| 超限返回 | HTTP **429** + `Retry-After`；Redis 分支还带 `Please wait {{seconds}} seconds...` |
| Redis 挂了 | `err != nil` → **降级到内存限流**（fail-open 到本地计数，不是放行） |

**关键缺口【确认】**：
- **完全没有按邮箱的冷却**：同一 IP 换邮箱可无限发；同一邮箱换 IP（代理池）也可无限发。
- 全局 `CriticalRateLimit`（默认 20 次 / 20 分钟 / **IP**，`common/init.go:139-141`）只挂在 `/api/user/register`、`/api/reset_password`、`/api/user/reset` 上；**`/api/verification` 没有挂**，它只有 `EmailVerificationRateLimit`（2/30s）+ 整个 `/api` 组共享的 `GlobalAPIRateLimit`（默认 360 次 / 180 秒 / IP，`common/init.go:127-129`、`router/api-router.go:23`）。即：换 IP 就能继续轰炸。
- 唯一的"按邮箱"保护是 `IsEmailAlreadyTaken` —— 已注册的邮箱直接拒绝发码（`controller/misc.go:226`），这也意味着**该接口是邮箱是否注册的枚举口子**（返回 "Email address is already in use"）。**【确认】**这是信息泄露面。

## A4. 用户表里和邮箱相关的字段

**【确认】只有 2 个真实列 + 1 个非持久化字段；没有 `email_verified`、没有 `verification_code` 列。**

`model/user.go:79-115`：
```go
88:  Email            string `json:"email" gorm:"index" validate:"max=50"`
94:  VerificationCode string `json:"verification_code" gorm:"-:all"`  // 只用于注册校验，不落库
...
89-93,107: GitHubId / DiscordId / OidcId / WeChatId / TelegramId / LinuxDOId  (第三方绑定)
```

- `Email`：`gorm:"index"` → **普通索引，非唯一索引**（`idx_users_email`）。
- `VerificationCode`：`gorm:"-:all"` → GORM 完全忽略（读写都不参与），只在 HTTP 请求体里短暂存在。
- **`email_verified` 在全仓库 0 命中**（grep `email_verified|EmailVerified|emailVerified` 无结果）。语义上"有 email 就是验证过的"，因为只有验证码通过才会写入 `cleanUser.Email`（`controller/user.go:283-285`）。
- 邮箱唯一性由**应用层**保证：`model/user.go:348-357 EnsureEmailAvailable` + `656-673 ensureEmailAvailableWithTx` + `371-395 withNormalizedEmailLock`（PG 用 `pg_advisory_xact_lock(hashtext(email))`，MySQL 用 `SELECT id FROM users WHERE email=? FOR UPDATE` 拿间隙锁，SQLite 靠单写者）。`emailQuery` 用 `Unscoped()`，**软删除用户的邮箱也算占用**。

## A5. 注册必填字段与校验规则

### 必填（**【确认】**）
| 字段 | 条件 | 依据 |
|---|---|---|
| `username` | 永远必填（`TrimSpace` 后非空） | `controller/user.go:234-237` |
| `password` | 永远必填（`HashAccountPassword` 会拒绝空串） | `controller/user.go:286` → `model/user.go:629-634` |
| `email` | `EmailVerificationEnabled=true` 时必填 | `controller/user.go:243-246` |
| `verification_code` | 同上 | 同上 |
| `aff_code` | 可选（邀请人的码） | `controller/user.go:274` |
| `turnstile` | `TurnstileCheckEnabled=true` 时必填（query 参数） | `middleware/turnstile-check.go` |

### 校验规则（**【确认】**）
> **重点：new-api 没有用户名/邮箱/密码的"正则"校验。** 全仓 `RegisterValidation` / `regexp.MustCompile` 与用户名字段无关联（grep `alphanum|username.*regexp` 只命中 OAuth slug 与 tokenkit）。

| 字段 | 规则 | 位置 |
|---|---|---|
| username | `validate:"max=20"`，仅长度上限，**无字符集限制**（中文/空格/emoji 都行），且 `TrimSpace` | `model/user.go:81` |
| password | `validate:"min=8,max=128"`（validator 按 **rune** 计）；实际落库走 `ValidateNewAccountPassword`：**8–128 个 rune，不做任何复杂度要求**（无大小写/数字/符号要求） | `model/user.go:82`、`common/account_password.go:34-39` |
| email | `validate:"max=50"` + `common.Validate.Var(email,"required,email,max=50")`（go-playground `email` tag，非自定义正则） | `model/user.go:88`、`service/email_binding.go:38` |
| display_name | `max=20`；注册时默认 = username | `model/user.go:85`、`controller/user.go:279` |
| remark | `max=255` | `model/user.go:109` |

密码哈希（**【确认】**）：
- 默认 **Argon2id**，`m=19456, t=2, p=1`，16B salt，32B key，编码 `$argon2id$v=19$m=19456,t=2,p=1$<b64salt>$<b64hash>`（`common/account_password.go:18-22, 44-61`）。
- 环境变量 `ACCOUNT_PASSWORD_HASH_ALGORITHM=bcrypt` 可切 bcrypt（兼容老库），`>72` 字节会报 `ErrPasswordLegacyLimit`（`:46-52`）。
- 登录校验 `common.ValidatePasswordAndHash` 同时接受 argon2id 与 bcrypt（`common/crypto.go:30-36`）。

### 登录（用户名或邮箱）
`model/user.go:1058-1083`：
```go
err = DB.Where("username = ? OR email = ?", username, username).First(user).Error
```
→ **登录框可以填邮箱**，但这个是"邮箱当用户名查"，**不是"邮箱验证码登录"**。前端 label 就是 `Username or Email`（`sign-in/components/user-auth-form.tsx:362`）。

## A6. 邮箱域名限制怎么实现

**【确认】** 两处：开关校验 + 实际校验。

**实际校验**（唯一入口）`service/email_binding.go:36-55`：
```go
func ValidateAccountEmail(email string) (string, error) {
    email = model.NormalizeEmail(email)                       // 小写 + trim
    if common.Validate.Var(email, "required,email,max=50") != nil {
        return "", ErrAccountEmailInvalid                      // "Please enter a valid email address"
    }
    parts := strings.Split(email, "@")
    if len(parts) != 2 { return "", ErrAccountEmailInvalid }
    if common.EmailDomainRestrictionEnabled {
        allowed := slices.Contains(common.EmailDomainWhitelist, parts[1])   // 精确匹配，非后缀
        if !allowed { return "", ErrAccountEmailRestricted }
    }
    if common.EmailAliasRestrictionEnabled && strings.ContainsAny(parts[0], "+.") {
        return "", ErrAccountEmailRestricted                   // 禁 + 与 . 别名
    }
    return email, nil
}
```

**调用点只有 4 处 / 3 个文件**（`grep ValidateAccountEmail`）：
1. `controller/misc.go:220` —— 发注册验证码
2. `controller/email_binding.go:36` —— 换绑邮箱（HTTP 层）
3. `service/email_binding.go:58` —— `StartEmailBinding` 内部再校验一次
4. `service/email_binding.go:141` —— `FinishEmailBinding` 提交前再校验一次

**因此【确认】+【重要】**：
- `Register` **不调用** `ValidateAccountEmail`。域名限制是**通过"发码"环节间接生效**的：拿不到码 → 注册不了。
- 但只要 `EmailVerificationEnabled=false`，`Register` 根本不会读 `user.Email`（`controller/user.go:283-285` 只在开关为真时赋值），所以此时邮箱被完全忽略 —— **域名白名单也就无从谈起**。
- 域名匹配是 `slices.Contains` 精确比对，**不支持 `*.example.com` 通配、不支持子域**；`EmailDomainWhitelist` 来自 `model/option.go:508-509` 的 `strings.Split(value, ",")`，**不会 trim、不会小写**（管理端若填 `Gmail.com` 或 `gmail.com, 163.com`（带空格）会失配）。**【推测】** 这是运维常见踩坑点。
- 开关保护：`controller/option.go:248-252`，白名单为空时**不允许开启** `EmailDomainRestrictionEnabled`。

默认白名单（`common/constants.go:78-90`）：`gmail.com, 163.com, 126.com, qq.com, outlook.com, hotmail.com, icloud.com, yahoo.com, foxmail.com`，默认**关闭**。

---

# B. SMTP 发信配置

## B1. 各配置项在哪用

| 配置项 | 默认值 | 定义 | 读入内存 | 使用点 |
|---|---|---|---|---|
| `SMTPServer` | `""` | `common/constants.go:104` | `model/option.go:510-511` | `common/email.go:46`（tls ServerName）、`:50`（`smtp.NewClient` host）、`:100`（未配置报错）、`:112`（`addr`） |
| `SMTPPort` | `587` | `:105` | `model/option.go:512-514`（`Atoi`） | `common/email.go:45`（465 判定）、`:112`（`fmt.Sprintf("%s:%d")`） |
| `SMTPSSLEnabled` | `false` | `:106` | `model/option.go:488-489` | `common/email.go:45` → `tls.Dial` **隐式 TLS（SMTPS）** |
| `SMTPStartTLSEnabled` | `false` | `:107` | `model/option.go:490-491` | `common/email.go:63-73` → `client.StartTLS`，且**服务器不 advertise 就报错**（`:64-68`） |
| `SMTPAccount` | `""` | `:110` | `model/option.go:515-516` | `common/email.go:24`（Outlook 判定）、`:33-35`（有账号+token 才认证）、`:94`（`SMTPFrom` 为空时回退）、`:125`（`client.Mail`） |
| `SMTPFrom` | `""` | `:111` | `model/option.go:517-518` | `common/email.go:14,18`（生成 Message-ID 域名）、`:93-95`（空则 = SMTPAccount）、`:104-110`（`From:` 头）、`:125`（信封 From） |
| `SMTPToken` | `""` | `:112` | `model/option.go:519-520` | `common/email.go:33-35`（是否认证）、`:30`（`AutoSMTPAuth` 的密码） |
| `SMTPInsecureSkipVerify` | `false` | `:108` | `model/option.go:492-493` | `common/email.go:37-42`（`tls.Config.InsecureSkipVerify`） |
| `SMTPForceAuthLogin` | `false` | `:109` | `model/option.go:494-495` | `common/email.go:23`、`common/email_ntlm_auth.go:22` |

**安全细节【确认】**：
- `GetOptions` 会对 key 做敏感后缀过滤（`controller/option.go:84-96`：`Token`/`Secret`/`Key`/`secret`/`api_key`/`private_key`），所以 **`SMTPToken` 永远不会回传给前端**；`SMTPAccount`/`SMTPServer` 会回传（管理端表单需要）。
- 但 `SMTPToken` 在前端"留空表示不修改"（`email-settings-section.tsx:169`），无法清空。

## B2. 用什么库？支持哪些认证方式？

**【确认】Go 标准库 `net/smtp`，没有 gomail、没有第三方 SMTP 库。**（`common/email.go:7` `import "net/smtp"`；`go.mod` 里无 `gomail`/`mail` 相关依赖。）

认证方式是**手写的 `smtp.Auth` 实现**，自动协商：

`common/email_ntlm_auth.go:17-49`
```go
func AutoSMTPAuth(username, password string) smtp.Auth { return &smtpAutoAuth{...} }

func (a *smtpAutoAuth) Start(server *smtp.ServerInfo) (string, []byte, error) {
    useLoginAuth := SMTPForceAuthLogin
    if !useLoginAuth && shouldUseSMTPLoginAuth() {
        useLoginAuth = !(server != nil && len(server.Auth) == 1 && smtpServerSupportsAuth(server, "NTLM"))
    }
    if useLoginAuth { a.mech = "LOGIN"; return "LOGIN", []byte{}, nil }
    switch {
    case smtpServerSupportsAuth(server, "PLAIN"): ... smtp.PlainAuth(...)
    case smtpServerSupportsAuth(server, "LOGIN"): ... "LOGIN"
    case smtpServerSupportsAuth(server, "NTLM"):  ... ntlmssp.NewNegotiateMessage("", "")
    default: ... smtp.PlainAuth(...)   // 兜底
    }
}
```

支持矩阵：

| 机制 | 支持 | 说明 |
|---|---|---|
| `PLAIN` | ✅ | 优先；但 Go 的 `smtp.PlainAuth` 在**非 TLS 且非 localhost** 时会直接拒绝（防明文口令），所以服务器必须 SSL/STARTTLS |
| `LOGIN` | ✅ | 手写 `Next()` 应答 `Username:` / `Password:` 挑战 |
| `NTLM` | ✅ | 依赖 `github.com/Azure/go-ntlmssp v0.1.1`（`go.mod:92`）；**仅在服务器"只" advertise NTLM 时启用** |
| `XOAUTH2` | ❌ | 全仓 grep `XOAUTH2|OAUTHBEARER` **0 命中** |
| `CRAM-MD5` | ❌ | 无 |

> **对邮箱池场景的关键结论【确认】**：new-api **不支持 SMTP XOAUTH2**，因此**无法用 Outlook / Gmail 的 OAuth2 令牌发信**，只能"账号 + 应用密码/基本认证"。而微软已于 2022 起在 Exchange Online 停用 SMTP AUTH 基本认证（`smtp.office365.com` 需租户显式开启且仍受限）——**【推测】** 用它当"Outlook 池发信端"会大面积 535。

## B3. 邮件模板长什么样？多语言怎么处理？

### 模板（**【确认】极简，硬编码在 controller 里，无 HTML 模板文件**）
`controller/misc.go:236-241`：
```go
content := fmt.Sprintf("<p>%s</p><p>%s</p><p>%s</p>",
    i18n.T(c, i18n.MsgEmailVerificationGreeting, systemName),  // "您好，你正在进行{{.SystemName}}邮箱验证。"
    i18n.T(c, i18n.MsgEmailVerificationCode, map[string]any{"Code": code}), // "您的验证码为: <strong>123456</strong>"
    i18n.T(c, i18n.MsgEmailVerificationValidity, map[string]any{"Minutes": 10}))
```

生成的 MIME（`common/email.go:103-110`）：
```
To: <receiver>
From: <SystemName> <SMTPFrom>
Subject: =?UTF-8?B?<base64(subject)>?=
Date: Mon, 02 Jan 2006 15:04:05 -0700
Message-ID: <1700000000000000000.AbC123def456@yourdomain.com>
Content-Type: text/html; charset=UTF-8

<p>...</p><p>...</p><p>...</p>
```

**注意【确认】**：
- **没有 `MIME-Version`、没有 `multipart/alternative`（无纯文本兜底）、没有完整 `<html><body>`、没有内联 CSS**。垃圾邮件评分会偏高。
- `To:` 直接用原始 `receiver` 字符串，多收件人用 `;` 分隔（`common/email.go:113` `strings.Split(receiver, ";")`）—— **不支持 `,` 分隔**，也不做地址校验（Resend 分支才校验）。
- `From:` 的显示名是 `common.SystemName`，而 `Message-ID` 域名取 `SMTPFrom` 的 `@` 后半段（`common/email.go:13-20`），若 `SMTPFrom` 不是合法邮箱会直接报 "invalid SMTP account"。
- 正文里的 `code` **没有 HTML 转义**（`fmt.Sprintf` 直插）—— 因为码是 hex，暂时无害；但换绑流程用了 `html.EscapeString`（`service/email_binding.go:186`），说明作者知道这是问题。

### i18n（**【确认】** go-i18n v2 + 嵌入式 YAML，只有 3 种语言）
- 库：`github.com/nicksnyder/go-i18n/v2 v2.6.1`（`go.mod:38`）
- 资源：`i18n/locales/{zh-CN,zh-TW,en}.yaml`，`//go:embed locales/*.yaml`（`i18n/i18n.go:28-29`）
- Key 常量：`i18n/keys.go:62-65`（`email.verification.subject/greeting/code/validity`）
- 模板（`i18n/locales/zh-CN.yaml:50-53`）：
  ```yaml
  email.verification.subject: "{{.SystemName}}邮箱验证邮件"
  email.verification.greeting: "您好，你正在进行{{.SystemName}}邮箱验证。"
  email.verification.code: "您的验证码为: <strong>{{.Code}}</strong>"
  email.verification.validity: "验证码 {{.Minutes}} 分钟内有效，如果不是本人操作，请忽略。"
  ```
  `en.yaml:49-52`：
  ```yaml
  email.verification.subject: "{{.SystemName}} email verification"
  email.verification.greeting: "Hello, you are verifying your email address for {{.SystemName}}."
  email.verification.code: "Your verification code is: <strong>{{.Code}}</strong>"
  email.verification.validity: "The verification code is valid for {{.Minutes}} minutes. If you did not request it, please ignore this email."
  ```

- **语言选择策略（两套，别混）**：
  1. **注册验证码**用"请求语言"：`i18n.T(c, ...)` → `StatedLang(c)`（`i18n/i18n.go:76-140`），优先级 = 用户设置语言 → 用户ID懒加载 → `Accept-Language` 头 → `DefaultLanguage`（`common/constants.go:42`，来自 `DEFAULT_LANGUAGE` 环境变量，默认 `en`）。`controller/misc.go:232-233` 注释明说"该地址可能还不属于任何账号，所以跟随请求语言"。
  2. **找回密码 / 安全通知**用"用户存储的语言"：`user.GetSetting().Language`（`controller/misc.go:264-267`），空则回退 `StatedLang(c)`。
- `normalizeLang`（`i18n/i18n.go:186-201`）把 `zhCN`/`zh_CN`/`zh-HK`/`zh-Hant` 都映射到 `zh-TW`/`zh-CN`，其他一律 `en`。
- **前端 i18n 是另一套**（`web/src/i18n/*.json`，7 种语言），**与邮件模板无关**。所以 new-api 的邮件只有 3 种语言，前端有 7 种 —— 这是真实缺口。

## B4. `EmailLoginAuthServerList` 是什么？

**【确认】它是一份"必须/优先用 AUTH LOGIN 机制"的 SMTP 服务器白名单**，不是"邮箱登录授权服务器列表"（名字极具误导性）。

`common/constants.go:91-94`
```go
var EmailLoginAuthServerList = []string{
    "smtp.sendcloud.net",
    "smtp.azurecomm.net",
}
```

唯一使用点 `common/email.go:22-27`：
```go
func shouldUseSMTPLoginAuth() bool {
    if SMTPForceAuthLogin { return true }
    return isOutlookServer(SMTPAccount) || slices.Contains(EmailLoginAuthServerList, SMTPServer)
}
```
配合 `common/email_ntlm_auth.go:22-29`：
```go
useLoginAuth := SMTPForceAuthLogin
if !useLoginAuth && shouldUseSMTPLoginAuth() {
    useLoginAuth = !(server != nil && len(server.Auth)==1 && smtpServerSupportsAuth(server,"NTLM"))
}
if useLoginAuth { return "LOGIN", []byte{}, nil }
```

**用途**：这两个服务商（SendCloud 搜狐、Azure Communication Services）的 SMTP 在 EHLO 里**不一定 advertise `AUTH PLAIN`**，或者只 advertise `LOGIN`；Go 的 `smtp.PlainAuth` 又拒绝非加密通道。把它们放进列表 = **强制走 `AUTH LOGIN`**（手写挑战应答，绕开 `PlainAuth` 的 TLS 前置检查）。
`isOutlookServer`（`common/email-outlook-auth.go:35-40`）同理，判断 `SMTPAccount` 里是否含 `outlook` / `onmicrosoft`，注释直言"其实应该加一个 Option 来区分…先临时兼容一下"。

**结论**：这是一个**兼容性 hack 清单**，硬编码、不可配置（`EmailLoginAuthServerList` 不在 `OptionMap` 里，管理员改不了），新增服务商只能改代码重编译。

---

# C. 用户系统与数据库

## C1. 【最重要】到底用 MySQL / PostgreSQL / SQLite？

### 结论
**三者都支持，默认 SQLite；生产 docker-compose 默认 PostgreSQL。** 项目**不是**"只能 MySQL"。

### 确凿证据（代码行）

**① 三选一的分派逻辑 —— `model/main.go:140-184`**
```go
140: func chooseDB(envName string, isLog bool) (*gorm.DB, common.DatabaseType, error) {
141:     dsn := os.Getenv(envName)              // envName = "SQL_DSN"
142:     if dsn != "" {
...
151:         if strings.HasPrefix(dsn, "postgres://") || strings.HasPrefix(dsn, "postgresql://") {
152:             // Use PostgreSQL
153:             common.SysLog(common.LogText("using PostgreSQL as database"))
156:             db, err := gorm.Open(postgresMigrationDialector{postgres.Dialector{Config: &postgres.Config{
157:                 DSN:                  dsn,
158:                 PreferSimpleProtocol: true,
159:             }}}, newGormConfig(false))
160:             return db, common.DatabaseTypePostgreSQL, err
161:         }
162:         if strings.HasPrefix(dsn, "local") {
163:             common.SysLog("SQL_DSN not set, using SQLite as database")
164:             db, err := gorm.Open(sqlite.Open(common.SQLitePath), newGormConfig(true))
165:             return db, common.DatabaseTypeSQLite, err
166:         }
167:         // Use MySQL
168:         common.SysLog(common.LogText("using MySQL as database"))
...
177:         db, err := gorm.Open(mysqlMigrationDialector{mysql.Dialector{Config: &mysql.Config{DSN: dsn}}}, newGormConfig(true))
178:         return db, common.DatabaseTypeMySQL, err
179:     }
180:     // Use SQLite
181:     common.SysLog("SQL_DSN not set, using SQLite as database")
182:     db, err := gorm.Open(sqlite.Open(common.SQLitePath), newGormConfig(true))
183:     return db, common.DatabaseTypeSQLite, err
184: }
```

**② 类型常量 —— `common/database.go:5-12`**
```go
const (
    DatabaseTypeMySQL      DatabaseType = "mysql"
    DatabaseTypeSQLite     DatabaseType = "sqlite"
    DatabaseTypePostgreSQL DatabaseType = "postgres"
    DatabaseTypeClickHouse DatabaseType = "clickhouse"
)
```

**③ SQLite 默认文件 —— `common/database.go:66`**
```go
var SQLitePath = "one-api.db?" + sqliteConcurrencyParams
// sqliteConcurrencyParams = "_pragma=busy_timeout(30000)&_pragma=journal_mode(WAL)&_txlock=immediate"
```
（文件名沿用 one-api 的历史命名 `one-api.db`；可用 `SQLITE_PATH` 覆盖，`common/init.go:71-73`）

**④ 驱动依赖 —— `go.mod`**
```
22: github.com/glebarez/sqlite v1.11.0        // 纯 Go SQLite（modernc.org/sqlite），无 CGO
62: gorm.io/driver/mysql v1.5.7
63: gorm.io/driver/postgres v1.5.9
69: gorm.io/driver/clickhouse v0.6.0          // 仅日志库
```

**⑤ 官方部署默认 PostgreSQL —— `docker-compose.yml:29`**
```yaml
- SQL_DSN=postgresql://root:123456@postgres:5432/new-api
```
（注释 `:7-11` 给出切 MySQL 的三步；`:30` 是 MySQL 的 DSN 样例）

**⑥ 表结构由 GORM `AutoMigrate` 生成，无 SQL 迁移文件 —— `model/main.go:342-376`**
```go
err := DB.AutoMigrate(
    &Channel{}, &Token{}, &User{}, &UserSession{}, &AuthFlow{}, ...
)
```
（`&User{}` 在 `:345`；`&Log{}` 在 `:354`）

### 选择逻辑速查
| `SQL_DSN` | 结果 |
|---|---|
| 空 | **SQLite**（`one-api.db`） |
| 以 `postgres://` / `postgresql://` 开头 | PostgreSQL |
| 以 `local` 开头 | SQLite（显式指定） |
| 其他（如 `user:pass@tcp(host:3306)/db`） | **MySQL** |
| `clickhouse://` / `tcp://` / `http(s)://` | ClickHouse（**仅** `LOG_SQL_DSN`，主库会报错 `model/main.go:143-149`） |

### 跨库差异（**【确认】** 代码里有分支）
- 邮箱并发锁：PG `pg_advisory_xact_lock(hashtext(?))`、MySQL `SELECT ... FOR UPDATE`、SQLite 无锁（`model/user.go:383-394`）。
- 配额列必须 64 位：MySQL/PG 启动时 `ensureUserQuotaColumns` 校验 `quota/used_quota/aff_quota/aff_history` 必须是 `bigint`，否则**启动失败**（`model/main.go:204, 279-323`）；SQLite 跳过。
- MySQL 启动强制检查中文 charset/collation（`model/main.go:199-202`）。
- 自定义 dialector 修 MySQL `decimal` 默认值 padding、PG `bpchar` 类型名（`model/migration_dialector.go:16-68`）。

## C2. 用户表完整字段定义

### Go 结构体（唯一权威定义）`model/user.go:79-115`

| # | 字段 | Go 类型 | GORM tag | 语义 |
|---|---|---|---|---|
| 1 | `Id` | `int` | （主键，GORM 约定） | 自增主键 |
| 2 | `Username` | `string` | `unique;index` `validate:"max=20"` | 登录名，**唯一** |
| 3 | `Password` | `string` | `not null;` `validate:"min=8,max=128"` | Argon2id/bcrypt 哈希；**OAuth 用户为空串** |
| 4 | `HasPassword` | `bool` | `-:all` | 运行时计算（`CASE WHEN password<>''`），不落库 |
| 5 | `OriginalPassword` | `string` | `-:all` | 改密时校验旧密码，不落库 |
| 6 | `DisplayName` | `string` | `index` `validate:"max=20"` | 显示名 |
| 7 | `Role` | `int` | `type:int;default:1` | 0/1/10/100 |
| 8 | `Status` | `int` | `type:int;default:1` | 1 启用 / 2 禁用 |
| 9 | `Email` | `string` | `index` `validate:"max=50"` | **非唯一索引**，应用层保证唯一 |
| 10 | `GitHubId` | `string` | `column:github_id;index` | |
| 11 | `DiscordId` | `string` | `column:discord_id;index` | |
| 12 | `OidcId` | `string` | `column:oidc_id;index` | |
| 13 | `WeChatId` | `string` | `column:wechat_id;index` | |
| 14 | `TelegramId` | `string` | `column:telegram_id;index` | |
| 15 | `VerificationCode` | `string` | `-:all` | **不落库** |
| 16 | `AccessToken` | `*string` | `type:char(32);uniqueIndex` | **Deprecated**（旧面板 token，过渡期） |
| 17 | `AccessTokenCreatedAt` | `*int64` | `type:bigint` | 同上 |
| 18 | `Quota` | `int` | `type:int;default:0` | 余额（内部单位；`QuotaPerUnit=500000`） |
| 19 | `UsedQuota` | `int` | `type:int;default:0` | 累计消耗 |
| 20 | `RequestCount` | `int` | `type:int;default:0` | 请求数 |
| 21 | `Group` | `string` | `varchar(64);default:'default'` | 分组（倍率/渠道） |
| 22 | `AffCode` | `string` | `varchar(32);uniqueIndex` | 自己的邀请码（注册时 `GetRandomString(4)`） |
| 23 | `AffCount` | `int` | `type:int;default:0` | 邀请人数 |
| 24 | `AffQuota` | `int` | `type:int;default:0` | 可转出邀请额度 |
| 25 | `AffHistoryQuota` | `int` | `type:int;column:aff_history` | 历史邀请额度（**列名是 `aff_history`**，与字段名不一致） |
| 26 | `InviterId` | `int` | `column:inviter_id;index` | 邀请人 ID |
| 27 | `DeletedAt` | `gorm.DeletedAt` | `index` | 软删除 |
| 28 | `LinuxDOId` | `string` | `column:linux_do_id;index` | |
| 29 | `Setting` | `string` | `type:text` | JSON：`language`、`sidebar_modules`、`notify_type` 等 |
| 30 | `Remark` | `string` | `varchar(255)` | 管理员备注 |
| 31 | `StripeCustomer` | `string` | `varchar(64);index` | |
| 32 | `CreatedAt` | `int64` | `autoCreateTime` | Unix 秒 |
| 33 | `LastLoginAt` | `int64` | `default:0` | Unix 秒 |
| 34 | `AuthVersion` | `int64` | `bigint;not null;default:1` | 会话/凭证版本号，改密/封禁即 +1 使旧 token 失效 |
| 35 | `AdminPermissions` | `map[string]map[string]bool` | `-:all` | 细粒度管理员权限，**不落库**（存 Casbin `casbin_rule`） |

### 实际 DDL（用 GORM v1.25.12 + 同样的 tag 复刻生成）

**SQLite**（默认）：
```sql
CREATE TABLE `users` (
  `id` integer PRIMARY KEY AUTOINCREMENT,
  `username` text,
  `password` text NOT NULL,
  `display_name` text,
  `role` integer DEFAULT 1,
  `status` integer DEFAULT 1,
  `email` text,
  `github_id` text,
  `discord_id` text,
  `oidc_id` text,
  `wechat_id` text,
  `telegram_id` text,
  `access_token` char(32),
  `access_token_created_at` bigint,
  `quota` integer DEFAULT 0,
  `used_quota` integer DEFAULT 0,
  `request_count` integer DEFAULT 0,
  `group` varchar(64) DEFAULT "default",
  `aff_code` varchar(32),
  `aff_count` integer DEFAULT 0,
  `aff_quota` integer DEFAULT 0,
  `aff_history` integer DEFAULT 0,
  `inviter_id` integer,
  `deleted_at` datetime,
  `linux_do_id` text,
  `setting` text,
  `remark` varchar(255),
  `stripe_customer` varchar(64),
  `created_at` integer,
  `last_login_at` integer DEFAULT 0,
  `auth_version` bigint NOT NULL DEFAULT 1,
  CONSTRAINT `uni_users_username` UNIQUE (`username`)
);
```

**索引清单（SQLite 实测）**：
```sql
CREATE UNIQUE INDEX `idx_users_aff_code`        ON `users`(`aff_code`);
CREATE UNIQUE INDEX `idx_users_access_token`    ON `users`(`access_token`);
CREATE INDEX `idx_users_username`               ON `users`(`username`);      -- 冗余（已有 UNIQUE 约束）
CREATE INDEX `idx_users_email`                  ON `users`(`email`);         -- 非唯一！
CREATE INDEX `idx_users_display_name`           ON `users`(`display_name`);
CREATE INDEX `idx_users_inviter_id`             ON `users`(`inviter_id`);
CREATE INDEX `idx_users_deleted_at`             ON `users`(`deleted_at`);
CREATE INDEX `idx_users_stripe_customer`        ON `users`(`stripe_customer`);
CREATE INDEX `idx_users_git_hub_id`             ON `users`(`github_id`);
CREATE INDEX `idx_users_discord_id`             ON `users`(`discord_id`);
CREATE INDEX `idx_users_oidc_id`                ON `users`(`oidc_id`);
CREATE INDEX `idx_users_we_chat_id`             ON `users`(`wechat_id`);
CREATE INDEX `idx_users_telegram_id`            ON `users`(`telegram_id`);
CREATE INDEX `idx_users_linux_do_id`            ON `users`(`linux_do_id`);
```

**MySQL 列类型（GORM mysql driver v1.5.7 实测映射）**：
`id bigint AUTO_INCREMENT` / `username varchar(191)` / `password longtext` / `display_name varchar(191)` / `role bigint` / `status bigint` / `email varchar(191)` / 各 `*_id varchar(191)` / `access_token char(32)` / `quota|used_quota|request_count|aff_*|inviter_id bigint` / `group varchar(64)` / `aff_code varchar(32)` / `remark varchar(255)` / `stripe_customer varchar(64)` / `created_at|last_login_at|auth_version bigint` / `setting text`。
（注意：`email` 在 MySQL 上被撑到 **varchar(191)** 而不是 50 —— GORM 对"有索引的 string"在没有显式 size 时给 191 以兼容 utf8mb4 索引长度；`validate:"max=50"` 只是应用层校验。）

**PostgreSQL**：`id bigserial` / 无 size 的 string → `text` / `quota` 等 `bigint` / `created_at` 等 `bigint` / `deleted_at timestamptz`。

### 其他相关表（**【确认】** 由 `AutoMigrate` 一起建）
- `tokens`（`model/token.go`）：API Key
- `user_sessions`（`model/user_session.go`）：**这是新的会话表**，存 access/refresh 会话、`sid`、`login_method`、IP、UA、撤销时间；`AuthVersion` 用于批量失效
- `auth_flows`（`model/auth_flow.go:46-58`）：一次性流程（OAuth state、2FA 登录挑战、Passkey、邮箱换绑），`token_hash char(64) uniqueIndex`、`purpose varchar(32)`、`expires_at`、`consumed_at`
- `two_fa` / `two_fa_backup_codes` / `passkey_credentials`
- `casbin_rule`（`service/authz`）：细粒度权限
- `user_oauth_bindings` / `external_identity_claims`：自定义 OAuth 绑定与身份去重
- 业务表：`channels`、`logs`、`redemptions`、`topups`、`quota_data`、`tasks`、`subscriptions`、`checkins` 等

## C3. 注册 / 登录 / 改密 / 找回密码 接口清单

> 全部来自 `router/api-router.go`（行号为该文件真实行号）。

### 注册 / 验证码
| 方法 | 路径 | 中间件 | Handler |
|---|---|---|---|
| `POST` | `/api/user/register` | `CriticalRateLimit` + `anonymousRequestBodyLimit` + `TurnstileCheck` | `controller.Register`（:79） |
| `GET` | `/api/verification?email=` | `EmailVerificationRateLimit` + `TurnstileCheck` | `controller.SendEmailVerification`（:46） |
| `GET` | `/api/status` | 无 | 返回 `email_verification` / `register_enabled` / `password_register_enabled` 等开关（`controller/misc.go:41-177`） |

### 登录
| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/api/user/login` | 用户名**或邮箱** + 密码；可返回 2FA/Passkey challenge（:81） |
| `GET` | `/api/user/login/encryption-key` | 密码传输加密公钥（:80） |
| `POST` | `/api/user/login/2fa` | 2FA 第二步（:82） |
| `POST` | `/api/user/login/verify` | 通用登录验证（2FA code，:83） |
| `POST` | `/api/user/login/passkey/begin` \| `/finish` | Passkey 登录（:84-85） |
| `POST` | `/api/user/passkey/login/begin` \| `/finish` | 旧版 Passkey 别名（:86-87） |
| `POST` | `/api/user/auth/refresh` | 刷新 access token（:77） |
| `POST` | `/api/user/auth/logout` | 登出（:78） |
| `GET` | `/api/oauth/:provider` | GitHub / Discord / OIDC / LinuxDO / Telegram 统一入口（:61） |
| `GET` | `/api/oauth/wechat` | 微信（:55） |
| `GET` | `/api/oauth/telegram/login` | Telegram（:57） |

### 改密 / 找回
| 方法 | 路径 | 说明 |
|---|---|---|
| `PUT` | `/api/user/self` | 改资料 / **改密码**（body 带 `password` + `original_password`，需 step-up proof）（:102） |
| `GET` | `/api/reset_password?email=` | 发重置邮件（`CriticalRateLimit` + `TurnstileCheck`）（:47） |
| `POST` | `/api/user/reset` | 用邮件里的 token 重置，**返回新生成的随机密码明文**（:48） |
| `DELETE` | `/api/user/self` | 注销账号（需证明）（:103） |

### 邮箱绑定（换邮箱）
| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/api/oauth/email/bind/start` | 发起换绑，向新邮箱（有旧邮箱时**也向旧邮箱**）发 6 位码（:51） |
| `POST` | `/api/oauth/email/bind/resend` | 重发，1 分钟冷却（:52） |
| `POST` | `/api/oauth/email/bind` | 提交 `new_code` + `old_code` 完成（:53） |

### 管理端（AdminAuth）
| 方法 | 路径 |
|---|---|
| `GET` | `/api/user/`（列表）、`/api/user/search`、`/api/user/:id` |
| `POST` | `/api/user/`（建用户）、`/api/user/manage`（禁用/启用/升降级） |
| `PUT` | `/api/user/`（改用户，可重置密码） |
| `DELETE` | `/api/user/:id`（硬删）、`/api/user/:id/bindings/:binding_type`（清绑定）、`/api/user/:id/2fa`、`/api/user/:id/reset_passkey` |

**"邮箱验证码登录"接口：不存在。** 全仓 grep `login/email`、`LoginByEmail`、`email_code`、`passwordless` 均无命中。`/api/user/login/verify` 只接受 `method="2fa"`（`controller/login_verification.go:26-35`）。

## C4. 角色 / 权限模型

### 系统角色（粗粒度，`users.role`）
`common/constants.go:201-210`：
```go
const (
    RoleGuestUser  = 0     // 访客（未登录 / 受限）
    RoleCommonUser = 1     // 普通用户
    RoleAdminUser  = 10    // 管理员
    RoleRootUser   = 100   // 超级管理员（唯一）
)
func IsValidateRole(role int) bool { ... }
```
- 中间件按阈值放行：`UserAuth()` → `>=1`、`AdminAuth()` → `>=10`、`RootAuth()` → `>=100`（`middleware/auth.go:132-149`）。
- 层级保护：`canManageTargetRole(myRole, targetRole)` = `myRole == Root || myRole > targetRole`（`controller/user.go:383-385`）→ **同级不可互管**；只有 Root 能 `promote`（`controller/user.go:1160-1169`）。
- 注册时**强制** `Role: common.RoleCommonUser`（`controller/user.go:281`），且 `cleanUser` 白名单赋值，**客户端传 `role:100` 无效**（`controller/user.go:276-285`）。
- 初始 Root 由 `/api/setup` 创建，`Quota: 100000000`（`controller/setup.go:90-99`）。

### 细粒度权限（`service/authz`，Casbin）
- 模型：`sub, obj, act`，`some(where (p.eft == allow))`（`service/authz/enforcer.go:21-31`）。
- 内置角色：`root`（`Superuser: true`，拥有全部权限）、`admin`（`service/authz/role.go:20-40`）。
- 主体：`role:admin` / `user:<id>`（`service/authz/permission.go`）。
- 每用户覆盖：`admin_permissions` map，通过 `SetUserPermissionsInTx` 写入 `casbin_rule`；Root 可为单个 admin 打开/关闭某项（如 `channel.sensitive_write`）。
- 前端拿到的是 `permissions.admin_permissions = authz.Capabilities(user.Id, user.Role)`（`controller/user.go:497`）。
- 权限注册表分资源：`channel`（read/operate/write/sensitive_write/secret_view）、`audit`、`task_plugin` 等（`service/authz/resources_*.go`）。

**结论**：**"role 数字"决定"是不是管理员"，"Casbin policy"决定"管理员能做什么"**。二者叠加，不是单一 RBAC。

## C5. 扩展功能

### 邀请码（**【确认】** 完整存在）
- 字段：`aff_code`（自己的码，注册时随机 4 位 `GetRandomString(4)`，`model/user.go:682`）、`inviter_id`（`controller/user.go:274-275`）、`aff_count`、`aff_quota`、`aff_history`。
- 奖励：`QuotaForNewUser` / `QuotaForInviter` / `QuotaForInvitee`（`common/constants.go:130-132`），在 `finishInsert` 里发放（`model/user.go:717-730`）。
- **合规闸门**：`if inviterId != 0 && operation_setting.IsPaymentComplianceConfirmed()`（`model/user.go:720`）→ 未确认支付合规，**邀请奖励不发**，但邀请关系仍记录。
- 接口：`GET /api/user/aff`（取/生成自己的码，`controller/user.go:445-465`）、`POST /api/user/aff_transfer`（把 `aff_quota` 转成 `quota`，最低 `QuotaPerUnit`，`model/user.go:587-622`）。

### 注册限制（**【确认】** 多层开关）
| 开关 | 作用 | 位置 |
|---|---|---|
| `RegisterEnabled` | 总开关，false 则 `POST /register` 直接拒 | `controller/user.go:218-221` |
| `PasswordRegisterEnabled` | 关闭"密码注册"，只允许第三方 | `controller/user.go:222-225` |
| `EmailVerificationEnabled` | 强制邮箱 + 验证码 | `controller/user.go:242-259` |
| `EmailDomainRestrictionEnabled` + `EmailDomainWhitelist` | 域名白名单 | `service/email_binding.go:45-50` |
| `EmailAliasRestrictionEnabled` | 禁 `+` / `.` 别名 | `service/email_binding.go:51-53` |
| `TurnstileCheckEnabled` + `TurnstileSiteKey/SecretKey` | Cloudflare 人机 | `middleware/turnstile-check.go` |
| `CriticalRateLimit*` | 20 次 / 20 分钟 / IP | `common/init.go:139-141` |
| `EmailVerificationRateLimit`（常量） | 2 次 / 30 秒 / IP | `middleware/email-verification-rate-limit.go:11-15` |
| `DemoSiteEnabled` / `SelfUseModeEnabled` | 演示站/自用模式（前端隐藏注册入口） | `controller/misc.go:88-89` |

**没有**：注册邮箱后缀黑名单、一次性邮箱（disposable domain）检测、注册 IP 归属地限制、邀请码"必须持码才能注册"模式（`aff_code` 只是可选加分）。

### OAuth 绑定（**【确认】** 非常完整）
| 提供方 | 存储列 | 备注 |
|---|---|---|
| GitHub | `github_id` | 有 legacy 数字 ID → 新版 ID 的迁移逻辑（`service/login_verification.go:24-30`、`MigrateLegacyGitHubBindingWithTx`） |
| Discord | `discord_id` | |
| OIDC | `oidc_id` | `system_setting.GetOIDCSettings()` |
| LinuxDO | `linux_do_id` | 有 `LinuxDOMinimumTrustLevel` |
| 微信 | `wechat_id` | 公众号验证码登录（不是 OAuth） |
| Telegram | `telegram_id` | Login Widget + 绑定流 |
| 自定义 OAuth | `user_oauth_bindings` 表 | 管理员可加任意 provider（`/api/custom-oauth-provider`，Root only） |
| Passkey | `passkey_credentials` | WebAuthn |
| 2FA | `two_fa` / `two_fa_backup_codes` | TOTP |

**换绑保护（设计得比注册严谨得多，值得抄）**：
- 6 位数字码用 `crypto/rand` 生成（`service/email_binding.go:158-163`），**Argon2/bcrypt 哈希存储**（`:167` `Password2Hash`），**不是明文**。
- 存 `auth_flows.payload`（DB），`EmailBindingTTL = 10min`、`ResendDelay = 1min`、`MaxAttempts = 5`（`model/email_binding.go:11-15`）。
- 有旧邮箱时**双码**：新邮箱收 `new_code`，旧邮箱收 `old_code`，两者都要对（`service/email_binding.go:74, 191-197`）。
- 成功后向新旧邮箱各发一封安全通知（`controller/email_binding.go`）。
- **对比注册流程**：注册的码是明文存 RAM、6 位 hex、无尝试次数限制 —— 两套实现质量差距明显。

### 会话体系（**【确认】** 比 one-api 强很多）
- `user_sessions` 表 + access token（短期）+ refresh cookie（`HttpOnly`，`service.WriteRefreshCookie`）。
- `AuthVersion` 单调递增：改密 / 封禁 / 删除 → `RevokeAllUserSessions`。
- `USER_SESSION_ACTIVE_LIMIT=50`、`ISSUANCE_LIMIT=100/24h`、`REVOKED_RETENTION_DAYS=7`、`HOURLY_ALERT_THRESHOLD=5000`（`common/constants.go:45-59`）。
- 登录审计：`recordLoginAudit` 记录 `login_method`（password/2fa/passkey/oauth:xxx/wechat/telegram）（`controller/user.go:102-144`）。

---

# D. Rust 实现清单

> 目标：MailPool 的 Rust 版要复刻"邮箱注册 + 验证码"能力，但**必须修掉 new-api 的 5 个坑**：①码在内存 → 多实例失效；②码明文、不删、可复用；③只按 IP 限流；④6 位 hex 与前端数字键盘不匹配；⑤无 `email_verified` 状态。

## D1. 用户表 DDL 建议（SQLite 为主，兼容 MySQL/PG 的写法见备注）

```sql
-- ────────────────────────────────────────────────────────────────
-- users：对齐 new-api 语义，但补上 email_verified_at 与 updated_at
-- ────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS users (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    username           TEXT    NOT NULL,                 -- 展示/登录名
    username_norm      TEXT    NOT NULL,                 -- lowercase，用于唯一
    email              TEXT    NOT NULL DEFAULT '',      -- 已归一化（trim+lower）
    email_verified_at  INTEGER NOT NULL DEFAULT 0,       -- 0 = 未验证；Unix 秒
    password_hash      TEXT    NOT NULL DEFAULT '',      -- argon2id；OAuth-only 用户为空
    display_name       TEXT    NOT NULL DEFAULT '',
    role               INTEGER NOT NULL DEFAULT 1,        -- 0 guest / 1 user / 10 admin / 100 root
    status             INTEGER NOT NULL DEFAULT 1,        -- 1 enabled / 2 disabled
    quota              INTEGER NOT NULL DEFAULT 0,
    used_quota         INTEGER NOT NULL DEFAULT 0,
    request_count      INTEGER NOT NULL DEFAULT 0,
    "group"            TEXT    NOT NULL DEFAULT 'default',
    aff_code           TEXT    NOT NULL DEFAULT '',      -- 自己的邀请码
    aff_count          INTEGER NOT NULL DEFAULT 0,
    aff_quota          INTEGER NOT NULL DEFAULT 0,
    aff_history_quota  INTEGER NOT NULL DEFAULT 0,       -- 对应 new-api 的 aff_history
    inviter_id         INTEGER NOT NULL DEFAULT 0,
    setting_json       TEXT    NOT NULL DEFAULT '{}',    -- language / notify 等
    remark             TEXT    NOT NULL DEFAULT '',
    auth_version       INTEGER NOT NULL DEFAULT 1,       -- 改密/封禁 +1，作废旧会话
    last_login_at      INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    deleted_at         INTEGER,                          -- NULL = 未删除
    CHECK (role IN (0,1,10,100)),
    CHECK (status IN (1,2))
);

-- 用户名唯一（大小写不敏感：靠 username_norm 保证，跨库一致）
CREATE UNIQUE INDEX IF NOT EXISTS ux_users_username_norm
    ON users(username_norm) WHERE deleted_at IS NULL;

-- 邮箱唯一：仅"未删除 且 非空"参与唯一（SQLite/PG 支持 partial index）
CREATE UNIQUE INDEX IF NOT EXISTS ux_users_email
    ON users(email) WHERE email <> '' AND deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS ix_users_email          ON users(email);
CREATE INDEX IF NOT EXISTS ix_users_inviter_id     ON users(inviter_id);
CREATE INDEX IF NOT EXISTS ix_users_created_at     ON users(created_at);
CREATE UNIQUE INDEX IF NOT EXISTS ux_users_aff_code
    ON users(aff_code) WHERE aff_code <> '';
```

**MySQL 怎么办**：MySQL 8 不支持 partial index。三个可选方案（推荐第 1 个）：
1. 用**生成列**做唯一键：
   ```sql
   ALTER TABLE users
     ADD COLUMN email_uniq VARCHAR(191)
       GENERATED ALWAYS AS (IF(deleted_at IS NULL AND email <> '', email, NULL)) VIRTUAL,
     ADD UNIQUE KEY ux_users_email_uniq (email_uniq);
   ```
2. 应用层 + `SELECT ... FOR UPDATE`（new-api 的做法，`model/user.go:378-395`），但**并发下仍可能双写**，必须有唯一索引兜底。
3. 软删除时把 email 改写为 `email || '#deleted#' || id`。

**PostgreSQL**：`GENERATED ALWAYS AS (...) STORED` 或直接用 partial unique index（同 SQLite）。

## D2. 验证码表设计（**核心改进：落库 + 哈希 + 单活跃 + 尝试次数**）

```sql
-- ────────────────────────────────────────────────────────────────
-- verification_codes：一次性验证码
-- 与 new-api 的差异：落库（多实例可用）、只存 HMAC 哈希、单活跃、计次、可审计
-- ────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS verification_codes (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    purpose       TEXT    NOT NULL,              -- 'register' | 'reset' | 'bind_new' | 'bind_old'
    email         TEXT    NOT NULL,              -- 已归一化
    code_hash     TEXT    NOT NULL,              -- HMAC-SHA256(server_secret, purpose||email||code) hex
    attempts      INTEGER NOT NULL DEFAULT 0,
    max_attempts  INTEGER NOT NULL DEFAULT 5,
    sent_count    INTEGER NOT NULL DEFAULT 1,    -- 同一 flow 重发次数
    request_ip    TEXT    NOT NULL DEFAULT '',
    user_id       INTEGER NOT NULL DEFAULT 0,    -- 换绑时为发起人；注册为 0
    flow_token    TEXT    NOT NULL DEFAULT '',   -- 换绑场景与 auth_flows 关联
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    consumed_at   INTEGER,                       -- NULL = 未使用
    CHECK (purpose IN ('register','reset','bind_new','bind_old'))
);

-- 同一 (email, purpose) 只允许存在 1 条未消费记录：
-- 发新码时先把旧记录 consumed_at = now（"新码作废旧码"，修掉 new-api 可复用的问题）
CREATE UNIQUE INDEX IF NOT EXISTS ux_vc_active
    ON verification_codes(email, purpose)
    WHERE consumed_at IS NULL;

CREATE INDEX IF NOT EXISTS ix_vc_expires
    ON verification_codes(expires_at) WHERE consumed_at IS NULL;

-- ────────────────────────────────────────────────────────────────
-- email_send_log：按 IP + 按邮箱双维度限流（new-api 只有 IP）
-- ────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS email_send_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    email      TEXT    NOT NULL,
    ip         TEXT    NOT NULL,
    purpose    TEXT    NOT NULL,
    outcome    TEXT    NOT NULL DEFAULT 'sent',  -- sent | failed | rejected
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_esl_ip    ON email_send_log(ip, created_at);
CREATE INDEX IF NOT EXISTS ix_esl_email ON email_send_log(email, created_at);

-- ──────────────────────────────────────────────────────��─────────
-- auth_flows：对齐 new-api 的一次性流程（换绑/2FA/OAuth state）
-- ────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS auth_flows (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    token_hash  TEXT    NOT NULL UNIQUE,        -- sha256(token)，不存明文
    purpose     TEXT    NOT NULL,
    user_id     INTEGER NOT NULL DEFAULT 0,
    session_id  TEXT    NOT NULL DEFAULT '',
    payload     TEXT    NOT NULL DEFAULT '{}',  -- 换绑的 new/old code hash、重发时间、失败次数
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    consumed_at INTEGER
);
CREATE INDEX IF NOT EXISTS ix_af_purpose_expiry ON auth_flows(purpose, expires_at);
```

**为什么用 HMAC 而不是 Argon2**：验证码只有 6 位数字（10^6 空间），Argon2 对在线爆破没意义（攻击者只能在线试，5 次就锁）；HMAC + 常数时间比较足够，且**便宜 3 个数量级**（Argon2 每次 ~20ms，发码/校验会拖垮吞吐）。**用户密码才用 Argon2id**。

## D3. 需要哪些 crate

| 用途 | crate | 版本 | 说明 |
|---|---|---|---|
| SMTP 发信 | **`lettre`** | 0.11 | features: `smtp-transport`, `tokio1`, `builder`, `rustls-tls`（或 `native-tls`）。支持 **PLAIN / LOGIN / XOAUTH2 / CRAM-MD5**，`Tls::Required`（STARTTLS）/ `Tls::Wrapper`（465 隐式）/ `Tls::Opportunistic`；`SmtpTransport::builder_dangerous()` 用于无 TLS 内网。**比 new-api 的 net/smtp 强，且能发 Office365 OAuth2** |
| 邮件模板 | **`minijinja`**（项目已有） | 2 | **不推荐 askama**：askama 是编译期模板，多语言要 3 套结构体/枚举，改文案要重编译；minijinja 运行期加载 `templates/email/{lang}/{purpose}.html`，热更新友好。项目 `Cargo.toml` 已在用 `minijinja` + `minijinja-autoreload` |
| 密码哈希 | **`argon2`** | 0.5 | features `password-hash`；`Argon2::default()` 即 Argon2id m=19456,t=2,p=1，与 new-api 参数一致 |
| 验证码哈希 | `hmac` + `sha2` + `subtle` | 已有 | `Hmac<Sha256>` + `ct_eq` |
| 随机 | `rand` | 已有 0.8 | `rand::thread_rng().gen_range(0..1_000_000)` → `format!("{:06}", n)` **6 位纯数字**（修 new-api 的 hex 问题） |
| 时间 | `chrono` | 已有 | Unix 秒 |
| 输入校验 | `validator` 0.18 或 `garde` | — | email 用 `validator::validate_email`；**不要自己写正则** |
| 限流 | `governor` 0.7 / `tower_governor` 0.6 | — | 或手写 `DashMap<String, VecDeque<Instant>>`（项目已有 `tokio`，几十行即可，参考 new-api `common/rate-limit.go` 的滑动窗口） |
| i18n（后端） | `rust-i18n` 3 或 `fluent` | — | 只要 3 种语言时，`HashMap<&'static str, &'static str>` + `include_str!` 更简单；**邮件模板和 API 文案分开管** |
| 邮件解析（收件侧，已有） | `mailparse` | 已有 | |
| 日志/错误 | `tracing` / `thiserror` / `anyhow` | 已有 | |

> **不需要**：`askama`（除非你坚持编译期模板）、`gomail` 类库（Go 专用）、`mail-send`（维护弱）。

## D4. 关键函数签名（`rust/src/services/email_auth.rs`）

```rust
use std::time::Duration;

// ── 配置 ────────────────────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpSecurity { None, StartTls, ImplicitTls }   // 对齐 SMTPSSLEnabled / SMTPStartTLSEnabled

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,                 // SMTPServer
    pub port: u16,                    // SMTPPort (默认 587)
    pub security: SmtpSecurity,       // SMTPSSLEnabled / SMTPStartTLSEnabled
    pub username: String,             // SMTPAccount
    pub password: String,             // SMTPToken
    pub from: String,                 // SMTPFrom（空则回退 username）
    pub from_name: String,            // SystemName
    pub insecure_skip_verify: bool,   // SMTPInsecureSkipVerify
    pub force_auth_login: bool,       // SMTPForceAuthLogin
}

/// new-api 的 EmailLoginAuthServerList 等价物：这些 host 强制 AUTH LOGIN
/// （lettre 的 `Mechanism::Login`；默认 `Mechanism::Plain` 或自动）
pub const LOGIN_AUTH_SERVERS: &[&str] = &["smtp.sendcloud.net", "smtp.azurecomm.net"];

pub fn should_use_login_auth(host: &str, account: &str, force: bool) -> bool {
    force
        || account.contains("outlook")
        || account.contains("onmicrosoft")
        || LOGIN_AUTH_SERVERS.iter().any(|h| h.eq_ignore_ascii_case(host))
}

// ── 发信 ────────────────────────────────────────────────────────
pub async fn send_mail(
    cfg: &SmtpConfig,
    to: &str,
    subject: &str,
    html: &str,
    text: Option<&str>,               // 纯文本兜底（new-api 没有，建议补）
) -> Result<(), MailError>;

/// 渲染模板：templates/email/{lang}/{register|reset|bind}.html.j2
pub fn render_email(
    lang: &str,
    purpose: EmailPurpose,
    vars: &serde_json::Value,
) -> Result<(String /*subject*/, String /*html*/, String /*text*/), TemplateError>;

// ── 邮箱策略（对齐 service.ValidateAccountEmail）──────────────────
#[derive(Debug, Clone)]
pub struct EmailPolicy {
    pub domain_restriction_enabled: bool,
    pub domain_whitelist: Vec<String>,      // 全部小写、已 trim
    pub alias_restriction_enabled: bool,
    pub max_len: usize,                     // new-api 是 50
}

#[derive(Debug, thiserror::Error)]
pub enum EmailPolicyError {
    #[error("Please enter a valid email address")]
    Invalid,
    #[error("This email address is not allowed by the administrator's email policy.")]
    Restricted,
}

/// trim + lowercase + 格式校验 + 域名精确匹配 + 别名(+ / .)限制
pub fn validate_account_email(raw: &str, policy: &EmailPolicy) -> Result<String, EmailPolicyError>;

// ── 验证码 ──────────────────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodePurpose { Register, Reset, BindNew, BindOld }

impl CodePurpose {
    pub fn as_str(self) -> &'static str { /* "register" | "reset" | ... */ }
}

#[derive(Debug, Clone)]
pub struct VerificationConfig {
    pub valid_minutes: i64,          // new-api: 10
    pub code_len: usize,             // 6（纯数字）
    pub max_attempts: u32,           // 5
    pub resend_cooldown_secs: i64,   // 60
    pub per_ip_per_minute: u32,      // 建议 2（对齐 new-api 2/30s ≈ 4/min，这里更严）
    pub per_email_per_hour: u32,     // 建议 5（new-api 无此限制，必须补）
}

/// 签发：先作废旧活跃码 → 生成 6 位数字 → HMAC → 落库 → 发信
/// 返回 Err(RateLimited{retry_after}) 表示被限流（不泄露邮箱是否存在）
pub async fn issue_code(
    pool: &sqlx::SqlitePool,
    cfg: &VerificationConfig,
    smtp: &SmtpConfig,
    purpose: CodePurpose,
    email: &str,
    request_ip: &str,
    user_id: i64,
    lang: &str,
) -> Result<(), VerifyError>;

/// 校验并消费：常数时间比较；失败 attempts+1；达到上限即作废
pub async fn consume_code(
    pool: &sqlx::SqlitePool,
    purpose: CodePurpose,
    email: &str,
    code: &str,
) -> Result<(), VerifyError>;

// ── 注册 / 登录 ─────────────────────────────────────────────────
#[derive(Debug, serde::Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
    pub email: Option<String>,
    pub verification_code: Option<String>,
    pub aff_code: Option<String>,
}

pub async fn register_user(
    pool: &sqlx::SqlitePool,
    req: RegisterRequest,
    policy: &EmailPolicy,
    cfg: &VerificationConfig,
    require_email_verification: bool,
) -> Result<i64, RegisterError>;

/// 用户名或邮箱 + 密码；成功返回 session（access token + refresh cookie）
pub async fn login(
    pool: &sqlx::SqlitePool,
    username_or_email: &str,
    password: &str,
    ip: &str,
    ua: &str,
) -> Result<AuthBundle, LoginError>;

pub async fn request_password_reset(
    pool: &sqlx::SqlitePool,
    cfg: &VerificationConfig,
    smtp: &SmtpConfig,
    email: &str,
    ip: &str,
) -> Result<(), VerifyError>;   // 无论邮箱是否存在都返回 Ok（防枚举）
```

## D5. 实施顺序建议

| 阶段 | 内容 | 验收 |
|---|---|---|
| 1 | `users` + `verification_codes` + `email_send_log` 迁移；`validate_account_email` + 单测 | 域名白名单/别名/大小写/长度 全绿 |
| 2 | `issue_code` / `consume_code` + 双维度限流 | 并发 100 次仅 2 次成功；错 5 次锁定；旧码失效 |
| 3 | `send_mail`（lettre）+ minijinja 模板 + 3 语言 | 163/QQ/Gmail 各收一封，中文主题不乱码 |
| 4 | `register_user` / `login` / `request_password_reset` | 注册→登录→改密→找回 全链路 |
| 5 | 换绑双码流程（`auth_flows`） | 新旧邮箱各收一码，缺一不可 |

---

# 附录：与 one-api 的差异（**【确认】** 来自代码，非 changelog）

| 维度 | one-api（原版） | new-api（本 fork） |
|---|---|---|
| 邮箱验证码存储 | 内存 map | **仍是内存 map**（没改） |
| 邮箱唯一性 | 无唯一索引，无锁 | **应用层 advisory lock / `FOR UPDATE`** + 明确 `ErrEmailAlreadyTaken` |
| 会话 | `access_token` 单 token | **`user_sessions` + access token + refresh cookie + `auth_version`** |
| 权限 | 只有 role 数字 | **role + Casbin 细粒度 + 每用户覆盖** |
| 2FA | 无 | **TOTP + 备份码 + Passkey** |
| 密码哈希 | bcrypt（sha256 前置） | **Argon2id（默认）/ bcrypt（兼容）** |
| 邮箱换绑 | 无 | **双码 + 哈希 + 限次 + 安全通知** |
| 发信 | `net/smtp` + PLAIN | `net/smtp` + **PLAIN/LOGIN/NTLM 自动协商** + **Resend HTTP API** |
| 数据库 | MySQL 为主 | **SQLite 默认 / MySQL / PostgreSQL / ClickHouse（仅日志）** |

---

*报告完。所有 `文件:行号` 均基于 2026-11-20 的 `main` 分支快照；本地 `docs/reference/` 5 个文件与之逐行一致（`api-router.go` 仅 3 行 `DisableCache()` 差异，不影响本文结论）。*
