//! 页面路由层：把 Python 版 `app/web/routes.py` 的 21 个页面路由逐条搬到 axum。
//!
//! ## 为什么单独一层
//!
//! 模板层（[`crate::web::templates`]）只负责「渲染」，不含任何业务查询；
//! 页面层负责「取数 → 组装上下文 → 渲染 → 套上会话/跳转」。这样：
//! 1. 模板可以脱离 HTTP 单测（`templates.rs` 里已有门禁测试）；
//! 2. 页面路由的权限边界集中在这一个文件里，审计时只看这一处。
//!
//! ## 与 Python 版的语义对齐点
//!
//! | 语义 | Python | 这里 |
//! |---|---|---|
//! | 会话 cookie | itsdangerous 签名 | [`crate::auth::dumps`] / [`crate::auth::loads`]（字节兼容） |
//! | 登录成功跳转 | `303` | [`Redirect::to`]（axum 固定 SEE_OTHER） |
//! | 未登录跳转 | `302` | [`redirect_302`]（手动构造，`Redirect` 不提供 302） |
//! | 金额 | `fmt_milli` 字符串 / 模板里 `/1000` | 该传字符串的传字符串，该传厘的传整数厘 |
//! | 时间 | `datetime` 对象 | ISO8601 字符串，由模板层包成可 `.strftime()` 的值 |
//!
//! ## 权限模型（三层，和 Python `main.py` 的中间件一致）
//!
//! - **管理页**（`/` `/overview` `/accounts` `/mailbox` `/projects` `/pool`
//!   `/billing/*` `/settings` `/account`）：必须 [`Identity::is_admin`]，否则 302 `/login`；
//! - **客户页**（`/buy-email` `/orders` `/recharges` `/balances`）：管理员或已登录客户，
//!   否则 302 `/user/login`（管理员也要能看，Python 是 `check_admin_access` 先放行）；
//! - **公开页**（`/login` `/logout` `/user/login` `/user/logout` `/docs/*`）：不拦。
//!
//! 注意：`Identity::is_admin` 除了会话 `admin_ok`，还认 `X-Admin-Password` 头与
//! 回环地址免密 —— 这正是 Python `check_admin_access` 的口径，页面与 API 共用一套判定。

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Form, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use chrono::NaiveDateTime;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::auth::{AppState, Identity, SessionData, COOKIE_NAME, MAX_AGE_SECS};
use crate::config::{self, Settings};
use crate::error::AppResult;
use crate::models::RechargeOrder;
use crate::money::Money;
use crate::services::{billing, mail, pool, settings};
use crate::web::templates::Templates;

/// 当前唯一实现的邮箱源。写成常量是为了将来加 provider 时只改这一处
/// （对齐 Python `app/providers` 里只注册了 outlook 的现状）。
const PROVIDER: &str = "outlook";

// ────────────────────────────────────────────────────────────────
//  基础设施：响应 / 会话 cookie / 上下文
// ────────────────────────────────────────────────────────────────

/// 302 跳转（未登录时用）。
///
/// 为什么不用 `Redirect::to`：axum 把它固定成 303（SEE_OTHER）。
/// 303 会把后续请求强制改成 GET，对「未登录跳登录页」来说语义太强，
/// 而且 Python 版这里明确是 302，照抄以免客户端/测试断言不一致。
fn redirect_302(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .expect("固定的 Location 头一定合法")
}

/// 从请求 cookie 里还原会话。签名不对/过期 → 空会话（对齐 Python `get_session`）。
fn session_from_jar(jar: &CookieJar) -> SessionData {
    jar.get(COOKIE_NAME)
        .and_then(|c| crate::auth::loads(c.value()))
        .unwrap_or_default()
}

/// 生成会话 cookie。
///
/// 用 `Cookie::parse` 拼原始串而不是 builder，是因为 `max_age` 要
/// `cookie::time::Duration`，而 `cookie` 只是 axum-extra 的间接依赖，
/// 本项目没有直接依赖它（硬性要求：不新增 crate 依赖）。
/// 属性与 Python `set_session` 一致：httponly + samesite=lax + max_age=7 天。
fn session_cookie(token: &str) -> Cookie<'static> {
    Cookie::parse(format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={MAX_AGE_SECS}"
    ))
    .expect("会话 token 是 base64url + '.'，一定是合法 cookie 值")
}

/// 过期会话 cookie（登出）。
fn expired_cookie() -> Cookie<'static> {
    Cookie::parse(format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"
    ))
    .expect("固定串一定合法")
}

/// 全局上下文（对应 Python `_render` 的 `ctx.setdefault(...)` 五件套）。
///
/// 所有页面都要带：`base.html` 靠 `admin_ok` / `user_logged_in` 决定渲染哪套菜单，
/// 客户菜单里的「下单流水」还依赖 `billing_on`。
fn base_ctx(identity: &Identity, billing_on: bool, active: &str) -> Map<String, Value> {
    let mut ctx = Map::new();
    ctx.insert("admin_ok".into(), json!(identity.is_admin));
    ctx.insert("user_logged_in".into(), json!(identity.user_id.is_some()));
    ctx.insert(
        "has_password".into(),
        json!(Settings::get().has_admin_password()),
    );
    ctx.insert("billing_on".into(), json!(billing_on));
    ctx.insert("active".into(), json!(active));
    // 管理员也能一键切到客户视角（买号/充值页面）。
    // 计费没开时客户中心只有"购买邮箱"有意义，也给入口 —— 老板要看客户看到什么。
    ctx.insert("customer_mode_link".into(), json!(true));
    ctx
}

/// 客户中心的上下文。
///
/// ## 为什么单独一个函数
///
/// 管理后台和客户中心现在是**两套独立的布局**（`base_admin.html` /
/// `base_customer.html`），不再是同一个 `base.html` 靠 `if admin_ok` 切换。
///
/// 这样做的原因：原来那套单模板双分支有三个问题 ——
/// 1. 同一个浏览器**既登录管理员又登录客户**时，`admin_ok` 优先，
///    客户会看到管理导航（点进去被 302，但导航是可见的）；
/// 2. 管理员导航 14 项平铺，管理功能和客户功能混在一起；
/// 3. 客户登录后被跳到 `/recharges`，而客户的主场是 `/app/buy-email`。
///
/// 现在客户中心**只认 `user_id`**：管理员进客户视角也能用，但界面
/// 永远是客户那套，不会露出管理入口。
fn customer_ctx(identity: &Identity, billing_on: bool, active: &str) -> Map<String, Value> {
    let mut ctx = Map::new();
    // 客户中心不需要 admin_ok：模板里根本不用它做分支
    ctx.insert("user_logged_in".into(), json!(identity.user_id.is_some()));
    ctx.insert("user_name".into(), json!(identity.username()));
    ctx.insert("billing_on".into(), json!(billing_on));
    ctx.insert("active".into(), json!(active));
    // 保留 has_password：客户中心首页会提示"管理员尚未设置后台口令"
    ctx.insert(
        "has_password".into(),
        json!(Settings::get().has_admin_password()),
    );
    ctx
}

