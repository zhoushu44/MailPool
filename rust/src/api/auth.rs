//! 用户注册 / 登录 API。
//!
//! ## 与 new-api 的差异（都是修正，不是随意改动）
//!
//! | 能力 | new-api | 这里 |
//! |---|---|---|
//! | 验证码存储 | 进程内 RAM | 数据库（多实例可用） |
//! | 验证码格式 | 6 位 hex | **6 位纯数字** |
//! | 验证码保护 | 明文 | HMAC-SHA256 |
//! | 尝试上限 | 无 | 5 次 |
//! | 校验后 | 不删除（10 分钟内可复用） | **立即删除** |
//! | 限流维度 | 只按 IP（2 次/30 秒） | **IP + 邮箱双维度** |
//! | 邮箱验证状态 | 无此字段 | `users.email_verified_at` |
//! | 邮箱验证码登录 | **没有** | 有（passwordless） |

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{self, AppState, Identity};
use crate::error::{admin_ok, api_err, ok, AppError, AppResult, CODE_OP_FAILED};
use crate::services::{billing, mailer, payment, settings, verification};

/// 注册相关路由。
pub fn router() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        // 公开：发验证码 + 注册 + 登录
        .route("/api/v1/auth/send-code", post(send_code))
        .route("/api/v1/auth/register", post(register))
        .route("/api/v1/auth/login-code", post(login_by_code))
        .route("/api/v1/auth/config", get(auth_config))
        // 需登录：查/改自己的邮箱
        .route("/api/v1/auth/me", get(me))
        .route("/api/v1/auth/rebind-email", post(rebind_email))
        // 管理：SMTP 配置
        .route(
            "/api/admin/settings/smtp",
            get(admin_get_smtp).post(admin_save_smtp),
        )
        .route("/api/admin/settings/smtp/test", post(admin_test_smtp))
        // 管理：注册策略
        .route(
            "/api/admin/settings/registration",
            get(admin_get_reg_policy).post(admin_save_reg_policy),
        )
}

// ────────────────────────────────────────────────────────────────
//  注册策略配置
// ────────────────────────────────────────────────────────────────

/// 注册策略（对应 new-api 的「身份验证 › 基本身份验证」那组开关）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegPolicy {
    /// 允许新用户注册
    pub allow_register: bool,
    /// 允许密码登录（关掉后只能邮箱验证码登录）
    pub allow_password_login: bool,
    /// 注册时强制邮箱验证（关掉 = 填了邮箱就能注册，不验证）
    pub require_email_verify: bool,
    /// 启用邮箱域名白名单
    pub domain_restriction: bool,
    /// 白名单域名（逗号分隔）
    pub domain_whitelist: String,
    /// 阻止 `user+alias@` 形式
    pub alias_restriction: bool,
}

impl Default for RegPolicy {
    fn default() -> Self {
        Self {
            allow_register: true,
            allow_password_login: true,
            require_email_verify: true,
            domain_restriction: false,
            domain_whitelist: "gmail.com,163.com,126.com,qq.com,outlook.com,hotmail.com,icloud.com,yahoo.com,foxmail.com"
                .to_string(),
            alias_restriction: false,
        }
    }
}

const REG_POLICY_KEY: &str = "register.policy";

