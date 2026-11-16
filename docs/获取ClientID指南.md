# 获取 Outlook client_id 指南

> 解决场景：你有 Outlook 账号（email + password，甚至带 refresh_token），但缺 `client_id`，导致无法走 Graph/IMAP-XOAUTH2 通道收信。
> MailPool 导入格式：`email----password----client_id----refresh_token`

---

## 一、先搞懂 client_id 是什么

`client_id`（客户端 ID）是微软 Azure AD 应用的唯一标识（UUID 格式），用于 OAuth2 授权流程：

```
登录授权 → 拿到授权码 code → 换 access_token + refresh_token
```

关键点：
- **refresh_token 与 client_id 强绑定**：用 A 应用的 client_id 拿到的 refresh_token，只能用 A 的 client_id 刷新，换 B 就报 `invalid_grant`。
- 所以"从别人/别处买来的 token 号"，必须配套正确的 client_id 才能用。

**三条获取路径（按推荐顺序）：**

1. **用公开 client_id（Thunderbird 等开源客户端）—— 最快，无需注册 Azure**
2. 找回你之前用过的 client_id（config 备份 / 历史文件）
3. 自己注册 Azure 应用（一劳永逸，推荐长期使用）

---

## 二、路径一：公开 client_id（推荐先试）

微软邮件客户端（Thunderbird、K9 Mail、Evolution 等）注册的 Azure 应用是**公开应用**，client_id 直接写死在其源码里，可直接复用。

### Thunderbird 桌面版（最常用，自带 IMAP/SMTP scope）

```
Client ID:    9e5f94bc-e8a4-4e73-b8be-63364c29d753
Redirect URI: https://localhost
Scopes:       https://outlook.office.com/IMAP.AccessAsUser.All
              https://outlook.office.com/SMTP.Send
              offline_access
```