/// 渲染模板并包成 HTML 响应。
///
/// 统一走 [`Templates::render_json`]：它是唯一会把 `*_at` 字段包成
/// 支持 `.strftime()` 的值的入口，用 `render()` 传原始 JSON 会让模板报
/// `string has no method named strftime`。
fn render_html(t: &Templates, name: &str, ctx: &Map<String, Value>) -> AppResult<Response> {
    let html = t.render_json(name, &Value::Object(ctx.clone()))?;
    Ok(Html(html).into_response())
}

/// 读取查询参数里的整数（缺省 / 非法都退回默认值）。
///
/// 用 `HashMap<String, String>` 而不是强类型 `Query<T>`：Python 版对
/// `?page=abc` 会 422，但页面是给人点的，容忍脏参数比报错体验好。
fn q_i64(q: &HashMap<String, String>, key: &str, default: i64) -> i64 {
    q.get(key)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(default)
}

fn q_str<'a>(q: &'a HashMap<String, String>, key: &str) -> &'a str {
    q.get(key).map(|s| s.as_str()).unwrap_or("")
}

/// 每页最大条数（不同页面 Python 的上限不同，各自传进来）。
fn clamp_page(page: i64, page_size: i64, max_size: i64) -> (i64, i64) {
    (page.max(1), page_size.clamp(1, max_size))
}

/// 总页数：至少 1（空列表时模板仍显示「第 1 页 / 共 1 页」）。
fn total_pages(total: i64, page_size: i64) -> i64 {
    let size = page_size.max(1);
    ((total + size - 1) / size).max(1)
}

// ────────────────────────────────────────────────────────────────
//  Provider 元数据（模板里 providers 列表的来源）
// ────────────────────────────────────────────────────────────────

/// 内置 provider 元数据（对齐 Python `list_providers()` 的 `meta()` 形状）。
///
/// 为什么要在这里硬编码：Rust 侧还没有 provider 注册表（`providers/mod.rs` 只有
/// 具体的 Graph/IMAP 实现），但设置页、号池页、项目页都要按字段渲染表单。
/// 等注册表落地后，这个函数换成遍历注册表即可，模板不用改。
fn providers_meta() -> Vec<Value> {
    vec![json!({
        "id": PROVIDER,
        "name": "Outlook 私有邮箱池",
        "pooled": true,
        "ephemeral": false,
        "config_fields": [
            {
                "key": "timeout",
                "label": "默认收码超时(秒)",
                "type": "number",
                "required": false,
                "placeholder": "",
                "help": "",
                "default": "120",
            },
            {
                "key": "default_client_id",
                "label": "默认 client_id（3 段导入时自动填充）",
                "type": "text",
                "required": false,
                "placeholder": "9e5f94bc-e8a4-4e73-b8be-63364c29d753",
                "help": "导入 3 段格式 email----password----refresh_token 时自动套用此 client_id；留空则 3 段号入库时 client_id 为空（无法走 Graph/IMAP-XOAUTH2 通道）",
                "default": "",
            },
        ],
        "help_text": "账号格式（每行一个，---- 分隔）:\n4 段：email----password----client_id----refresh_token\n3 段：email----password----refresh_token（套用默认 client_id）\n注：微软已禁用 IMAP 密码登录，2 段格式无法收件，必须带 OAuth 凭据",
    })]
}

/// provider id → 显示名（Dashboard 卡片上的「邮箱源」）。
fn provider_name(id: &str) -> String {
    match id {
        PROVIDER => "Outlook 私有邮箱池".to_string(),
        other => other.to_string(),
    }
}

/// 内置 logo 库 + 解析后的图片地址（对齐 Python `_logos_with_src`）。
fn logos_with_src() -> Vec<Value> {
    config::PROJECT_LOGOS
        .iter()
        .map(|(id, name)| {
            json!({
                "id": id,
                "name": name,
                "src": config::project_logo_src(id),
            })
        })
        .collect()
}

/// 项目 JSON + 页面展示字段（可用数量 / 是否号池 / logo）。
///
/// `avail` 是**一次批量查询**的结果：Python 版按项目各跑一次全表扫描，
/// 53 个项目要 5 秒，这里复用 [`pool::count_claimable_bulk`] 的 `project_code → 数量`。
fn project_json(p: &crate::models::Project, avail: &HashMap<String, i64>) -> Value {
    let cfg = p.config();
    let logo = cfg.get("logo").and_then(|v| v.as_str()).unwrap_or("");
    // 只有号池型 provider 才有「剩余可 claim 数」；临时源（已移除）返回 null
    let is_pooled = p.provider == PROVIDER;
    let available = if is_pooled {
        avail.get(&p.code).copied()
    } else {
        None
    };
    json!({
        "id": p.id,
        "code": p.code,
        "name": p.name,
        "provider": p.provider,
        "config_json": p.config_json,
        "enabled": p.enabled,
        "is_pooled": is_pooled,
        "available": available,
        "logo_src": config::project_logo_src(logo),
        "provider_name": provider_name(&p.provider),
    })
}

// ────────────────────────────────────────────────────────────────
//  登录 / 登出
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    pub password: String,
}

/// `GET /login`
async fn login_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    // 登录页本身不显示 billing_on（它是独立模板，不继承 base.html），
    // 但仍按全局契约填准，避免以后模板加菜单时出现「管理员看到两套导航」。
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "");
    ctx.insert("error".into(), json!(""));
    render_html(&t, "login.html", &ctx)
}

/// `POST /login`
///
/// 成功：把 `admin_ok=true` **合并**进现有会话再下发 cookie（303 → `/`）。
/// 为什么是合并而不是新建：管理员和客户可能共用同一个浏览器 cookie，
/// 覆盖会话会把客户的 `user_id` 顶掉（Python 版注释里明确提过这一点）。
async fn login_submit(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> AppResult<Response> {
    if crate::auth::verify_admin_password(&form.password) {
        let mut sess = session_from_jar(&jar);
        sess.insert("admin_ok".into(), json!(true));
        let cookie = session_cookie(&crate::auth::dumps(&sess));
        return Ok((jar.add(cookie), Redirect::to("/")).into_response());
    }

    let identity = Identity::default();
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "");
    ctx.insert("error".into(), json!("口令错误"));
    render_html(&t, "login.html", &ctx)
}

/// `GET /logout` —— 清整个会话（管理员 + 客户身份一起清）。
async fn logout(jar: CookieJar) -> Response {
    (jar.add(expired_cookie()), Redirect::to("/login")).into_response()
}

#[derive(Debug, Deserialize)]
pub struct UserLoginForm {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

/// 旧的客户页路径 → 新路径。
///
/// 保留是为了不让老书签和外部链接失效。301 而不是 302：
/// 这是永久迁移，搜索引擎和浏览器都该记住新地址。
async fn legacy_customer_redirect(uri: axum::http::Uri) -> Response {
    let path = uri.path();
    let target = match path {
        "/buy-email" => "/app/buy-email",
        "/orders" => "/app/orders",
        "/recharges" => "/app/recharges",
        "/balances" => "/app/balances",
        _ => "/app/buy-email",
    };
    // 带上原查询串（分页/筛选参数不能丢）
    let target = match uri.query() {
        Some(q) => format!("{target}?{q}"),
        None => target.to_string(),
    };
    (
        StatusCode::MOVED_PERMANENTLY,
        [(header::LOCATION, target)],
    )
        .into_response()
}

/// `GET /user/login`
async fn user_login_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    // user_login.html 继承 base.html，未登录时走「公开访客」分支，
    // 但 billing_on 仍要准（客户一旦登录，菜单就会按它显示「下单流水」）。
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "");
    ctx.insert("error".into(), json!(""));
    ctx.insert("username".into(), json!(""));
    render_html(&t, "user_login.html", &ctx)
}

