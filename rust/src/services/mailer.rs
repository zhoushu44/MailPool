//! 邮件发送：SMTP（lettre）+ 可选的邮件模板。
//!
//! ## 为什么用 lettre 而不是手写 net/smtp
//!
//! new-api 用的是 Go 标准库 `net/smtp`，有两个硬伤：
//! 1. **不支持 XOAUTH2** —— 无法用 Outlook / Gmail 的 OAuth2 令牌发信
//!    （这两个服务商已经全面禁用 basic auth）
//! 2. 认证方式要手写协商（它为此单独写了个 `email_ntlm_auth.go`）
//!
//! `lettre` 原生支持 PLAIN / LOGIN / XOAUTH2，且内置 TLS。
//!
//! ## 配置读取顺序
//!
//! 1. 数据库 `settings` 表的 `smtp.*` 键（管理后台可改，立即生效）
//! 2. 环境变量 `MAILPOOL_SMTP_*`（容器化部署时的兜底）
//!
//! 这样既能在后台改，也能用环境变量预置。

use std::time::Duration;

use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::services::settings;

/// SMTP 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    /// none / starttls / ssl
    pub encryption: String,
    pub username: String,
    pub password: String,
    /// 发件地址（可含显示名：`MailPool <noreply@x.com>`）
    pub from: String,
    /// 跳过证书校验（自签名 SMTP 用）
    pub skip_tls_verify: bool,
    /// 强制 AUTH LOGIN（部分国产 SMTP 只认这个）
    pub force_auth_login: bool,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 587,
            encryption: "starttls".to_string(),
            username: String::new(),
            password: String::new(),
            from: String::new(),
            skip_tls_verify: false,
            force_auth_login: false,
        }
    }
}

impl SmtpConfig {
    /// 是否配置完整（缺任何一项都发不了信）。
    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && !self.from.trim().is_empty()
    }

    /// 用于前端展示的脱敏副本（密码永远不返回）。
    pub fn masked(&self) -> serde_json::Value {
        serde_json::json!({
            "host": self.host,
            "port": self.port,
            "encryption": self.encryption,
            "username": self.username,
            "from": self.from,
            "skip_tls_verify": self.skip_tls_verify,
            "force_auth_login": self.force_auth_login,
            // 只告诉前端"有没有设过"，不给值
            "password_set": !self.password.is_empty(),
        })
    }
}

/// 从 settings 表读配置（键名 `smtp.*`），环境变量兜底。
pub async fn load_config(pool: &SqlitePool) -> AppResult<SmtpConfig> {
    let d = SmtpConfig::default();

    // 注意：不能用闭包返回 async block —— 参数借用会让生命周期无法满足
    // （编译器会报 "return type of closure contains a lifetime"）。
    // 直接逐个 await 最清楚。
    async fn get(pool: &SqlitePool, key: &str, default: &str) -> AppResult<String> {
        settings::get_value(pool, &format!("smtp.{key}"), default).await
    }

    let host = {
        let v = get(pool, "host", &d.host).await?;
        if v.is_empty() {
            std::env::var("MAILPOOL_SMTP_HOST").unwrap_or_default()
        } else {
            v
        }
    };

    let port = {
        let s = get(pool, "port", "587").await?;
        s.parse().unwrap_or_else(|_| {
            std::env::var("MAILPOOL_SMTP_PORT")
                .ok()
                .and_then(|x| x.parse().ok())
                .unwrap_or(587)
        })
    };

    let username = {
        let v = get(pool, "username", &d.username).await?;
        if v.is_empty() {
            std::env::var("MAILPOOL_SMTP_USERNAME").unwrap_or_default()
        } else {
            v
        }
    };
    let password = {
        let v = get(pool, "password", &d.password).await?;
        if v.is_empty() {
            std::env::var("MAILPOOL_SMTP_PASSWORD").unwrap_or_default()
        } else {
            v
        }
    };
    let from = {
        let v = get(pool, "from", &d.from).await?;
        if v.is_empty() {
            std::env::var("MAILPOOL_SMTP_FROM").unwrap_or_default()
        } else {
            v
        }
    };
    let encryption = {
        let v = get(pool, "encryption", &d.encryption).await?;
        if v.is_empty() {
            d.encryption.clone()
        } else {
            v
        }
    };

    Ok(SmtpConfig {
        host,
        port,
        encryption,
        username,
        password,
        from,
        skip_tls_verify: get(pool, "skip_tls_verify", "false").await? == "true",
        force_auth_login: get(pool, "force_auth_login", "false").await? == "true",
    })
}