> 来源：[Mozilla OAuth2Providers 源码](https://github.com/mozilla/releases-comm-central/blob/master/mailnews/base/src/OAuth2Providers.sys.mjs)，支持 IMAP/POP/SMTP 全协议，社区接码项目（gpt-outlook-register 等）实测可用。

### 其他公开 client_id 备选

| 客户端 | Client ID | Redirect URI |
|---|---|---|
| Thunderbird Android | `e6f8716e-299d-4ed9-bbf3-453f192f44e5` | `https://login.live.com/oauth20_desktop.srf` |
| K9 Mail | `e647013a-ada4-4114-b419-e43d250f99c5` | `msauth://com.fsck.k9/...` |
| Evolution (GNOME) | `cc6e0693-0e26-4220-8322-9d363e308fc6` | `https://login.live.com/oauth20_desktop.srf` |
| GNOME Online Accounts | `8ef61e06-9fd5-49af-9b63-6983aede4213` | `goa-oauth2://localhost` |

### 使用方式

拿到 Thunderbird client_id 后，用 **Device Code Flow（设备码流程）**为你的账号重新获取 refresh_token（无需浏览器交互，无需客户端密钥）：

```
1. POST https://login.microsoftonline.com/common/oauth2/v2.0/devicecode
   body: client_id=9e5f94bc-e8a4-4e73-b8be-63364c29d753&scope=offline_access%20https%3A%2F%2Foutlook.office.com%2FIMAP.AccessAsUser.All
   → 返回 device_code + user_code + verification_uri

2. 用户打开 verification_uri，输入 user_code，登录账号并同意授权

3. POST https://login.microsoftonline.com/common/oauth2/v2.0/token
   body: grant_type=urn:ietf:params:oauth:grant-type:device_code
         client_id=9e5f94bc-e8a4-4e73-b8be-63364c29d753
         device_code=...
   → 返回 access_token + refresh_token
```

拿到 refresh_token 后即可导入 MailPool：

```
you@outlook.com----密码----9e5f94bc-e8a4-4e73-b8be-63364c29d753----M.C5xxx...
```

> 注意：使用公开 client_id 重拿的 token 通常**缺少 Graph scope**（只有 IMAP/SMTP），MailPool 会自动走 IMAP-XOAUTH2 通道收信，不影响取验证码。若要 Graph 通道，需用路径三注册自己的应用并添加 Graph 权限。

---

## 三、路径二：找回之前用过的 client_id

如果这些账号当初注册时配过 Azure 应用（你的 OutlookRegister 流程里 `oauth2.client_id`），先尝试找回：

### 查找位置（按可能性排序）

1. **OutlookRegister 的 config.json** —— 注册时填过，可能被清空/改过，看是否有备份：
   - 项目根目录 `config.json`（当前可能已被清空）
   - 复制出去的另一份 config / 压缩包 / 聊天记录
   - 其他目录（桌面、下载、文档）里的同名项目
2. **Azure 门户** —— 登录 [portal.azure.com](https://portal.azure.com) →「应用注册」→ 查看你名下所有应用，找当时的那个（按创建时间 / 名称回忆）
3. **token 本身** —— refresh_token 无法反推 client_id，只能逐个 client_id 试刷新：
   ```
   POST /token  grant_type=refresh_token&client_id=候选ID&refresh_token=...
   ```
   返回成功即为匹配的 client_id

### 判断标准

用候选 client_id 调刷新接口，成功返回新 token 就是正确的；报 `AADSTS7000215 / invalid_client` = 不对，`AADSTS7000216 / invalid_grant` = 账号被风控或 token 已失效。

---

## 四、路径三：注册自己的 Azure 应用（推荐长期使用）

自己注册的应用不依赖第三方，scope 可自由配置（Graph + IMAP 全都要），且不会受公开 client_id 策略调整影响。

### 注册步骤

1. 打开 [Azure 门户](https://portal.azure.com) → 搜索「应用注册」→「新注册」
2. 填名称（随意，如 `mailpool`）
3. **受支持的帐户类型**：选「个人 Microsoft 帐户」(Microsoft personal accounts) —— 关键！
   > 选错组织类型会导致个人 outlook.com 账号无法授权（AADSTS500011）
4. 重定向 URI 留空即可（Device Code Flow 不需要）
5. 注册完成后，记录页面顶部的 **应用程序（客户端）ID** 即为 `client_id`

### 配置权限（API 权限）

「API 权限」→「添加权限」→「Microsoft Graph」→「委托的权限」，添加：

| 权限 | 用途 |
|---|---|
| `offline_access` | 拿 refresh_token（关键） |
| `Mail.Read` | Graph 收信 |
| `Mail.ReadWrite` | 读+标记已读 |

> 若要用 IMAP-XOAUTH2 通道，还需添加 Outlook 的委托权限：
> 「Microsoft Graph」→「委托的权限」→ 搜索 `IMAP.AccessAsUser.All` 添加（需在工作负载列表中开启，或直接使用 Graph Mail.Read 走 Graph 通道即可，二选一）

### 启用公共客户端流（Device Code 必需）

「身份验证」→「高级设置」→ 打开 **「允许公共客户端流」**（Allow public client flows）→ 保存

### 拿到 client_id 后

用与路径一相同的 Device Code Flow 获取 refresh_token，或直接用于 MailPool 导入（若你已有该账号的 refresh_token，直接配 client_id 即可）。

---

## 五、Quick 操作速查

| 目标 | 怎么做 |
|---|---|
| 最快跑通（推荐） | 用 Thunderbird client_id `9e5f94bc-e8a4-4e73-b8be-63364c29d753` + Device Code Flow 重拿 token |
| 找回旧 client_id | 检查 config 备份 → Azure 门户应用注册列表 → 逐个试刷新 |
| 长期稳定 | 注册自己的 Azure 应用（个人账户 + offline_access + Mail.Read + 允许公共客户端流） |
| 检查 token 是否可用 | 用 client_id 调 `/token` 刷新，成功 = 可用；invalid_grant = 账号被风控 |

---

## 六、相关工具

| 工具 | 说明 |
|---|---|
| [Outlook-Oauth-GetToken](https://github.com/daimon3332/Outlook-Oauth-GetToken) | 单独为已有邮箱获取/重拿 refresh_token，不跑注册流程 |
| [msOauth2api](https://github.com/HChaoHui/msOauth2api) | 微软 OAuth2 取件 API 封装，支持 Graph/IMAP、批量刷新 token、在线邮箱管理 |
| [M365-IMAP](https://github.com/UvA-FNWI/M365-IMAP) | 官方式 OAuth2 令牌获取 + 刷新脚本（含 Thunderbird client_id 用法示例） |
| git-credential-outlook | 命令行工具，内置多客户端 client_id，支持 Device Code Flow |

---

*注：本指南仅供技术学习与合规使用。使用第三方公开 client_id 前请确认其开源许可与使用条款。*