/// `POST /user/login`
///
/// 三条都要过：用户存在、`status == 1`（启用）、密码哈希校验通过。
/// 任何一种失败都给同一句「用户名或密码错误」——分开提示等于给撞库做用户名枚举。
async fn user_login_submit(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    jar: CookieJar,
    Form(form): Form<UserLoginForm>,
) -> AppResult<Response> {
    let username = form.username.trim();

    let mut error: Option<&str> = None;
    // (user_id, 是否管理员角色)
    let mut user_id: Option<(i64, bool)> = None;
    match billing::get_user_by_name(&st.pool, username).await? {
        Some(u) if u.status == 1 && !u.password_hash.is_empty() => {
            if crate::auth::verify_password(&form.password, &u.password_hash) {
                user_id = Some((u.id, u.role == crate::models::ROLE_ADMIN));
            } else {
                error = Some("用户名或密码错误");
            }
        }
        Some(_) => error = Some("用户名或密码错误"),
        None => error = Some("用户名或密码错误"),
    }

    if let Some((uid, is_admin_role)) = user_id {
        let mut sess = session_from_jar(&jar);
        sess.insert("user_id".into(), json!(uid));
        sess.insert("username".into(), json!(username));

        // role=admin 的账号同时授予后台权限。
        //
        // 后台本来是纯口令登录（/login 只认环境变量，没有账号概念），
        // 但「管理员账号」的预期是：一个账号密码登录，客户中心和管理
        // 后台都能进。在会话里补 admin_ok 即可 —— `Identity::is_admin`
        // 认这个标志，所有后台 API/页面自动放行。
        if is_admin_role {
            sess.insert("admin_ok".into(), json!(true));
        }

        let cookie = session_cookie(&crate::auth::dumps(&sess));
        // 客户登录后去「购买邮箱」，不是「余额与充值」。
        // 原来跳 /recharges 是照搬 Python 的行为，但客户的第一个动作
        // 几乎总是买号，跳到充值页等于多让人点一次。
        // 管理员角色则直接进后台仪表盘。
        let target = if is_admin_role { "/" } else { "/app/buy-email" };
        return Ok((jar.add(cookie), Redirect::to(target)).into_response());
    }

    // 失败页保留用户输入的用户名（模板 value="{{ username }}"），省得重打
    let identity = Identity::default();
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "");
    ctx.insert("error".into(), json!(error.unwrap_or("用户名或密码错误")));
    ctx.insert("username".into(), json!(form.username));
    render_html(&t, "user_login.html", &ctx)
}

/// `GET /user/logout` —— **只**清客户身份，管理员登录态保留。
///
/// 为什么：同一个 cookie 里可能同时有 `admin_ok` 和 `user_id`，
/// 客户点「退出」不应该把后台管理员一起踢下线。
async fn user_logout(jar: CookieJar) -> Response {
    let mut sess = session_from_jar(&jar);
    sess.remove("user_id");
    sess.remove("username");
    let cookie = session_cookie(&crate::auth::dumps(&sess));
    (jar.add(cookie), Redirect::to("/user/login")).into_response()
}

// ────────────────────────────────────────────────────────────────
//  仪表盘 / 静态壳页面
// ────────────────────────────────────────────────────────────────

/// `GET /` —— 仪表盘：池子计数 + 项目卡片 + 最近 10 封收件。
async fn dashboard(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }

    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let counts = pool::count_accounts(&st.pool, PROVIDER).await?;
    let projects = mail::list_projects(&st.pool).await?;
    // 一次算出所有项目的可 claim 数（53 个项目只打 3 条 SQL）
    let avail = pool::count_claimable_bulk(&st.pool, PROVIDER).await?;
    let project_rows: Vec<Value> = projects.iter().map(|p| project_json(p, &avail)).collect();

    let recent = mail::list_mail_records(&st.pool, None, None, None, 10, 0).await?;
    let recent_rows: Vec<Value> = recent
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "email": m.email,
                "project_code": m.project_code,
                "sender": m.sender,
                "subject": m.subject,
                "code": m.code,
                "received_at": crate::util::iso_from_naive(Some(m.received_at)),
            })
        })
        .collect();

    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "dashboard");
    ctx.insert("counts".into(), serde_json::to_value(&counts)?);
    ctx.insert("projects".into(), Value::Array(project_rows));
    ctx.insert("recent".into(), Value::Array(recent_rows));
    ctx.insert("logos".into(), Value::Array(logos_with_src()));
    render_html(&t, "dashboard.html", &ctx)
}

/// `GET /overview` —— 数据概览（页面数据全由前端 JS 拉 API，服务端无业务变量）。
///
/// 仍然要查 billing_on：`base.html` 的客户菜单按它决定是否显示「下单流水」，
/// 这个值全局上下文里每页都得是准的。
async fn overview_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let ctx = base_ctx(&identity, billing_cfg.is_enabled(), "overview");
    render_html(&t, "overview.html", &ctx)
}

/// `GET /accounts` —— 账号管理（同样是前端 JS 拉数据）。
async fn accounts_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let ctx = base_ctx(&identity, billing_cfg.is_enabled(), "accounts");
    render_html(&t, "accounts.html", &ctx)
}

/// `GET /mailbox?email=` —— 收件箱，`email` 用于预选左侧账号。
async fn mailbox_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "mailbox");
    ctx.insert("selected_email".into(), json!(q_str(&q, "email")));
    render_html(&t, "mailbox.html", &ctx)
}

// ────────────────────────────────────────────────────────────────
//  项目 / 号池 / 商城
// ────────────────────────────────────────────────────────────────

/// `GET /projects` —— 项目 CRUD 页（表单 + 列表 + logo 选择器）。
async fn projects_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }

    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let projects = mail::list_projects(&st.pool).await?;
    let avail = pool::count_claimable_bulk(&st.pool, PROVIDER).await?;
    let project_rows: Vec<Value> = projects.iter().map(|p| project_json(p, &avail)).collect();

    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "projects");
    ctx.insert("projects".into(), Value::Array(project_rows));
    ctx.insert("providers".into(), Value::Array(providers_meta()));
    ctx.insert("logos".into(), Value::Array(logos_with_src()));
    render_html(&t, "projects.html", &ctx)
}

/// 页面里的账号行（模板只读这几个字段，所以单独建一个视图结构，
/// 避免把 `account_json`（含 refresh_token）序列化进 HTML）。
fn account_json_row(a: &crate::models::EmailAccount, used_by: &str) -> Value {
    json!({
        "id": a.id,
        "email": a.email,
        "provider": a.provider,
        "status": a.status,
        "pool_status": a.pool_status,
        "health_status": a.health_status,
        "health_reason": a.health_reason,
        // 模板用它选徽章；不要直接用遗留 status 列（可能与 pool_status 脱节）
        "display_status": a.display_status(),
        "check_result": a.check_result,
        "fail_count": a.fail_count,
        "tags": a.tags,
        "proxy": a.proxy,
        "remark": a.remark,
        "used_by": used_by,
        "last_check_at": crate::util::iso_from_naive(a.last_check_at),
        "created_at": crate::util::iso_from_naive(Some(a.created_at)),
    })
}