async fn load_policy(pool: &sqlx::SqlitePool) -> AppResult<RegPolicy> {
    let raw = settings::get_value(pool, REG_POLICY_KEY, "{}").await?;
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

async fn save_policy(pool: &sqlx::SqlitePool, p: &RegPolicy) -> AppResult<()> {
    settings::set_value(pool, REG_POLICY_KEY, &serde_json::to_string(p)?).await
}

/// 校验邮箱是否符合域名白名单 / 别名策略。
fn check_email_policy(policy: &RegPolicy, email: &str) -> Result<(), String> {
    let email = email.trim().to_ascii_lowercase();
    let Some((local, domain)) = email.split_once('@') else {
        return Err("邮箱格式不正确".into());
    };

    if policy.alias_restriction && local.contains('+') {
        return Err("不允许使用邮箱别名（+ 号）".into());
    }

    if policy.domain_restriction {
        // 注意 new-api 的 bug：`strings.Split(value, ",")` 不 trim 不 lowercase，
        // 用户填 `gmail.com, 163.com`（带空格）就全部失配。这里统一归一化。
        let allowed: Vec<String> = policy
            .domain_whitelist
            .split(',')
            .map(|d| d.trim().to_ascii_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        if !allowed.iter().any(|d| d == domain) {
            return Err(format!("邮箱域名 {domain} 不在允许列表中"));
        }
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────
//  公开端点
// ────────────────────────────────────────────────────────────────

/// `GET /api/v1/auth/config` — 前端启动时读取（决定显示哪些登录方式）。
async fn auth_config(State(st): State<Arc<AppState>>) -> AppResult<Json<Value>> {
    let policy = load_policy(&st.pool).await?;
    let smtp = mailer::load_config(&st.pool).await?;
    Ok(ok(json!({
        "allow_register": policy.allow_register,
        "allow_password_login": policy.allow_password_login,
        "require_email_verify": policy.require_email_verify,
        // 前端据此决定"邮箱验证码登录"按钮是否可见
        "email_login_enabled": smtp.is_configured(),
    })))
}

#[derive(Debug, Deserialize)]
struct SendCodeBody {
    email: String,
    /// register（默认）/ reset
    #[serde(default)]
    purpose: Option<String>,
}

/// `POST /api/v1/auth/send-code` — 发验证码。
async fn send_code(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<SendCodeBody>,
) -> AppResult<Json<Value>> {
    let email = body.email.trim().to_ascii_lowercase();
    if email.is_empty() || !email.contains('@') {
        return Ok(api_err(CODE_OP_FAILED, "邮箱格式不正确"));
    }

    let policy = load_policy(&st.pool).await?;
    let purpose = body.purpose.as_deref().unwrap_or(verification::PURPOSE_REGISTER);

    // 注册用途：检查是否允许注册 + 邮箱是否已被占用
    if purpose == verification::PURPOSE_REGISTER {
        if !policy.allow_register {
            return Ok(api_err(CODE_OP_FAILED, "当前未开放注册"));
        }
        if let Err(msg) = check_email_policy(&policy, &email) {
            return Ok(api_err(CODE_OP_FAILED, msg));
        }
        let taken: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE lower(email) = ?")
                .bind(&email)
                .fetch_one(&st.pool)
                .await?;
        if taken > 0 {
            return Ok(api_err(CODE_OP_FAILED, "该邮箱已被注册"));
        }
    }

    // SMTP 必须可用
    let smtp = mailer::load_config(&st.pool).await?;
    if !smtp.is_configured() {
        return Ok(api_err(
            CODE_OP_FAILED,
            "邮件服务未配置，请联系管理员",
        ));
    }

    // 双维度限流
    match verification::check_rate_limit(&st.pool, &email, &identity.client_ip).await? {
        verification::RateCheck::Denied { reason, .. } => {
            return Ok(api_err(CODE_OP_FAILED, reason));
        }
        verification::RateCheck::Ok => {}
    }

    // 签发
    let code = verification::issue_code(
        &st.pool,
        &email,
        purpose,
        &identity.client_ip,
    )
    .await?;

    // 发信
    let site_name = settings::get_value(&st.pool, "site.name", "MailPool").await?;
    let send_res = mailer::send_verification_code(
        &smtp,
        &email,
        &code,
        &site_name,
        verification::CODE_TTL_MINUTES,
    )
    .await;

    match &send_res {
        Ok(_) => {
            verification::log_send(&st.pool, &email, &identity.client_ip, purpose, true).await?;
            tracing::info!("[auth] 已发送验证码 email={email} purpose={purpose}");
            Ok(ok(json!({
                "sent": true,
                "ttl_minutes": verification::CODE_TTL_MINUTES,
                "message": "验证码已发送，请查收邮件",
            })))
        }
        Err(e) => {
            verification::log_send(&st.pool, &email, &identity.client_ip, purpose, false).await?;
            // 发信失败要把刚签发的码删掉，否则用户拿到一个收不到的码的哈希
            let _ = sqlx::query(
                "DELETE FROM email_verification_codes WHERE email = ? AND purpose = ? AND consumed = 0",
            )
            .bind(&email)
            .bind(purpose)
            .execute(&st.pool)
            .await;
            tracing::warn!("[auth] 发信失败 email={email}: {e}");
            Ok(api_err(CODE_OP_FAILED, format!("邮件发送失败: {e}")))
        }
    }
}

#[derive(Debug, Deserialize)]
struct RegisterBody {
    username: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
}

/// `POST /api/v1/auth/register`
async fn register(
    State(st): State<Arc<AppState>>,
    Json(body): Json<RegisterBody>,
) -> AppResult<Json<Value>> {
    let policy = load_policy(&st.pool).await?;
    if !policy.allow_register {
        return Ok(api_err(CODE_OP_FAILED, "当前未开放注册"));
    }

    let username = body.username.trim();
    let email = body.email.trim().to_ascii_lowercase();

    // 用户名规则（new-api 只有 max=20，太松）
    if username.is_empty() {
        return Ok(api_err(CODE_OP_FAILED, "用户名不能为空"));
    }
    if username.chars().count() < 3 || username.chars().count() > 32 {
        return Ok(api_err(CODE_OP_FAILED, "用户名长度需在 3-32 个字符之间"));
    }
    if !username
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Ok(api_err(
            CODE_OP_FAILED,
            "用户名只能包含字母、数字、下划线、连字符和点",
        ));
    }

    // 密码规则（8-128 字符）
    if !body.password.is_empty() {
        let n = body.password.chars().count();
        if n < 8 || n > 128 {
            return Ok(api_err(CODE_OP_FAILED, "密码长度需在 8-128 个字符之间"));
        }
    } else if email.is_empty() {
        return Ok(api_err(CODE_OP_FAILED, "密码与邮箱至少填一个"));
    }

    // 用户名唯一
    if billing::get_user_by_name(&st.pool, username).await?.is_some() {
        return Ok(api_err(CODE_OP_FAILED, "用户名已存在"));
    }

    // 邮箱策略 + 唯一性
    let mut email_verified_at: Option<chrono::NaiveDateTime> = None;
    if !email.is_empty() {
        if let Err(msg) = check_email_policy(&policy, &email) {
            return Ok(api_err(CODE_OP_FAILED, msg));
        }
        let taken: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE lower(email) = ?")
                .bind(&email)
                .fetch_one(&st.pool)
                .await?;
        if taken > 0 {
            return Ok(api_err(CODE_OP_FAILED, "该邮箱已被注册"));
        }

        if policy.require_email_verify {
            // 必须带验证码且校验通过
            if body.code.trim().is_empty() {
                return Ok(api_err(CODE_OP_FAILED, "请先获取邮箱验证码"));
            }
            let (ok_flag, msg) = verification::verify_code(
                &st.pool,
                &email,
                &body.code,
                verification::PURPOSE_REGISTER,
            )
            .await?;
            if !ok_flag {
                return Ok(api_err(CODE_OP_FAILED, msg));
            }
            email_verified_at = Some(crate::util::now_naive());
        }
    }

    // 建号
    let hash = if body.password.is_empty() {
        String::new()
    } else {
        auth::hash_password(&body.password)
    };
    let uid = billing::create_user(&st.pool, username, &hash, crate::models::ROLE_USER, &email).await?;

    if let Some(t) = email_verified_at {
        sqlx::query("UPDATE users SET email_verified_at = ? WHERE id = ?")
            .bind(t)
            .bind(uid)
            .execute(&st.pool)
            .await?;
    }

    tracing::info!("[auth] 新用户注册 id={uid} username={username} email={email}");
    Ok(ok(json!({
        "id": uid,
        "username": username,
        "email": email,
        "email_verified": email_verified_at.is_some(),
        "message": "注册成功",
    })))
}

#[derive(Debug, Deserialize)]
struct LoginCodeBody {
    email: String,
    code: String,
}

/// `POST /api/v1/auth/login-code` — 邮箱验证码登录（**new-api 没有这个功能**）。
async fn login_by_code(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<LoginCodeBody>,
) -> AppResult<Json<Value>> {
    let email = body.email.trim().to_ascii_lowercase();

    let smtp = mailer::load_config(&st.pool).await?;
    if !smtp.is_configured() {
        return Ok(api_err(CODE_OP_FAILED, "邮件服务未配置"));
    }

    // 校验码（登录用独立 purpose，与注册码隔离）
    let (ok_flag, msg) = verification::verify_code(
        &st.pool,
        &email,
        &body.code,
        verification::PURPOSE_RESET,
    )
    .await?;
    if !ok_flag {
        return Ok(api_err(CODE_OP_FAILED, msg));
    }

    let user = sqlx::query("SELECT * FROM users WHERE lower(email) = ? LIMIT 1")
        .bind(&email)
        .fetch_optional(&st.pool)
        .await?;

    let Some(u) = user else {
        return Ok(api_err(CODE_OP_FAILED, "该邮箱未注册"));
    };

    let uid = crate::models::User::from_row(&u);
    if !uid.is_active() {
        return Ok(api_err(CODE_OP_FAILED, "账号已被禁用"));
    }

    // 签发会话（复用现有 cookie 机制）
    let mut sess = identity.session.clone();
    sess.insert("user_id".into(), json!(uid.id));
    sess.insert("username".into(), json!(uid.username.clone()));
    let token = auth::dumps(&sess);

    Ok(ok(json!({
        "user_id": uid.id,
        "username": uid.username,
        "session": token,
        "message": "登录成功",
    })))
}

// ────────────────────────────────────────────────────────────────
//  需登录
// ────────────────────────────────────────────────────────────────

/// `GET /api/v1/auth/me`
async fn me(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    let Some(uid) = identity.user_id else {
        return Err(AppError::unauthorized("需要登录"));
    };
    let Some(u) = billing::get_user(&st.pool, uid).await? else {
        return Err(AppError::not_found("用户不存在"));
    };
    let verified: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT email_verified_at FROM users WHERE id = ?")
            .bind(uid)
            .fetch_optional(&st.pool)
            .await?
            .flatten();

    Ok(ok(json!({
        "id": u.id,
        "username": u.username,
        "email": u.email,
        "email_verified": verified.is_some(),
        "email_verified_at": crate::util::iso_from_naive(verified),
        "balance": u.balance_money().to_yuan(),
        "available_balance": u.available_balance().to_yuan(),
    })))
}

#[derive(Debug, Deserialize)]
struct RebindBody {
    email: String,
    code: String,
}

/// `POST /api/v1/auth/rebind-email` — 换绑邮箱（需验证新邮箱）。
async fn rebind_email(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<RebindBody>,
) -> AppResult<Json<Value>> {
    let Some(uid) = identity.user_id else {
        return Err(AppError::unauthorized("需要登录"));
    };
    let email = body.email.trim().to_ascii_lowercase();
    let policy = load_policy(&st.pool).await?;

    if let Err(msg) = check_email_policy(&policy, &email) {
        return Ok(api_err(CODE_OP_FAILED, msg));
    }
    let taken: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE lower(email) = ? AND id <> ?",
    )
    .bind(&email)
    .bind(uid)
    .fetch_one(&st.pool)
    .await?;
    if taken > 0 {
        return Ok(api_err(CODE_OP_FAILED, "该邮箱已被其他账号使用"));
    }

    let (ok_flag, msg) = verification::verify_code(
        &st.pool,
        &email,
        &body.code,
        verification::PURPOSE_RESET,
    )
    .await?;
    if !ok_flag {
        return Ok(api_err(CODE_OP_FAILED, msg));
    }

    sqlx::query("UPDATE users SET email = ?, email_verified_at = ? WHERE id = ?")
        .bind(&email)
        .bind(crate::util::now_naive())
        .bind(uid)
        .execute(&st.pool)
        .await?;

    Ok(ok(json!({"email": email, "message": "换绑成功"})))
}

// ────────────────────────────────────────────────────────────────
//  管理：SMTP 设置
// ────────────────────────────────────────────────────────────────

fn ensure_admin(identity: &Identity) -> AppResult<()> {
    if identity.is_admin {
        Ok(())
    } else {
        Err(AppError::unauthorized(
            "需要管理员口令（X-Admin-Password 或后台登录）",
        ))
    }
}

async fn admin_get_smtp(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = mailer::load_config(&st.pool).await?;
    // 只为展示的占位：让前端知道"有没有配过"
    let policy = load_policy(&st.pool).await?;
    Ok(admin_ok(json!({
        "smtp": cfg.masked(),
        "registration": policy,
    })))
}

async fn admin_save_smtp(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(patch): Json<Value>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    mailer::save_config(&st.pool, &patch).await?;
    let cfg = mailer::load_config(&st.pool).await?;
    Ok(admin_ok(cfg.masked()))
}

#[derive(Debug, Deserialize)]
struct TestSmtpBody {
    #[serde(default)]
    to: String,
}

/// `POST /api/admin/settings/smtp/test` — 发一封测试邮件。
async fn admin_test_smtp(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<TestSmtpBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = mailer::load_config(&st.pool).await?;
    if !cfg.is_configured() {
        return Ok(api_err(CODE_OP_FAILED, "SMTP 未配置完整"));
    }
    let to = if body.to.trim().is_empty() {
        cfg.username.clone()
    } else {
        body.to.trim().to_string()
    };
    match mailer::send_test_mail(&cfg, &to).await {
        Ok(_) => Ok(ok(json!({"sent": true, "to": to, "message": "测试邮件已发送"}))),
        Err(e) => Ok(api_err(CODE_OP_FAILED, format!("发送失败: {e}"))),
    }
}

async fn admin_get_reg_policy(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    Ok(admin_ok(serde_json::to_value(load_policy(&st.pool).await?)?))
}

async fn admin_save_reg_policy(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(patch): Json<Value>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let mut p = load_policy(&st.pool).await?;
    if let Some(v) = patch.get("allow_register").and_then(|v| v.as_bool()) {
        p.allow_register = v;
    }
    if let Some(v) = patch.get("allow_password_login").and_then(|v| v.as_bool()) {
        p.allow_password_login = v;
    }
    if let Some(v) = patch.get("require_email_verify").and_then(|v| v.as_bool()) {
        p.require_email_verify = v;
    }
    if let Some(v) = patch.get("domain_restriction").and_then(|v| v.as_bool()) {
        p.domain_restriction = v;
    }
    if let Some(v) = patch.get("domain_whitelist").and_then(|v| v.as_str()) {
        p.domain_whitelist = v.to_string();
    }
    if let Some(v) = patch.get("alias_restriction").and_then(|v| v.as_bool()) {
        p.alias_restriction = v;
    }
    save_policy(&st.pool, &p).await?;
    Ok(admin_ok(serde_json::to_value(p)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_defaults() {
        let p = RegPolicy::default();
        assert!(p.allow_register);
        assert!(p.require_email_verify, "默认要求邮箱验证");
        assert!(!p.domain_restriction);
    }

    #[test]
    fn domain_whitelist_trims_and_lowercases() {
        // new-api 的 bug：不 trim 导致 "gmail.com, 163.com" 全部失配
        let p = RegPolicy {
            domain_restriction: true,
            domain_whitelist: "  Gmail.COM , 163.com ".into(),
            ..Default::default()
        };
        assert!(check_email_policy(&p, "a@gmail.com").is_ok());
        assert!(check_email_policy(&p, "b@163.com").is_ok());
        assert!(check_email_policy(&p, "c@qq.com").is_err());
    }

    #[test]
    fn alias_restriction() {
        let p = RegPolicy {
            alias_restriction: true,
            ..Default::default()
        };
        assert!(check_email_policy(&p, "user+alias@gmail.com").is_err());
        assert!(check_email_policy(&p, "user@gmail.com").is_ok());
    }

    #[test]
    fn bad_email_rejected() {
        let p = RegPolicy::default();
        assert!(check_email_policy(&p, "no-at-sign").is_err());
    }
}