/// 保存配置（空 password 表示「保持原值」，与前端"留空以保留"的语义一致）。
pub async fn save_config(pool: &SqlitePool, patch: &serde_json::Value) -> AppResult<()> {
    let pairs = [
        ("host", "host"),
        ("port", "port"),
        ("encryption", "encryption"),
        ("username", "username"),
        ("from", "from"),
        ("skip_tls_verify", "skip_tls_verify"),
        ("force_auth_login", "force_auth_login"),
    ];
    for (json_key, db_key) in pairs {
        if let Some(v) = patch.get(json_key) {
            if v.is_null() {
                continue;
            }
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Bool(b) => b.to_string(),
                other => other.to_string(),
            };
            settings::set_value(pool, &format!("smtp.{db_key}"), &s).await?;
        }
    }
    // 密码：非空才覆盖（留空 = 保留原值）
    if let Some(pw) = patch.get("password").and_then(|v| v.as_str()) {
        if !pw.is_empty() {
            settings::set_value(pool, "smtp.password", pw).await?;
        }
    }
    Ok(())
}

/// 构造 lettre 的 SMTP transport。
///
/// 返回类型必须显式标注 `AsyncSmtpTransport<Tokio1Executor>`：
/// `builder.build()` 的泛型无法推断时编译器会报 "type annotations needed"。
fn build_transport(cfg: &SmtpConfig) -> AppResult<AsyncSmtpTransport<Tokio1Executor>> {
    let tls_params = TlsParameters::builder(cfg.host.clone())
        .dangerous_accept_invalid_certs(cfg.skip_tls_verify)
        .build()
        .map_err(|e| AppError::bad_request(format!("TLS 参数无效: {e}")))?;

    let tls = match cfg.encryption.trim().to_ascii_lowercase().as_str() {
        "ssl" | "ssl/tls" | "tls" => Tls::Wrapper(tls_params),
        "starttls" => Tls::Required(tls_params),
        _ => Tls::Opportunistic(tls_params),
    };

    // 注意：`builder_dangerous` 返回的是 **SmtpTransportBuilder**，
    // 不是 AsyncSmtpTransport。标注结构体类型会让后续 .credentials() /
    // .build() 全部找不到（它们只存在于 builder 上）。
    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host)
        .port(cfg.port)
        .tls(tls)
        .timeout(Some(Duration::from_secs(20)));

    // 认证：密码为空时可能是内网无需认证的 relay
    if !cfg.username.trim().is_empty() {
        builder = builder.credentials(Credentials::new(
            cfg.username.trim().to_string(),
            cfg.password.clone(),
        ));
        // 部分国产 SMTP（sendcloud / azurecomm）只认 AUTH LOGIN，
        // 对应 new-api 的 EmailLoginAuthServerList 硬编码名单 —— 这里做成配置项
        if cfg.force_auth_login {
            builder = builder.authentication(vec![Mechanism::Login]);
        }
    }

    Ok(builder.build())
}

/// 发一封纯文本 + HTML 双版本的邮件。
///
/// 双版本是必须的：只有 HTML 会被部分客户端判垃圾邮件；只有纯文本
/// 又会让验证码排版难看。
pub async fn send_mail(
    cfg: &SmtpConfig,
    to: &str,
    subject: &str,
    text_body: &str,
    html_body: &str,
) -> AppResult<()> {
    if !cfg.is_configured() {
        return Err(AppError::bad_request(
            "SMTP 未配置（需要 host / username / from）",
        ));
    }
    let to = to.trim();
    if to.is_empty() || !to.contains('@') {
        return Err(AppError::bad_request("收件人邮箱格式不正确"));
    }

    let from_mbox: Mailbox = cfg
        .from
        .trim()
        .parse()
        .map_err(|e| AppError::bad_request(format!("发件地址格式不正确: {e}")))?;
    let to_mbox: Mailbox = to
        .parse()
        .map_err(|e| AppError::bad_request(format!("收件地址格式不正确: {e}")))?;

    let email = Message::builder()
        .from(from_mbox)
        .to(to_mbox)
        .subject(subject)
        .multipart(
            MultiPart::alternative()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(text_body.to_string()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_HTML)
                        .body(html_body.to_string()),
                ),
        )
        .map_err(|e| AppError::bad_request(format!("构造邮件失败: {e}")))?;

    let transport = build_transport(cfg)?;
    transport
        .send(email)
        .await
        .map_err(|e| AppError::bad_gateway(format!("SMTP 发送失败: {e}")))?;

    Ok(())
}