/// 本页账号的「被哪些项目绑定」映射（email 小写 → "p1,p2"）。
///
/// 只查当前页的邮箱（IN 查询），不整表扫：995 行的 `email_usages` 全量拉回来做
/// 字符串拼接，在号池页每次翻页都白烧一遍 CPU。
async fn email_usage_map(
    pool_ref: &sqlx::SqlitePool,
    emails: &[String],
) -> AppResult<HashMap<String, String>> {
    if emails.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = std::iter::repeat("?")
        .take(emails.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT email, project_code FROM email_usages WHERE email IN ({placeholders}) \
         ORDER BY first_used_at"
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&sql);
    for e in emails {
        q = q.bind(e.to_ascii_lowercase());
    }
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (email, code) in q.fetch_all(pool_ref).await? {
        out.entry(email.to_ascii_lowercase()).or_default().push(code);
    }
    Ok(out
        .into_iter()
        .map(|(e, codes)| (e, codes.join(",")))
        .collect())
}

/// `client_id` 维度统计（对齐 Python `pool_service.stats_by_client_id`）。
///
/// 为什么要它：微软是按 client_id 封的，同一个 client_id 的号会集体失效。
/// 这一栏能一眼看出「是不是某个 client_id 被拉黑了」。
async fn cid_stats(pool_ref: &sqlx::SqlitePool) -> AppResult<Vec<Value>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT account_json, pool_status, health_status FROM email_accounts WHERE provider = ?",
    )
    .bind(PROVIDER)
    .fetch_all(pool_ref)
    .await?;

    #[derive(Default)]
    struct Bucket {
        total: i64,
        healthy: i64,
        unhealthy: i64,
        dead: i64,
        unknown: i64,
        available: i64,
        using: i64,
    }

    let mut buckets: HashMap<String, Bucket> = HashMap::new();
    for (raw, pool_status, health_status) in rows {
        let cid = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| {
                v.get("client_id")
                    .and_then(|c| c.as_str())
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_default();
        // 脱敏：client_id 是凭据的一部分，页面上只露头尾
        let display = if cid.len() > 16 {
            format!("{}...{}", &cid[..8], &cid[cid.len() - 4..])
        } else if cid.is_empty() {
            "(空)".to_string()
        } else {
            cid
        };

        let b = buckets.entry(display).or_default();
        b.total += 1;
        match pool_status.as_str() {
            "available" => b.available += 1,
            "using" => b.using += 1,
            _ => {}
        }
        match health_status.as_str() {
            "healthy" => b.healthy += 1,
            "unhealthy" => b.unhealthy += 1,
            "dead" => b.dead += 1,
            _ => b.unknown += 1,
        }
    }

    let mut out: Vec<Value> = buckets
        .into_iter()
        .map(|(cid, b)| {
            let rate = if b.total > 0 {
                ((b.healthy as f64) / (b.total as f64) * 1000.0).round() / 10.0
            } else {
                0.0
            };
            json!({
                "client_id": cid,
                "total": b.total,
                "healthy": b.healthy,
                "unhealthy": b.unhealthy,
                "dead": b.dead,
                "unknown": b.unknown,
                "available": b.available,
                "using": b.using,
                "healthy_rate": rate,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b["total"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["total"].as_i64().unwrap_or(0))
    });
    Ok(out)
}

/// `GET /pool?limit=&offset=` —— 邮箱池（分页）。
async fn pool_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }

    // 默认 200 而不是 500：500 行 HTML 约 403KB / 6600 个 DOM 节点，
    // 首屏传输 + 解析要 2 秒以上；要更多可以显式 ?limit=500。
    let limit = q_i64(&q, "limit", 200).clamp(20, 1000);
    let offset = q_i64(&q, "offset", 0).max(0);

    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let (accounts, _total) = pool::list_accounts(&st.pool, PROVIDER, "", limit, offset).await?;
    let counts = pool::count_accounts(&st.pool, PROVIDER).await?;
    let health = pool::health_summary(&st.pool, PROVIDER).await?;

    let emails: Vec<String> = accounts.iter().map(|a| a.email.clone()).collect();
    let used = email_usage_map(&st.pool, &emails).await?;
    let account_rows: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let key = a.email.to_ascii_lowercase();
            account_json_row(a, used.get(&key).map(|s| s.as_str()).unwrap_or(""))
        })
        .collect();

    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "pool");
    ctx.insert("accounts".into(), Value::Array(account_rows));
    ctx.insert("counts".into(), serde_json::to_value(&counts)?);
    ctx.insert("health".into(), serde_json::to_value(&health)?);
    ctx.insert("cid_stats".into(), Value::Array(cid_stats(&st.pool).await?));
    ctx.insert("providers".into(), Value::Array(providers_meta()));
    ctx.insert("page_limit".into(), json!(limit));
    ctx.insert("page_offset".into(), json!(offset));
    render_html(&t, "pool.html", &ctx)
}

/// `GET /buy-email` —— 商城：项目卡片按「价格 / 库存 / 过滤规则」展示。



/// `GET /app/exclusive` —— 独占邮箱页。
async fn app_exclusive_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    if !identity.is_admin && identity.user_id.is_none() {
        return Ok(redirect_302("/app/login"));
    }
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let ctx = customer_ctx(&identity, billing_cfg.is_enabled(), "exclusive");
    render_html(&t, "app_exclusive.html", &ctx)
}

/// `GET /app/register` —— 客户注册页。
///
/// 后端注册接口早就有（`/api/v1/auth/register` + `/api/v1/auth/send-code`），
/// 但一直没有页面入口。迈巢的登录页有「没有账户？注册」，我们缺这块。
async fn app_register_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let ctx = customer_ctx(&identity, billing_cfg.is_enabled(), "");
    render_html(&t, "app_register.html", &ctx)
}

/// `GET /app/mailbox` —— 客户收件页。
///
/// 购买页点「获取邮箱」后跳到这里（带 `?project_code=&email=`），
/// 页面用 `/api/app/mailbox/*` 拉邮件并自动解析验证码。
///
/// 之前这个路由不存在，客户点「获取邮箱」直接 404 —— 复刻时漏掉的整块。
async fn app_mailbox_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    if !identity.is_admin && identity.user_id.is_none() {
        return Ok(redirect_302("/app/login"));
    }
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    // 用 customer_ctx：属于客户中心，不能带 admin_ok 走后台布局
    let mut ctx = customer_ctx(&identity, billing_cfg.is_enabled(), "mailbox");
    ctx.insert("selected_email".into(), json!(q_str(&q, "email")));
    ctx.insert("selected_project".into(), json!(q_str(&q, "project_code")));
    render_html(&t, "app_mailbox.html", &ctx)
}

