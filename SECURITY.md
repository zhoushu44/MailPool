# 安全策略

## 报告漏洞

请**不要**通过公开 Issue 报告安全漏洞。

请发邮件到项目维护者，或使用 GitHub 的
[Private vulnerability reporting](../../security/advisories/new) 功能。
我们会在 3 个工作日内回复。

## 部署安全清单

MailPool 会持有大量邮箱的 OAuth 凭据（`refresh_token`），部署时请务必：

- [ ] **必须设置 `MAILPOOL_ADMIN_PASSWORD`**。不设置时服务只允许本机回环地址访问
- [ ] **不要提交 `data/` 目录**。里面有 SQLite 数据库，包含全部账号凭据
- [ ] **不要提交 `.env`、`.secrets/`、`*.pem`、`*.key`**
- [ ] 生产环境走 HTTPS（反向代理终止 TLS），不要在公网裸跑 HTTP
- [ ] 定期轮换 `MAILPOOL_SESSION_SECRET`
- [ ] 限制容器出网：只需要访问 `login.microsoftonline.com`、`graph.microsoft.com`、`outlook.office365.com`

## 凭据泄露后的处置

1. 立刻更换 `MAILPOOL_ADMIN_PASSWORD`
2. 在管理后台吊销所有 API Key
3. 若 `refresh_token` 泄露：在 Microsoft 账户中心撤销该应用的授权，重新走 Device Code 授权
4. 检查 `data/mailpool.db` 是否被外带

## 已知的安全边界

- 管理端与客户端是**两套身份**：管理员会话（`admin_ok`）看全量数据，客户会话（`user_id`）只看自己订单的邮箱
- `/api/v1/mails`、`/api/v1/email/receive`、`/api/v1/email/release` 对客户身份做归属校验，防止横向读取他人验证码
- 匿名 API Key（未绑定 `user_id`）语义等同管理员，属于运营方自用凭据，**不要发给客户**