/// 发验证码邮件（带品牌样式）。
pub async fn send_verification_code(
    cfg: &SmtpConfig,
    to: &str,
    code: &str,
    site_name: &str,
    ttl_minutes: i64,
) -> AppResult<()> {
    let subject = format!("{site_name} 验证码：{code}");
    let text = format!(
        "您的验证码是：{code}\n\n\
         该验证码 {ttl_minutes} 分钟内有效，仅可使用一次。\n\
         如果这不是您本人的操作，请忽略本邮件。\n\n\
         —— {site_name}"
    );
    // 注意：code 是纯数字，不存在 HTML 注入风险；但 subject 里含用户可控的
    // site_name，仍统一转义一遍（防御性编程）
    let code_html = html_escape(code);
    let name_html = html_escape(site_name);
    let html = format!(
        r#"<!DOCTYPE html>
<html><body style="margin:0;padding:0;background:#f5f7fa;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif">
  <div style="max-width:480px;margin:40px auto;background:#fff;border-radius:12px;padding:32px;box-shadow:0 2px 8px rgba(0,0,0,.06)">
    <div style="font-size:18px;font-weight:600;color:#111;margin-bottom:8px">{name_html}</div>
    <div style="font-size:14px;color:#666;margin-bottom:24px">您的验证码是</div>
    <div style="font-size:36px;font-weight:700;letter-spacing:8px;color:#2563eb;text-align:center;padding:20px 0;background:#f0f5ff;border-radius:8px;font-family:ui-monospace,monospace">{code_html}</div>
    <div style="font-size:13px;color:#888;margin-top:24px;line-height:1.6">
      该验证码 <b>{ttl_minutes} 分钟内</b>有效，仅可使用一次。<br>
      如果这不是您本人的操作，请忽略本邮件。
    </div>
  </div>
</body></html>"#
    );
    send_mail(cfg, to, &subject, &text, &html).await
}

/// 发一封测试邮件（后台「测试连接」按钮用）。
pub async fn send_test_mail(cfg: &SmtpConfig, to: &str) -> AppResult<()> {
    let text = "这是一封来自 MailPool 的测试邮件。\n\n如果你收到它，说明 SMTP 配置正确。";
    let html = r#"<!DOCTYPE html>
<html><body style="font-family:sans-serif;padding:24px">
  <h2 style="color:#2563eb">SMTP 配置正常</h2>
  <p>这是一封来自 <b>MailPool</b> 的测试邮件。</p>
  <p>如果你收到它，说明 SMTP 配置正确，可以用于发送注册验证码。</p>
</body></html>"#;
    send_mail(cfg, to, "MailPool SMTP 测试邮件", text, html).await
}

/// 最小 HTML 转义（邮件正文里插值用）。
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_not_configured_when_empty() {
        let c = SmtpConfig::default();
        assert!(!c.is_configured());
    }

    #[test]
    fn config_configured_with_required_fields() {
        let c = SmtpConfig {
            host: "smtp.test.com".into(),
            username: "u@test.com".into(),
            from: "u@test.com".into(),
            ..Default::default()
        };
        assert!(c.is_configured());
    }

    #[test]
    fn masked_never_leaks_password() {
        let c = SmtpConfig {
            host: "smtp.test.com".into(),
            username: "u@test.com".into(),
            password: "SUPER_SECRET".into(),
            from: "u@test.com".into(),
            ..Default::default()
        };
        let m = c.masked();
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("SUPER_SECRET"), "密码绝不能出现在响应里: {s}");
        assert_eq!(m["password_set"], true);
    }

    #[test]
    fn html_escape_works() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a&b"), "a&amp;b");
    }

    #[tokio::test]
    async fn send_fails_clearly_when_unconfigured() {
        let c = SmtpConfig::default();
        let r = send_mail(&c, "a@b.test", "s", "t", "h").await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("SMTP 未配置"));
    }

    #[tokio::test]
    async fn send_rejects_bad_recipient() {
        let c = SmtpConfig {
            host: "smtp.test.com".into(),
            username: "u@test.com".into(),
            from: "u@test.com".into(),
            ..Default::default()
        };
        let r = send_mail(&c, "not-an-email", "s", "t", "h").await;
        assert!(r.is_err());
    }
}