async fn buy_email_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    // 客户页：管理员或已登录客户都能看（未登录去客户登录页）
    if !identity.is_admin && identity.user_id.is_none() {
        return Ok(redirect_302("/app/login"));
    }

    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let billing_on = billing_cfg.is_enabled();
    let projects = mail::list_projects(&st.pool).await?;
    let avail = pool::count_claimable_bulk(&st.pool, PROVIDER).await?;

    let mut rows = Vec::with_capacity(projects.len());
    for p in &projects {
        let mut v = project_json(p, &avail);
        let cfg = p.config();
        let text = |k: &str| {
            cfg.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };

        // 卡片上的过滤说明：query 优先，其次 sender_domains（与 Python 一致）
        let query = {
            let qv = text("query");
            if qv.is_empty() {
                text("sender_domains")
            } else {
                qv
            }
        };
        let duration = {
            let tv = text("timeout");
            if tv.is_empty() {
                // provider 未配置 timeout 时回落到全局默认（默认 120 秒）
                Settings::get().default_timeout.to_string()
            } else {
                tv
            }
        };
        // 真实单价：计费关闭时留空串 → 模板显示「免费」
        let price_milli = if billing_on {
            billing::project_price(&st.pool, p, Some(&billing_cfg))
                .await?
                .milli()
        } else {
            0
        };
        let original = cfg
            .get("original_price")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        v["card_query"] = json!(query);
        v["card_keywords"] = json!(text("mail_keywords"));
        v["card_duration"] = json!(duration);
        v["card_price"] = json!(if billing_on {
            Money::from_milli(price_milli).to_yuan()
        } else {
            String::new()
        });
        v["card_original_price"] = json!(if billing_on && !original.is_empty() {
            Money::parse_yuan(original).unwrap_or_default().to_yuan()
        } else {
            String::new()
        });
        rows.push(v);
    }

    let mut ctx = customer_ctx(&identity, billing_on, "buy-email");
    ctx.insert("projects".into(), Value::Array(rows));
    ctx.insert("billing_on".into(), json!(billing_on));
    ctx.insert("providers".into(), Value::Array(providers_meta()));
    render_html(&t, "buy_email.html", &ctx)
}

// ────────────────────────────────────────────────────────────────
//  下单流水 / 费用明细 / 充值
// ────────────────────────────────────────────────────────────────

/// `email_orders` 的一行（只取页面需要的列）。
#[derive(Debug, sqlx::FromRow)]
struct EmailOrderRow {
    id: i64,
    user_id: i64,
    email: String,
    project_code: String,
    price: i64,
    billing_status: String,
    charged_amount: i64,
    created_at: NaiveDateTime,
    charged_at: Option<NaiveDateTime>,
}

/// 分页查 `email_orders`（`user_id=None` = 全部用户，管理员视角）。
async fn list_email_orders(
    pool_ref: &sqlx::SqlitePool,
    user_id: Option<i64>,
    project_code: Option<&str>,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<EmailOrderRow>, i64)> {
    let mut where_sql = String::from(" WHERE 1=1");
    if user_id.is_some() {
        where_sql.push_str(" AND user_id = ?");
    }
    if project_code.filter(|s| !s.is_empty()).is_some() {
        where_sql.push_str(" AND project_code = ?");
    }

    let count_sql = format!("SELECT COUNT(*) FROM email_orders{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    if let Some(uid) = user_id {
        cq = cq.bind(uid);
    }
    if let Some(pc) = project_code.filter(|s| !s.is_empty()) {
        cq = cq.bind(pc);
    }
    let total = cq.fetch_one(pool_ref).await?;

    let list_sql = format!(
        "SELECT id, user_id, email, project_code, price, billing_status, charged_amount, \
         created_at, charged_at FROM email_orders{where_sql} ORDER BY id DESC LIMIT ? OFFSET ?"
    );
    let mut lq = sqlx::query_as::<_, EmailOrderRow>(&list_sql);
    if let Some(uid) = user_id {
        lq = lq.bind(uid);
    }
    if let Some(pc) = project_code.filter(|s| !s.is_empty()) {
        lq = lq.bind(pc);
    }
    let rows = lq.bind(limit).bind(offset).fetch_all(pool_ref).await?;
    Ok((rows, total))
}

/// 用户 id → 用户名（管理页展示用）。只取前 500 个，和 Python 的默认一致。
async fn user_name_map(pool_ref: &sqlx::SqlitePool) -> AppResult<HashMap<i64, String>> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, username FROM users ORDER BY id ASC LIMIT 500")
            .fetch_all(pool_ref)
            .await?;
    Ok(rows.into_iter().collect())
}

/// `GET /orders?project_code=&page=&page_size=` —— 下单流水。
///
/// 计费开启读 `email_orders`（真订单），关闭时回落到 `email_usages` 聚合。
/// 计费关闭时聚合表没有用户归属，客户看会看到全平台数据 → 一律引导去商城。
async fn orders_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    if !identity.is_admin && identity.user_id.is_none() {
        return Ok(redirect_302("/app/login"));
    }

    let (page, page_size) = clamp_page(q_i64(&q, "page", 1), q_i64(&q, "page_size", 20), 100);
    let project_code = q_str(&q, "project_code").to_string();
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let billing_on = billing_cfg.is_enabled();
    let is_admin = identity.is_admin;
    let uid = identity.user_id.unwrap_or(0);
    let offset = (page - 1) * page_size;

    // 计费关闭 + 非管理员：这个用户没有「自己的订单」可言。
    //
    // 原来直接 303 回商城，但跳转目标是旧路径 `/buy-email`，
    // 浏览器再跟一次 301 才到 `/app/buy-email` —— 用户看到的是
    // 「点订单进到了购买页」，像是页面串了。
    //
    // 注意：**不要**在计费关闭时直接渲染空页。
    //
    // 订单 = 取号记录，和「是否计费」是两件事。原来这里直接 return 空状态，
    // 结果是客户明明取过号、email_orders 里有记录，下单流水却永远显示
    // 「未接入计费，不产生订单记录」—— 页面等于废的。
    //
    // 计费开关只影响「显示单价/计费状态」这些列，不影响要不要列记录。

    let projects = mail::list_projects(&st.pool).await?;

    // 一次查询同时拿到「本页行 + 总条数」——分页总数就在同一个函数的 COUNT 里，
    // 不要再为拿 total 重复跑一遍同样的两条 SQL（旧写法每次请求多打 2 次全表扫）
    let (rows, total): (Vec<Value>, i64) = if billing_on || !is_admin {
        // 计费开启 → 完整订单（含单价/计费状态）
        // 计费关闭 → 同样查订单，只是价格列显示为免费
        let (orders, total) = list_email_orders(
            &st.pool,
            if is_admin { None } else { Some(uid) },
            Some(&project_code),
            page_size,
            offset,
        )
        .await?;
        let proj_names: HashMap<String, String> = projects
            .iter()
            .map(|p| {
                (
                    p.code.clone(),
                    if p.name.is_empty() {
                        p.code.clone()
                    } else {
                        p.name.clone()
                    },
                )
            })
            .collect();
        let names = if is_admin {
            user_name_map(&st.pool).await?
        } else {
            HashMap::new()
        };
        let rows: Vec<Value> = orders
            .iter()
            .map(|o| {
                json!({
                    "order_id": o.id,
                    "email": o.email,
                    "project_code": o.project_code,
                    "project_name": proj_names
                        .get(&o.project_code)
                        .cloned()
                        .unwrap_or_else(|| o.project_code.clone()),
                    // 模板直接输出字符串（不再做除法），避免模板里丢精度
                    "price": Money::from_milli(o.price).to_yuan(),
                    "billing_status": o.billing_status,
                    "charged_amount": Money::from_milli(o.charged_amount).to_yuan(),
                    // 订单没有「收件时间」，用建单/扣费时间补位（与 Python 字段名一致）
                    "first_used_at": crate::util::iso_from_naive(Some(o.created_at)),
                    "last_received_at": crate::util::iso_from_naive(o.charged_at),
                    "code": "",
                    "mail_count": 0,
                    // 订单全部来自 Outlook 号池
                    "pooled": true,
                    "user_id": o.user_id,
                    "username": names.get(&o.user_id).cloned().unwrap_or_default(),
                })
            })
            .collect();
        (rows, total)
    } else {
        let (summaries, total) = mail::list_email_summaries(
            &st.pool,
            Some(&project_code),
            page_size,
            offset,
        )
        .await?;
        (
            summaries
                .iter()
                .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
                .collect(),
            total,
        )
    };

    let mut ctx = customer_ctx(&identity, billing_on, "orders");
    ctx.insert("rows".into(), Value::Array(rows));
    ctx.insert("total".into(), json!(total));
    ctx.insert("page".into(), json!(page));
    ctx.insert("page_size".into(), json!(page_size));
    ctx.insert("total_pages".into(), json!(total_pages(total, page_size)));
    ctx.insert("projects".into(), Value::Array(
        projects
            .iter()
            .map(|p| json!({"id": p.id, "code": p.code, "name": p.name}))
            .collect(),
    ));
    ctx.insert("filter_project".into(), json!(project_code));
    ctx.insert("is_admin".into(), json!(is_admin));
    render_html(&t, "orders.html", &ctx)
}

/// `GET /balances?log_type=&page=&page_size=` —— 费用明细。
///
/// 计费开启读 `balance_logs`（真流水），关闭时回落到 `mail_records`（收件流水，全免费）。
async fn balances_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    if !identity.is_admin && identity.user_id.is_none() {
        return Ok(redirect_302("/app/login"));
    }

    let (page, page_size) = clamp_page(q_i64(&q, "page", 1), q_i64(&q, "page_size", 20), 100);
    let log_type = q_str(&q, "log_type").to_string();
    let offset = (page - 1) * page_size;
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let billing_on = billing_cfg.is_enabled();
    let is_admin = identity.is_admin;
    let uid = identity.user_id.unwrap_or(0);

    let mut records: Vec<Value> = Vec::new();
    let total: i64;

    if billing_on {
        let (logs, n) = billing::list_balance_logs(
            &st.pool,
            if is_admin { None } else { Some(uid) },
            page_size,
            offset,
            Some(&log_type),
        )
        .await?;
        total = n;
        let names = if is_admin {
            user_name_map(&st.pool).await?
        } else {
            HashMap::new()
        };
        for r in &logs {
            records.push(json!({
                "id": r.id,
                "log_type": r.log_type,
                // 模板里拿字符串直接显示；用厘会变成「18」而不是「0.018」
                "amount": Money::from_milli(r.amount).to_yuan(),
                "balance_before": Money::from_milli(r.balance_before).to_yuan(),
                "balance_after": Money::from_milli(r.balance_after).to_yuan(),
                "frozen_before": Money::from_milli(r.frozen_before).to_yuan(),
                "frozen_after": Money::from_milli(r.frozen_after).to_yuan(),
                "remark": r.remark,
                "user_id": r.user_id,
                "username": names.get(&r.user_id).cloned().unwrap_or_default(),
                "created_at": crate::util::iso_from_naive(Some(r.created_at)),
            }));
        }
    } else {
        // 计费关闭时的「收件流水」。
        //
        // ⚠ 必须限定在这个客户自己的邮箱范围内。
        // 原来直接 `list_mail_records(None, ...)` 拉了全表 ——
        // 客户能在费用明细里看到**其他所有人**的邮箱和取号记录
        // （实测 zs 看到 1676 条别人的记录），这是严重的越权泄露。
        let my_emails: Vec<String> = if is_admin {
            Vec::new() // 管理员看全部
        } else {
            sqlx::query_scalar(
                "SELECT DISTINCT email FROM email_orders WHERE user_id = ?",
            )
            .bind(uid)
            .fetch_all(&st.pool)
            .await?
        };

        let mine_opt: Option<&[String]> = if is_admin { None } else { Some(&my_emails) };

        let recs = mail::list_mail_records(
            &st.pool, None, None, mine_opt, page_size, offset,
        )
        .await?;
        total = if is_admin {
            mail::count_mail_records(&st.pool).await?
        } else if my_emails.is_empty() {
            0
        } else {
            mail::count_mail_records_by_emails(&st.pool, &my_emails).await?
        };

        for r in &recs {
            records.push(json!({
                "id": r.id,
                "log_type": "",
                "amount": "免费",
                "balance_before": "—",
                "balance_after": "—",
                "frozen_before": "—",
                "frozen_after": "—",
                "remark": format!("{} {}", r.email, r.project_code).trim().to_string(),
                "created_at": crate::util::iso_from_naive(Some(r.received_at)),
            }));
        }
    }

    let snapshot = billing::balance_snapshot(&st.pool, uid).await?;

    let mut ctx = customer_ctx(&identity, billing_on, "balances");
    ctx.insert("records".into(), Value::Array(records));
    ctx.insert("snapshot".into(), serde_json::to_value(&snapshot)?);
    ctx.insert("total".into(), json!(total));
    ctx.insert("page".into(), json!(page));
    ctx.insert("page_size".into(), json!(page_size));
    ctx.insert("total_pages".into(), json!(total_pages(total, page_size)));
    ctx.insert("log_type".into(), json!(log_type));
    ctx.insert("is_admin".into(), json!(is_admin));
    render_html(&t, "balances.html", &ctx)
}

/// 分页查充值订单（`user_id=None` = 管理员看全部）。
async fn list_recharge_orders(
    pool_ref: &sqlx::SqlitePool,
    user_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<RechargeOrder>, i64)> {
    let where_sql = if user_id.is_some() {
        " WHERE user_id = ?"
    } else {
        ""
    };

    let count_sql = format!("SELECT COUNT(*) FROM recharge_orders{where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    if let Some(uid) = user_id {
        cq = cq.bind(uid);
    }
    let total = cq.fetch_one(pool_ref).await?;

    let list_sql =
        format!("SELECT * FROM recharge_orders{where_sql} ORDER BY id DESC LIMIT ? OFFSET ?");
    let mut lq = sqlx::query_as::<_, RechargeOrder>(&list_sql);
    if let Some(uid) = user_id {
        lq = lq.bind(uid);
    }
    let rows = lq.bind(limit).bind(offset).fetch_all(pool_ref).await?;
    Ok((rows, total))
}

/// `GET /recharges?page=&page_size=` —— 余额与充值页。
async fn recharges_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    if !identity.is_admin && identity.user_id.is_none() {
        return Ok(redirect_302("/app/login"));
    }

    let (page, page_size) = clamp_page(q_i64(&q, "page", 1), q_i64(&q, "page_size", 20), 100);
    let offset = (page - 1) * page_size;
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let is_admin = identity.is_admin;
    let uid = identity.user_id.unwrap_or(0);

    let snapshot = billing::balance_snapshot(&st.pool, uid).await?;
    // 管理员看全部；客户只能看自己的（走到这里 uid 一定 > 0，前面的守卫已拦掉未登录）
    let (orders, total) =
        list_recharge_orders(&st.pool, if is_admin { None } else { Some(uid) }, page_size, offset)
            .await?;

    let names = if is_admin {
        user_name_map(&st.pool).await?
    } else {
        HashMap::new()
    };
    let rows: Vec<Value> = orders
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "user_id": r.user_id,
                "trade_no": r.trade_no,
                // 模板里写的是 `r.amount / 1000`，所以这里必须传整数厘
                "amount": r.amount,
                "money": r.money,
                "pay_type": r.pay_type,
                "pay_provider": r.pay_provider,
                "status": r.status,
                "remark": r.remark,
                "created_at": crate::util::iso_from_naive(Some(r.created_at)),
                "completed_at": crate::util::iso_from_naive(r.completed_at),
                "username": names.get(&r.user_id).cloned().unwrap_or_default(),
            })
        })
        .collect();

    // 支付通道：易支付的 url/pid/key 三件套齐全才算「可用」
    let channels: Vec<Value> = if !billing_cfg.epay_url.is_empty()
        && !billing_cfg.epay_pid.is_empty()
        && !billing_cfg.epay_key.is_empty()
    {
        vec![
            json!({"name": "alipay", "display_name": "支付宝", "category": "alipay"}),
            json!({"name": "wxpay", "display_name": "微信支付", "category": "wxpay"}),
        ]
    } else {
        Vec::new()
    };

    let mut ctx = customer_ctx(&identity, billing_cfg.is_enabled(), "recharges");
    ctx.insert("snapshot".into(), serde_json::to_value(&snapshot)?);
    ctx.insert("rows".into(), Value::Array(rows));
    ctx.insert("total".into(), json!(total));
    ctx.insert("page".into(), json!(page));
    ctx.insert("page_size".into(), json!(page_size));
    ctx.insert("total_pages".into(), json!(total_pages(total, page_size)));
    ctx.insert("channels".into(), Value::Array(channels));
    ctx.insert("billing_config".into(), serde_json::to_value(&billing_cfg)?);
    ctx.insert("is_admin".into(), json!(is_admin));
    render_html(&t, "recharges.html", &ctx)
}

// ────────────────────────────────────────────────────────────────
//  后台：用户余额 / 充值卡 / 账户资料 / 设置
// ────────────────────────────────────────────────────────────────

/// `GET /billing/users?page=&page_size=` —— 用户余额管理。
async fn billing_users_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
    Query(q): Query<HashMap<String, String>>,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }

    // 这个页面的上限是 200（用户数通常比账号少，一次能看更多）
    let (page, page_size) = clamp_page(q_i64(&q, "page", 1), q_i64(&q, "page_size", 50), 200);
    let (users, total) = billing::list_users(&st.pool, page_size, (page - 1) * page_size).await?;

    let rows: Vec<Value> = users
        .iter()
        .map(|u| {
            json!({
                "id": u.id,
                "username": u.username,
                "role": u.role,
                "status": u.status,
                "email": u.email,
                // 模板里全部是 `r.balance / 1000`，所以传整数厘
                "balance": u.balance,
                "frozen_balance": u.frozen_balance,
                "total_recharged": u.total_recharged,
                "total_charged": u.total_charged,
                "created_at": crate::util::iso_from_naive(Some(u.created_at)),
            })
        })
        .collect();

    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "billing-users");
    ctx.insert("rows".into(), Value::Array(rows));
    ctx.insert("total".into(), json!(total));
    ctx.insert("page".into(), json!(page));
    ctx.insert("page_size".into(), json!(page_size));
    ctx.insert("total_pages".into(), json!(total_pages(total, page_size)));
    render_html(&t, "billing_users.html", &ctx)
}

/// `GET /billing/cards` —— 充值卡管理（前端 JS 调 API 生成/查询）。
async fn billing_cards_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }
    let billing_cfg = settings::get_billing_config(&st.pool).await?;
    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "billing-cards");
    ctx.insert("billing_config".into(), serde_json::to_value(&billing_cfg)?);
    render_html(&t, "billing_cards.html", &ctx)
}

/// `GET /account` —— 账户资料：基础信息 + 全局 Key + 本人名下的 Key。
async fn account_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }

    let api_keys = settings::get_api_keys(&st.pool).await?;
    let uid = identity.user_id;
    let user = match uid {
        Some(id) => billing::get_user(&st.pool, id).await?,
        None => None,
    };
    // 用户名下的 Key（计费 key）；没登录客户就没有
    let user_keys: Vec<Value> = match uid {
        Some(id) => {
            let rows = sqlx::query_as::<_, crate::models::ApiKey>(
                "SELECT * FROM api_keys WHERE user_id = ? ORDER BY id ASC",
            )
            .bind(id)
            .fetch_all(&st.pool)
            .await?;
            rows.iter()
                .map(|k| {
                    json!({
                        "id": k.id,
                        "key": k.key,
                        "name": k.name,
                        "enabled": k.enabled,
                        "created_at": crate::util::iso_from_naive(Some(k.created_at)),
                    })
                })
                .collect()
        }
        None => Vec::new(),
    };

    let snapshot = billing::balance_snapshot(&st.pool, uid.unwrap_or(0)).await?;
    let billing_cfg = settings::get_billing_config(&st.pool).await?;

    let user_json = match &user {
        Some(u) => json!({
            "id": u.id,
            "username": u.username,
            "email": u.email,
            "status": u.status,
            "role": u.role,
            "balance": u.balance,
            "frozen_balance": u.frozen_balance,
            // 模板里 `user.total_recharged / 1000`
            "total_recharged": u.total_recharged,
            "total_charged": u.total_charged,
            "created_at": crate::util::iso_from_naive(Some(u.created_at)),
        }),
        None => Value::Null,
    };

    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "account");
    ctx.insert("user".into(), user_json);
    ctx.insert("snapshot".into(), serde_json::to_value(&snapshot)?);
    ctx.insert("api_key".into(), json!(api_keys.first().cloned().unwrap_or_default()));
    ctx.insert("api_keys".into(), json!(api_keys));
    ctx.insert("user_keys".into(), Value::Array(user_keys));
    render_html(&t, "account.html", &ctx)
}

/// `GET /settings` —— 设置：provider 参数表单 + API Key + 三个定时任务配置。
async fn settings_page(
    State(st): State<Arc<AppState>>,
    Extension(t): Extension<Arc<Templates>>,
    identity: Identity,
) -> AppResult<Response> {
    if !identity.is_admin {
        return Ok(redirect_302("/login"));
    }

    // provider 元数据 + 已保存的配置。
    // 关键：config 里要**把每个字段都补齐**（缺的填 default）——
    // 模板写的是 `p.config[f.key]|default(f.default)`，minijinja 的 map 没有
    // `.get()`，缺键时靠 `|default` 兜底，但提前展开能让整行行为更可预期。
    let mut providers = providers_meta();
    for p in providers.iter_mut() {
        let id = p["id"].as_str().unwrap_or("").to_string();
        let stored = settings::get_provider_config(&st.pool, &id).await?;
        let mut cfg = Map::new();
        if let Some(fields) = p["config_fields"].as_array() {
            for f in fields {
                let key = f["key"].as_str().unwrap_or("");
                let default = f["default"].as_str().unwrap_or("");
                let value = stored
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| json!(default));
                cfg.insert(key.to_string(), value);
            }
        }
        // 保留数据库里有、但字段声明里没有的键（避免保存时丢配置）
        for (k, v) in stored.iter() {
            cfg.entry(k.clone()).or_insert_with(|| v.clone());
        }
        p["config"] = Value::Object(cfg);
    }

    let api_keys = settings::get_api_keys(&st.pool).await?;
    let heartbeat_cfg = settings::get_heartbeat_config(&st.pool).await?;
    let heartbeat_last = settings::get_last_run(&st.pool, "heartbeat_last_summary").await?;
    let health_check_cfg = settings::get_health_check_config(&st.pool).await?;
    let health_check_last = settings::get_last_run(&st.pool, "health_check_last_summary").await?;
    let refresh_cfg = settings::get_refresh_config(&st.pool).await?;
    let refresh_last = settings::get_last_run(&st.pool, "refresh_last_summary").await?;
    let billing_cfg = settings::get_billing_config(&st.pool).await?;

    let mut ctx = base_ctx(&identity, billing_cfg.is_enabled(), "settings");
    ctx.insert("api_keys".into(), json!(api_keys));
    ctx.insert("providers".into(), Value::Array(providers));
    ctx.insert("health_check_cfg".into(), serde_json::to_value(&health_check_cfg)?);
    ctx.insert("health_check_last".into(), health_check_last);
    ctx.insert("heartbeat_cfg".into(), serde_json::to_value(&heartbeat_cfg)?);
    ctx.insert("heartbeat_last".into(), heartbeat_last);
    ctx.insert("refresh_cfg".into(), serde_json::to_value(&refresh_cfg)?);
    ctx.insert("refresh_last".into(), refresh_last);
    render_html(&t, "settings.html", &ctx)
}

// ────────────────────────────────────────────────────────────────
//  开发文档（公开，不走 base.html）
// ────────────────────────────────────────────────────────────────

/// 文档页统一渲染：只传 `doc_active` / `doc_title` 两个变量（对齐 Python `_doc_render`）。
fn render_doc(
    t: &Templates,
    template: &str,
    doc_active: &str,
    doc_title: &str,
) -> AppResult<Response> {
    let mut ctx = Map::new();
    ctx.insert("doc_active".into(), json!(doc_active));
    ctx.insert("doc_title".into(), json!(doc_title));
    render_html(t, template, &ctx)
}

/// `GET /docs` —— 302 到第一页（Python 明确是 302，不是 303）。
async fn docs_index() -> Response {
    redirect_302("/docs/quick-start")
}

async fn docs_quick_start(Extension(t): Extension<Arc<Templates>>) -> AppResult<Response> {
    render_doc(&t, "docs/quick-start.html", "quick-start", "快速开始")
}

async fn docs_temporary_email(Extension(t): Extension<Arc<Templates>>) -> AppResult<Response> {
    render_doc(&t, "docs/temporary-email.html", "temporary", "临时邮箱")
}

async fn docs_api_overview(Extension(t): Extension<Arc<Templates>>) -> AppResult<Response> {
    render_doc(&t, "docs/api-overview.html", "api", "API 文档")
}

async fn docs_errors(Extension(t): Extension<Arc<Templates>>) -> AppResult<Response> {
    render_doc(&t, "docs/errors.html", "errors", "错误码")
}

// ────────────────────────────────────────────────────────────────
//  路由表
// ────────────────────────────────────────────────────────────────

/// 构造页面子路由。
///
/// 返回 `Router<Arc<AppState>>`（不带 state），由 `main.rs` 统一 `.with_state`。
/// 模板用 `Extension` 挂在**子路由层**：这样 pages 模块不需要往 AppState 里加字段，
/// 也不会影响 API 路由。
pub fn router(templates: Arc<Templates>) -> axum::Router<Arc<AppState>> {
    use axum::routing::get;

    axum::Router::new()
        // ── 登录 ──
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", get(logout))
        // ── 客户中心（/app/*）：与管理后台完全隔离 ──
        //
        // 为什么另起一段路径而不是复用 /buy-email：
        // 客户在地址栏看到 /app/buy-email 才知道自己在"客户中心"，
        // 而不是误入管理后台的某个页面。两套布局、两套导航、两套退出。
        .route("/app", get(|| async { redirect_302("/app/buy-email") }))
        .route("/app/login", get(user_login_page).post(user_login_submit))
        .route("/app/register", get(app_register_page))
        .route("/app/exclusive", get(app_exclusive_page))
        .route("/app/logout", get(user_logout))
        .route("/app/buy-email", get(buy_email_page))
        .route("/app/orders", get(orders_page))
        .route("/app/recharges", get(recharges_page))
        .route("/app/balances", get(balances_page))
        .route("/app/mailbox", get(app_mailbox_page))
        // 旧的客户路径保留为 301 跳到新路径，避免老书签失效
        .route("/user/login", get(user_login_page).post(user_login_submit))
        .route("/user/logout", get(user_logout))
        .route("/buy-email", get(legacy_customer_redirect))
        .route("/orders", get(legacy_customer_redirect))
        .route("/recharges", get(legacy_customer_redirect))
        .route("/balances", get(legacy_customer_redirect))
        // ── 管理页 ──
        .route("/", get(dashboard))
        .route("/overview", get(overview_page))
        .route("/accounts", get(accounts_page))
        .route("/mailbox", get(mailbox_page))
        .route("/projects", get(projects_page))
        .route("/pool", get(pool_page))
        // ── 计费后台 ──
        .route("/billing/users", get(billing_users_page))
        .route("/billing/cards", get(billing_cards_page))
        .route("/account", get(account_page))
        .route("/settings", get(settings_page))
        // ── 文档（公开）──
        .route("/docs", get(docs_index))
        .route("/docs/quick-start", get(docs_quick_start))
        .route("/docs/temporary-email", get(docs_temporary_email))
        .route("/docs/api-overview", get(docs_api_overview))
        .route("/docs/errors", get(docs_errors))
        .layer(Extension(templates))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_is_at_least_one_page() {
        assert_eq!(total_pages(0, 20), 1);
        assert_eq!(total_pages(1, 20), 1);
        assert_eq!(total_pages(21, 20), 2);
        assert_eq!(total_pages(100, 20), 5);
    }

    #[test]
    fn page_clamped() {
        // page < 1 归 1；page_size 超上限截断
        assert_eq!(clamp_page(0, 999, 100), (1, 100));
        assert_eq!(clamp_page(-5, 0, 100), (1, 1));
        assert_eq!(clamp_page(3, 20, 100), (3, 20));
    }

    #[test]
    fn query_defaults_on_garbage() {
        let mut q = HashMap::new();
        q.insert("page".to_string(), "abc".to_string());
        assert_eq!(q_i64(&q, "page", 1), 1);
        assert_eq!(q_i64(&q, "missing", 7), 7);
        assert_eq!(q_str(&q, "missing"), "");
    }
}
